#!/usr/bin/env python3
"""Run service tests against disposable local Docker containers, never the shared bus.

Requires Python 3.10+, Docker with Linux containers, Cargo, and (for smoke) pwsh.
No resource is found/deleted by name: cleanup uses captured container IDs and the
HTTP process handle created by this invocation. Missing prerequisites are errors.
"""

from __future__ import annotations

import argparse
from contextlib import contextmanager
import json
import os
import re
import secrets
import shutil
import signal
import socket
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.parse
import urllib.request
from pathlib import Path

from wine_lifecycle import NativeWineProvider, WineGuest, prove_runner_namespace

ROOT = Path(__file__).resolve().parents[2]
RESERVED_PORTS = {6379, 6380, 5432, 5300, 8400, 8401, 18400}


def run(args, *, env=None, capture=False, timeout=180):
    # Never echo the arguments: they can contain this run's disposable password.
    executable = shutil.which(args[0])
    if executable is None:
        raise RuntimeError(f"Required tool is missing: {args[0]}")
    return subprocess.run(
        [executable, *args[1:]],
        cwd=ROOT,
        env=env,
        check=True,
        text=True,
        stdout=subprocess.PIPE if capture else None,
        stderr=subprocess.PIPE if capture else None,
        timeout=timeout,
        creationflags=subprocess.CREATE_NO_WINDOW if os.name == "nt" else 0,
    )


def clean_environment(source):
    env = {
        key: value
        for key, value in source.items()
        if not key.upper().startswith("AGENT_BUS_")
        and key.upper() not in {"HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY", "NO_PROXY"}
    }
    env["NO_PROXY"] = "localhost,127.0.0.1,::1"
    if "AGENT_BUS_BUILD_REVISION" in source:
        env["AGENT_BUS_BUILD_REVISION"] = source["AGENT_BUS_BUILD_REVISION"]
    return env


def require_local_docker():
    if not shutil.which("docker"):
        raise RuntimeError("Docker is required; no shared-service fallback exists")
    # Docker's explicit context takes precedence over DOCKER_HOST.
    endpoint = (
        None if os.environ.get("DOCKER_CONTEXT") else os.environ.get("DOCKER_HOST")
    )
    if not endpoint:
        context = json.loads(run(["docker", "context", "inspect"], capture=True).stdout)
        endpoint = context[0]["Endpoints"]["docker"]["Host"]
    parsed = urllib.parse.urlsplit(endpoint)
    if parsed.scheme not in {"unix", "npipe"} and not (
        parsed.scheme == "tcp" and parsed.hostname in {"localhost", "127.0.0.1", "::1"}
    ):
        raise RuntimeError("A local Docker endpoint is required")
    if (
        run(["docker", "info", "--format", "{{.OSType}}"], capture=True).stdout.strip()
        != "linux"
    ):
        raise RuntimeError("Docker must provide Linux containers")


def unused_port():
    with socket.socket() as reservation:
        reservation.bind(("localhost", 0))
        port = reservation.getsockname()[1]
    if port in RESERVED_PORTS:
        raise RuntimeError("OS selected a reserved service port; retry the harness")
    return port


def endpoint_identity(value):
    parsed = urllib.parse.urlsplit(value)
    return parsed.scheme, parsed.hostname, parsed.port, parsed.path


class NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, req, fp, code, msg, headers, newurl):
        raise RuntimeError("Refusing a redirected disposable-service response")


def read_json(url, token=None):
    headers = {"Authorization": f"Bearer {token}"} if token else {}
    opener = urllib.request.build_opener(urllib.request.ProxyHandler({}), NoRedirect())
    with opener.open(
        urllib.request.Request(url, headers=headers), timeout=3
    ) as response:
        return json.load(response)


class Services:
    def __init__(self, directory, mode, network_container=None, wine=False):
        self.directory = Path(directory)
        self.mode = mode
        self.network_container = network_container
        self.run_id = secrets.token_hex(16)
        self.password = secrets.token_hex(24)
        self.token = secrets.token_hex(24)
        self.containers = []
        self.cid_files = []
        self.http = None
        self.http_log = None
        self.wine = wine
        self.wine_provider = None
        self.wine_guest = None
        self.env = clean_environment(os.environ)
        self.env.update(
            {name: str(self.directory) for name in ("TMPDIR", "TEMP", "TMP")}
        )

    def create(self, image, port, extra):
        cid_file = self.directory / f"container-{len(self.cid_files)}.cid"
        self.cid_files.append(cid_file)
        if self.network_container:
            host_port = unused_port()
            network = ["--network", f"container:{self.network_container}"]
            if image.startswith("redis:"):
                service_args = [
                    "redis-server",
                    "--port",
                    str(host_port),
                    "--bind",
                    "127.0.0.1",
                ]
            else:
                extra = [*extra, "--env", f"PGPORT={host_port}"]
                service_args = ["-c", "listen_addresses=127.0.0.1"]
        else:
            network = ["--publish", f"127.0.0.1::{port}"]
            service_args = []
        limits = (
            ["--memory", "128m", "--cpus", "0.5"]
            if image.startswith("redis:")
            else ["--memory", "512m", "--cpus", "1"]
        )
        run(
            [
                "docker",
                "create",
                "--cidfile",
                str(cid_file),
                "--label",
                f"agent-bus.test-run={self.run_id}",
                *limits,
                *network,
                *extra,
                image,
                *service_args,
            ],
            capture=True,
        )
        cid = cid_file.read_text().strip()
        if not re.fullmatch(r"[a-f0-9]{64}", cid):
            raise RuntimeError("Docker returned an invalid container identity")
        self.containers.append(cid)
        run(["docker", "start", cid], capture=True)
        if self.network_container:
            return cid, host_port
        ports = json.loads(
            run(
                [
                    "docker",
                    "inspect",
                    "--format",
                    "{{json .NetworkSettings.Ports}}",
                    cid,
                ],
                capture=True,
            ).stdout
        )
        bindings = ports[f"{port}/tcp"]
        if len(bindings) != 1 or bindings[0]["HostIp"] != "127.0.0.1":
            raise RuntimeError(
                "Disposable service is not bound exclusively to loopback"
            )
        host_port = int(bindings[0]["HostPort"])
        if host_port in RESERVED_PORTS or not 1 <= host_port <= 65535:
            raise RuntimeError("Docker selected an unsafe service port")
        return cid, host_port

    def wait_container(self, cid, command):
        deadline = time.monotonic() + 60
        while time.monotonic() < deadline:
            try:
                run(["docker", "exec", cid, *command], capture=True, timeout=5)
                return
            except subprocess.CalledProcessError:
                time.sleep(0.2)
        raise RuntimeError("Disposable backend did not become ready within 60 seconds")

    def start(self):
        prefix = "agent_bus_history_" if self.mode == "history" else "agent_bus_test_"
        database = prefix + self.run_id
        pg, pg_port = self.create(
            "postgres:16" if self.network_container else "postgres:17-alpine",
            5432,
            [
                "--mount",
                "type=tmpfs,destination=/var/lib/postgresql/data",
                "--env",
                f"POSTGRES_PASSWORD={self.password}",
                "--env",
                f"POSTGRES_DB={database}",
            ],
        )
        pg_service_port = pg_port if self.network_container else 5432
        self.wait_container(
            pg,
            [
                "pg_isready",
                "-h",
                "localhost",
                "-p",
                str(pg_service_port),
                "-U",
                "postgres",
                "-d",
                database,
            ],
        )
        database_url = (
            f"postgresql://postgres:{self.password}@localhost:{pg_port}/{database}"
        )
        config = self.directory / "config.json"
        config.write_text("{}", encoding="utf-8")
        self.env.update(
            {
                "AGENT_BUS_CONFIG": str(config),
                "AGENT_BUS_DATABASE_URL": database_url,
                "AGENT_BUS_TEST_DATABASE_URL": database_url,
                "AGENT_BUS_TEST_RUN_ID": self.run_id,
                "AGENT_BUS_AUTH_TOKEN": self.token,
                "AGENT_BUS_TEST_AUTH_TOKEN": self.token,
                "AGENT_BUS_SERVER_HOST": "localhost",
                "AGENT_BUS_STARTUP_ENABLED": "false",
                "AGENT_BUS_SERVICE_AGENT_ID": f"agent-bus-test-{self.run_id}",
            }
        )
        if self.mode != "history":
            redis, redis_port = self.create("redis:7-alpine", 6379, [])
            redis_service_port = redis_port if self.network_container else 6379
            self.wait_container(
                redis, ["redis-cli", "-p", str(redis_service_port), "ping"]
            )
            redis_url = f"redis://localhost:{redis_port}/0"
            self.env.update(
                {
                    "AGENT_BUS_REDIS_URL": redis_url,
                    "AGENT_BUS_TEST_REDIS_URL": redis_url,
                }
            )

    def start_http(self, binary):
        port = unused_port()
        url = f"http://localhost:{port}"
        self.env["AGENT_BUS_TEST_SERVER_URL"] = url
        self.http_log = (self.directory / "http.log").open("w", encoding="utf-8")
        self.http = subprocess.Popen(
            [str(binary), "--port", str(port)],
            cwd=ROOT,
            env=self.env,
            stdout=self.http_log,
            stderr=subprocess.STDOUT,
            creationflags=subprocess.CREATE_NO_WINDOW if os.name == "nt" else 0,
        )
        if self.wine:
            self.wine_guest = WineGuest(
                self.wine_provider, self.http, "agent-bus-http.exe", self.run_id
            )
        deadline = time.monotonic() + 60
        while time.monotonic() < deadline:
            if self.http.poll() is not None:
                raise RuntimeError("Owned HTTP process exited before readiness")
            try:
                health = read_json(url + "/health")
            except (OSError, urllib.error.URLError):
                time.sleep(0.2)
                continue
            maintenance = health.get("maintenance", {})
            if self.wine:
                self.wine_guest.corroborate(health)
            elif (
                maintenance.get("pid") != self.http.pid
                or maintenance.get("service_agent_id")
                != self.env["AGENT_BUS_SERVICE_AGENT_ID"]
            ):
                raise RuntimeError(
                    "HTTP endpoint does not belong to this harness invocation"
                )
            for field, variable in (
                ("redis_url", "AGENT_BUS_REDIS_URL"),
                ("database_url", "AGENT_BUS_DATABASE_URL"),
            ):
                if endpoint_identity(health.get(field, "")) != endpoint_identity(
                    self.env[variable]
                ):
                    raise RuntimeError(
                        "HTTP backend identity does not match disposable services"
                    )
            if all(
                health.get(flag) is True
                for flag in ("ok", "database_ok", "storage_ready")
            ):
                read_json(url + "/admin/service", self.token)
                if self.wine:
                    # Authenticated backend readiness must precede stopping.
                    final_health = read_json(url + "/health")
                    self.wine_guest.corroborate(final_health)
                    if not all(
                        final_health.get(flag) is True
                        for flag in ("ok", "database_ok", "storage_ready")
                    ) or any(
                        endpoint_identity(final_health.get(field, ""))
                        != endpoint_identity(self.env[variable])
                        for field, variable in (
                            ("redis_url", "AGENT_BUS_REDIS_URL"),
                            ("database_url", "AGENT_BUS_DATABASE_URL"),
                        )
                    ):
                        raise RuntimeError(
                            "Wine readiness/backend identity changed "
                            "after authentication"
                        )
                    self.wine_guest.ready = True
                return
            time.sleep(0.2)
        raise RuntimeError(
            "Owned HTTP service did not become healthy within 60 seconds"
        )

    def close(self):
        errors = []
        try:
            if self.wine_guest is not None:

                def stop():
                    request = urllib.request.Request(
                        self.env["AGENT_BUS_TEST_SERVER_URL"]
                        + "/admin/service/control",
                        data=json.dumps({"action": "stop", "flush": True}).encode(),
                        headers={
                            "Authorization": "Bearer " + self.token,
                            "Content-Type": "application/json",
                        },
                        method="POST",
                    )
                    opener = urllib.request.build_opener(
                        urllib.request.ProxyHandler({}), NoRedirect()
                    )
                    with opener.open(request, timeout=3) as response:
                        if response.status != 200:
                            raise RuntimeError("Owned Wine graceful stop failed")

                def listener_closed():
                    port = urllib.parse.urlsplit(
                        self.env["AGENT_BUS_TEST_SERVER_URL"]
                    ).port
                    try:
                        with socket.create_connection(("localhost", port), timeout=0.2):
                            return False
                    except ConnectionRefusedError:
                        return True

                self.wine_guest.close(stop, listener_closed)
            elif self.wine_provider is not None:
                self.wine_provider.wait_prefix(10)
                self.wine_provider.retire_prefix()
            elif self.http is not None and self.http.poll() is None:
                self.http.terminate()
                try:
                    self.http.wait(timeout=10)
                except subprocess.TimeoutExpired:
                    self.http.kill()
                    self.http.wait(timeout=10)
        except (RuntimeError, subprocess.SubprocessError, OSError) as error:
            # Retain the first cleanup subcause without publishing commands,
            # URLs, credentials or exception payloads from external clients.
            failure = {
                "stage": "owned-wine-close" if self.wine else "owned-http-close",
                "type": type(error).__name__,
                "reason": (
                    str(error)
                    if isinstance(error, RuntimeError)
                    and str(error).startswith("Owned Wine")
                    else "Owned process cleanup failed"
                ),
            }
            if hasattr(error, "metadata"):
                failure["finite_client"] = error.metadata
            print(json.dumps({"cleanup_failure": failure}), file=sys.stderr)
            errors.append(
                "owned Wine guest/prefix" if self.wine else "owned HTTP process"
            )
        if self.http_log is not None:
            self.http_log.close()
        # Recover only cidfiles created in this invocation's private directory,
        # including a create command interrupted after Docker wrote the ID.
        ids = set(self.containers)
        for path in self.cid_files:
            if path.is_file():
                cid = path.read_text().strip()
                if re.fullmatch(r"[a-f0-9]{64}", cid):
                    ids.add(cid)
        for cid in sorted(ids):
            try:
                run(["docker", "rm", "--force", "--volumes", cid], capture=True)
            except (subprocess.SubprocessError, OSError):
                errors.append(cid)
        if errors:
            raise RuntimeError("Failed to clean owned resources: " + ", ".join(errors))


def binary_path(name, env):
    metadata = json.loads(
        run(
            [
                "cargo",
                "metadata",
                "--no-deps",
                "--format-version",
                "1",
            ],
            env=env,
            capture=True,
        ).stdout
    )
    return (
        Path(metadata["target_directory"])
        / "debug"
        / (name + (".exe" if os.name == "nt" else ""))
    )


def execute(mode, services, cli=None, http=None, minimum_backend_tests=73):
    if mode == "history":
        run(
            [
                "cargo",
                "test",
                "-p",
                "agent-bus-core",
                "--test",
                "history_catalog_postgres",
                "--",
                "--test-threads=1",
                "--nocapture",
            ],
            env=services.env,
            timeout=1800,
        )
    elif mode == "integration":
        run(
            ["cargo", "build", "-p", "agent-bus-http", "--bin", "agent-bus-http"],
            env=services.env,
            timeout=1800,
        )
        services.start_http(binary_path("agent-bus-http", services.env))
        try:
            result = run(
                [
                    "cargo",
                    "test",
                    "--workspace",
                    "--tests",
                    "--no-fail-fast",
                    "--",
                    "--ignored",
                    "--test-threads=1",
                ],
                env=services.env,
                capture=True,
                timeout=1800,
            )
        except subprocess.CalledProcessError as failure:
            # Preserve failing test evidence while suppressing disposable credentials.
            for stream in (failure.stdout, failure.stderr):
                if stream:
                    print(
                        stream.replace(services.password, "<redacted>").replace(
                            services.token, "<redacted>"
                        ),
                        end="",
                    )
            raise
        output = result.stdout + result.stderr
        print(
            output.replace(services.password, "<redacted>").replace(
                services.token, "<redacted>"
            ),
            end="",
        )
        passed = sum(
            int(value)
            for value in re.findall(r"(?m)^test result: ok\. (\d+) passed;", output)
        )
        if passed < minimum_backend_tests:
            raise RuntimeError(
                f"Only {passed} backend tests passed; "
                f"required floor is {minimum_backend_tests}"
            )
    else:
        if services.wine:
            services.wine_provider = NativeWineProvider(
                services.directory, services.env
            )
            services.env["AGENT_BUS_TEST_WINE_PREFIX"] = str(
                services.wine_provider.prefix
            )
            services.env["AGENT_BUS_TEST_WINE_IDENTITY"] = json.dumps(
                services.wine_provider.identity
            )
            cli = services.wine_provider.wrapper(cli)
            http = services.wine_provider.wrapper(http)
        # Reuse CLI checks without letting PowerShell own a nested HTTP child.
        run(
            [
                "pwsh",
                "-NoLogo",
                "-NoProfile",
                "-File",
                str(ROOT / "scripts/test-agent-bus-functional.ps1"),
                "-CliPath",
                str(cli),
                "-HttpBinaryPath",
                str(http),
                "-HttpAuthToken",
                services.token,
                "-SkipHttp",
                "-RequirePostgres",
                "-SkipForcedDegraded",
            ],
            env=services.env,
            timeout=600,
        )
        services.start_http(http)
        url = services.env["AGENT_BUS_TEST_SERVER_URL"]
        wine_identity = (
            [
                "-ExpectedWineGuestProcessId",
                str(services.wine_guest.guest_pid),
                "-WinePrefix",
                str(services.wine_provider.prefix),
            ]
            if services.wine
            else []
        )
        run(
            [
                "pwsh",
                "-NoLogo",
                "-NoProfile",
                "-File",
                str(ROOT / "scripts/test-agent-bus-http-smoke.ps1"),
                "-BinaryPath",
                str(http),
                "-BaseUrl",
                url,
                "-Port",
                str(urllib.parse.urlsplit(url).port),
                "-DatabaseMode",
                "Healthy",
                "-AuthToken",
                services.token,
                "-UseExistingServer",
                "-ExpectedProcessId",
                str(services.http.pid),
                "-ExpectedServiceAgentId",
                services.env["AGENT_BUS_SERVICE_AGENT_ID"],
                *wine_identity,
            ],
            env=services.env,
            timeout=600,
        )


@contextmanager
def owned_directory(wine=False):
    if not wine:
        with tempfile.TemporaryDirectory(prefix="agent-bus-isolated-") as directory:
            yield directory
        return
    directory = tempfile.mkdtemp(prefix="agent-bus-isolated-wine-")
    try:
        yield directory
    except BaseException:
        print("Failed Wine harness directory retained: " + directory, flush=True)
        raise
    else:
        shutil.rmtree(directory)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("mode", choices=("integration", "history", "smoke"))
    parser.add_argument("--cli", type=Path)
    parser.add_argument("--http", type=Path)
    parser.add_argument("--wine", action="store_true")
    parser.add_argument(
        "--fixture-network", choices=("runner-container", "native-host")
    )
    parser.add_argument(
        "--network-container",
        help="Explicit Docker runner container whose loopback namespace fixtures share",
    )
    parser.add_argument("--minimum-backend-tests", type=int, default=73)
    args = parser.parse_args()
    if args.wine and (
        args.mode != "smoke"
        or args.fixture_network is None
        or (args.fixture_network == "runner-container" and not args.network_container)
        or (
            args.fixture_network == "runner-container"
            and not re.fullmatch(r"[a-f0-9]{64}", args.network_container or "")
        )
        or (args.fixture_network == "native-host" and args.network_container)
    ):
        raise RuntimeError("Wine smoke requires an explicit qualified fixture network")
    if not args.wine and args.fixture_network:
        raise RuntimeError("Wine fixture-network selection requires --wine")
    if args.minimum_backend_tests < 0:
        raise RuntimeError("Backend test floor must be nonnegative")
    required = ("pwsh",) if args.mode == "smoke" else ("cargo",)
    for tool in required:
        if not shutil.which(tool):
            raise RuntimeError(f"Required tool is missing: {tool}")
    if args.mode == "smoke" and not all(
        path and path.is_file() for path in (args.cli, args.http)
    ):
        raise RuntimeError(
            "Smoke requires explicit existing --cli and --http artifact paths"
        )
    require_local_docker()
    network_container = None
    if args.network_container:
        if not re.fullmatch(r"[a-f0-9]{12,64}", args.network_container):
            raise RuntimeError(
                "Runner network container must be an explicit container ID"
            )
        owner = json.loads(
            run(["docker", "inspect", args.network_container], capture=True).stdout
        )
        if (
            len(owner) != 1
            or not owner[0].get("State", {}).get("Running")
            or not owner[0].get("Id", "").startswith(args.network_container)
        ):
            raise RuntimeError("Runner network container identity is not verified")
        network_container = owner[0]["Id"]
        if args.wine:
            prove_runner_namespace(
                network_container,
                lambda argv: run(argv, capture=True).stdout,
                lambda path: (
                    os.readlink(path)
                    if path.endswith("ns/net")
                    else Path(path).read_text(encoding="ascii")
                ),
            )
    with owned_directory(args.wine) as directory:
        services = Services(directory, args.mode, network_container, args.wine)
        try:
            print(f"Disposable {args.mode} run: {services.run_id}", flush=True)
            services.start()
            execute(
                args.mode,
                services,
                args.cli.resolve() if args.cli else None,
                args.http.resolve() if args.http else None,
                args.minimum_backend_tests,
            )
        except BaseException:
            # The first fixture/test failure remains authoritative even if cleanup
            # independently fails. Never stringify subprocess argv/passwords.
            try:
                services.close()
            except (RuntimeError, OSError, subprocess.SubprocessError) as cleanup_error:
                detail = (
                    str(cleanup_error)
                    if isinstance(cleanup_error, RuntimeError)
                    else type(cleanup_error).__name__
                )
                print(f"Cleanup also failed: {detail}")
            raise
        else:
            services.close()


if __name__ == "__main__":

    def interrupted(signum, frame):
        raise KeyboardInterrupt

    signal.signal(signal.SIGTERM, interrupted)
    try:
        main()
    except (
        RuntimeError,
        OSError,
        subprocess.SubprocessError,
        KeyboardInterrupt,
    ) as exc:
        # CalledProcessError.__str__ contains argv/passwords; print only safe context.
        message = str(exc) if isinstance(exc, RuntimeError) else type(exc).__name__
        raise SystemExit(f"Isolated service run failed: {message}") from None
