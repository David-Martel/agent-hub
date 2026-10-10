"""Keep Wine guest custody separate from an owned Unix launcher."""

from __future__ import annotations

import argparse
import csv
import io
import json
import os
import re
import shutil
import stat
import subprocess
import sys
import tempfile
import time
import hashlib
import uuid
from pathlib import Path

# Retain original client handles when EOF/exit cannot be established within the
# shared settlement budget. Never close a pipe under a blocked reader thread.
UNSETTLED_CLIENTS = []


class WineCommandFailure(RuntimeError):
    def __init__(self, metadata):
        super().__init__("Owned finite Wine client failed; prefix retained")
        self.metadata = metadata


def bounded_client(args, env, timeout):
    """Bound finite clients and settlement without killing a process tree."""
    started = time.monotonic()
    try:
        process = subprocess.Popen(
            args, env=env, stdout=subprocess.PIPE, stderr=subprocess.PIPE
        )
    except OSError:
        raise WineCommandFailure({"reason": "launch-failed"}) from None
    timed_out = False
    killed = False
    settled = True
    stdout = stderr = b""
    try:
        stdout, stderr = process.communicate(
            timeout=max(0.001, timeout - (time.monotonic() - started))
        )
    except subprocess.TimeoutExpired as first:
        timed_out = True
        stdout, stderr = first.stdout or b"", first.stderr or b""
        settlement_deadline = time.monotonic() + 2
        if process.poll() is None:
            # Popen owns the unreaped original child. No process group/tree kill.
            try:
                process.kill()
                killed = True
            except ProcessLookupError:
                pass  # Original child raced to exit; no PID retry/search.
            except OSError:
                # Retain original handle and visible failure, no wider kill.
                UNSETTLED_CLIENTS.append(process)
                raise WineCommandFailure(
                    {
                        "reason": "direct-child-kill-failed",
                        "pid": process.pid,
                        "exit": process.poll(),
                        "pipes_and_child_settled": False,
                    }
                ) from None
        try:
            stdout, stderr = process.communicate(
                timeout=max(0.001, settlement_deadline - time.monotonic())
            )
        except subprocess.TimeoutExpired as last:
            stdout, stderr = last.stdout or stdout, last.stderr or stderr
            settled = False
            UNSETTLED_CLIENTS.append(process)
    code = process.poll()
    elapsed = time.monotonic() - started
    if elapsed >= timeout:
        timed_out = True  # Fully settled success after its deadline still fails.
    if timed_out or code != 0 or not settled:
        raise WineCommandFailure(
            {
                "reason": "deadline" if timed_out else "nonzero-exit",
                "pid": process.pid,
                "exit": code,
                "direct_child_killed": killed,
                "pipes_and_child_settled": settled,
                "elapsed_seconds": elapsed,
                "stdout_bytes": len(stdout),
                "stderr_bytes": len(stderr),
                "stdout_sha256": hashlib.sha256(stdout).hexdigest(),
                "stderr_sha256": hashlib.sha256(stderr).hexdigest(),
            }
        )
    return stdout.decode("utf-8", errors="strict")


def prove_runner_namespace(cid, command, read_local):
    """Compare the selected daemon's exact runner to this kernel and network."""
    if not re.fullmatch(r"[a-f0-9]{64}", cid):
        raise RuntimeError("A canonical runner container ID is required")
    for path, pattern in (
        (
            "/proc/sys/kernel/random/boot_id",
            r"[a-f0-9]{8}(?:-[a-f0-9]{4}){3}-[a-f0-9]{12}",
        ),
        ("/proc/self/ns/net", r"net:\[[0-9]+\]"),
    ):
        tool = "readlink" if path.endswith("ns/net") else "cat"
        actual = command(["docker", "exec", cid, tool, path]).strip()
        expected = read_local(path).strip()
        if not re.fullmatch(pattern, actual) or actual != expected:
            raise RuntimeError("Runner kernel/network namespace is not qualified")


def process_rows(text):
    """Parse explicit Windows tasklist CSV without accepting partial output."""
    result = {}
    rows = list(csv.reader(io.StringIO(text)))
    if not rows or len(rows) > 4096:
        raise RuntimeError("Windows process query returned no bounded inventory")
    for row in rows:
        if len(row) != 5 or not re.fullmatch(r"[0-9]+", row[1]):
            raise RuntimeError("Windows process query is unsupported or malformed")
        pid = int(row[1])
        if pid in result:
            raise RuntimeError("Windows process query returned duplicate guest IDs")
        result[pid] = row[0]
    return result


class WineGuest:
    """Require independent guest readback and complete owned-prefix settlement."""

    def __init__(self, provider, launcher, image, run_id):
        self.provider = provider
        self.launcher = launcher
        self.image = image
        self.run_id = run_id
        self.guest_pid = None
        self.ready = False

    def corroborate(self, health):
        maintenance = health.get("maintenance", {})
        pid = maintenance.get("pid")
        if (
            type(pid) is not int
            or pid <= 0
            or maintenance.get("service_agent_id") != "agent-bus-test-" + self.run_id
            or self.launcher.poll() is not None
            or self.provider.processes().get(pid) != self.image
        ):
            raise RuntimeError("Wine guest identity does not belong to this run")
        if self.guest_pid is not None and self.guest_pid != pid:
            raise RuntimeError("Wine guest identity changed during readiness")
        self.guest_pid = pid

    def close(
        self,
        stop,
        listener_closed,
        *,
        timeout=10,
        clock=time.monotonic,
        pause=time.sleep,
    ):
        # A launcher that already exited must never skip guest/prefix settlement.
        self.provider.assert_owner()
        deadline = clock() + timeout
        if self.ready:
            stop()
        elif self.launcher.poll() is None:
            # Only the original direct child; no PID search or prefix/global kill.
            self.launcher.terminate()
        while True:
            remaining = deadline - clock()
            if remaining <= 0:
                raise RuntimeError(
                    "Owned Wine guest/launcher/listener did not settle; prefix retained"
                )
            guests = self.provider.processes(timeout=remaining)
            guest_absent = self.guest_pid is None or self.guest_pid not in guests
            if guest_absent and listener_closed() and self.launcher.poll() is not None:
                break
            if clock() >= deadline:
                raise RuntimeError(
                    "Owned Wine guest/launcher/listener did not settle; prefix retained"
                )
            pause(0.1)
        # Prefix-scoped wait only; never wineserver -k, pkill, or another prefix.
        remaining = deadline - clock()
        if remaining <= 0:
            raise RuntimeError(
                "Owned Wine prefix settlement exceeded its deadline; prefix retained"
            )
        self.provider.wait_prefix(remaining)
        self.provider.assert_owner()
        self.provider.retire_prefix()


class NativeWineProvider:
    """Fail closed when this runner cannot supply the guest query contract."""

    def __init__(self, directory, env):
        if sys.platform != "linux":
            raise RuntimeError(
                "Wine smoke requires an explicitly qualified Linux runner"
            )
        self.directory = Path(directory).resolve(strict=True)
        self.home = self.directory / "wine-home"
        self.home.mkdir(mode=0o700)
        current_home = self.home.lstat()
        self.home_identity = (
            current_home.st_dev,
            current_home.st_ino,
            current_home.st_uid,
        )
        # Outside the harness TemporaryDirectory: failures retain real guest bytes.
        self.prefix = Path(tempfile.mkdtemp(prefix="agent-bus-owned-wine-"))
        current = self.prefix.lstat()
        self.identity = (current.st_dev, current.st_ino, current.st_uid)
        self.env = self.private_environment(env)
        self.assert_owner()
        # Record custody before initialization, including failures before assignment.
        receipt = self.directory / "wine-prefix-custody.json"
        descriptor = os.open(receipt, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
        with os.fdopen(descriptor, "w", encoding="utf-8") as stream:
            json.dump(
                {
                    "prefix": str(self.prefix),
                    "identity": self.identity,
                    "home": str(self.home),
                    "home_identity": self.home_identity,
                },
                stream,
            )
        print("Owned Wine prefix: " + str(self.prefix), flush=True)
        self.command(["wineboot", "--init"], timeout=120)

    def assert_owner(self):
        current = self.prefix.lstat()
        if (
            not stat.S_ISDIR(current.st_mode)
            or current.st_uid != os.getuid()
            or stat.S_IMODE(current.st_mode) != 0o700
            or (current.st_dev, current.st_ino, current.st_uid) != self.identity
        ):
            raise RuntimeError("Exclusive Wine prefix ownership changed")

    def assert_home(self):
        current = self.home.lstat()
        if (
            not stat.S_ISDIR(current.st_mode)
            or stat.S_IMODE(current.st_mode) != 0o700
            or current.st_uid != os.getuid()
            or (current.st_dev, current.st_ino, current.st_uid) != self.home_identity
            or self.home.parent != self.directory
        ):
            raise RuntimeError("Private Wine HOME ownership changed")

    def private_environment(self, source):
        # Only tool/runtime essentials and this harness's explicit disposable
        # settings. Never inherit host credentials, loader or Wine path overrides.
        essential = {
            "PATH",
            "LANG",
            "LC_ALL",
            "TZ",
            "DISPLAY",
            "TERM",
            "TEMP",
            "TMP",
        }
        result = {
            key: value
            for key, value in source.items()
            if key.upper() in essential or key.upper().startswith("AGENT_BUS_")
        }
        result.update(
            HOME=str(self.home),
            WINEPREFIX=str(self.prefix),
            WINEDEBUG="-all",
            XDG_CONFIG_HOME=str(self.home / ".config"),
            XDG_CACHE_HOME=str(self.home / ".cache"),
            XDG_DATA_HOME=str(self.home / ".local/share"),
        )
        # Ubuntu Wine 9's server_tmpdir frees the borrowed TMPDIR environment
        # pointer. Leave it unset: Wine creates its own mode-0700 random server
        # directory while this run's fixed prefix, HOME and guest TEMP/TMP remain
        # private. Passing even our owned TMPDIR aborts before wineboot starts.
        return result

    def command(self, args, *, env=None, timeout=10):
        self.assert_owner()
        self.assert_home()
        try:
            result = bounded_client(args, env or self.env, timeout)
        except WineCommandFailure as failure:
            # Metadata only: arguments/environment and raw diagnostic streams can
            # contain disposable credentials. Preserve their sizes/hashes privately.
            self.assert_home()
            receipt = self.home / (
                "finite-client-failure-" + uuid.uuid4().hex + ".json"
            )
            descriptor = os.open(receipt, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
            with os.fdopen(descriptor, "w", encoding="utf-8") as stream:
                json.dump(failure.metadata, stream)
            raise
        self.assert_owner()
        self.assert_home()
        return result

    def processes(self, timeout=10):
        return process_rows(
            self.command(["wine", "tasklist", "/fo", "csv", "/nh"], timeout=timeout)
        )

    def wait_prefix(self, timeout):
        self.command(["wineserver", "-w"], timeout=timeout)

    def retire_prefix(self):
        self.assert_owner()
        shutil.rmtree(self.prefix)

    def guest_environment(self, env):
        result = self.private_environment(env)
        # HOME stays POSIX; TMPDIR stays absent for the Wine server workaround.
        # Windows guest-facing configuration/TEMP/TMP need drive mappings.
        for key in ("AGENT_BUS_CONFIG", "TEMP", "TMP"):
            path = Path(result[key]).resolve(strict=True)
            if not path.is_relative_to(self.directory):
                raise RuntimeError("Guest path escapes the owned harness directory")
            converted = self.command(["winepath", "-w", str(path)]).strip()
            if not re.fullmatch(r"[A-Za-z]:\\[^\r\n\x00]+", converted):
                raise RuntimeError("Wine path conversion is unsupported")
            # Round-trip actual guest mapping; no lexical-only trust.
            reverse = self.command(["winepath", "-u", converted]).strip()
            if Path(reverse).resolve(strict=True) != path:
                raise RuntimeError("Wine path round-trip does not match owned input")
            result[key] = converted
        config = Path(env["AGENT_BUS_CONFIG"]).read_text(encoding="utf-8")
        # subprocess argv, not shell command interpolation; config path is one argument.
        observed = self.command(
            ["wine", "cmd", "/c", "type", result["AGENT_BUS_CONFIG"]], env=result
        )
        if config != "{}" or observed.strip() != config:
            raise RuntimeError(
                "Windows guest did not read the private empty configuration"
            )
        return result

    def wrapper(self, artifact):
        artifact = Path(artifact).resolve(strict=True)
        output = self.directory / (artifact.stem + "-wine")
        body = (
            "#!/usr/bin/env python3\nimport os, sys\n"
            "sys.path.insert(0, " + repr(str(Path(__file__).parent)) + ")\n"
            "from wine_lifecycle import run_guest\n"
            "raise SystemExit(run_guest("
            + repr(str(self.prefix))
            + ", "
            + repr(str(self.directory))
            + ", "
            + repr(str(artifact))
            + ", sys.argv[1:], "
            + repr(self.identity)
            + ", "
            + repr(self.home_identity)
            + "))\n"
        )
        with output.open("x", encoding="utf-8") as stream:
            stream.write(body)
        output.chmod(0o700)
        return output


def run_guest(prefix, directory, artifact, argv, identity, home_identity):
    """Map each CLI invocation's fresh environment without changing its parent."""
    provider = NativeWineProvider.__new__(NativeWineProvider)
    provider.prefix = Path(prefix)
    provider.identity = tuple(identity)
    provider.directory = Path(directory).resolve(strict=True)
    provider.home = provider.directory / "wine-home"
    provider.home_identity = tuple(home_identity)
    provider.env = provider.private_environment(os.environ)
    provider.assert_owner()
    provider.assert_home()
    env = provider.guest_environment(os.environ)
    # Current maintained smoke commands have no file-valued arguments.
    # Refuse future path-bearing flags instead of silently passing Unix paths.
    if any(value in {"--config", "--output", "--token-file"} for value in argv):
        raise RuntimeError(
            "Wine adapter requires an explicit mapping for file arguments"
        )
    # This is the deliberately long-lived HTTP/CLI guest launcher supervised by
    # the outer smoke harness. It is distinct from finite query/init/wait clients.
    process = subprocess.Popen(
        ["wine", str(Path(artifact).resolve(strict=True)), *argv], env=env
    )
    result = process.wait()
    provider.assert_owner()
    provider.assert_home()
    return result


def query_prefix(prefix, identity, directory, wait=False, timeout=10):
    """Read guest inventory with a caller-bound immutable prefix identity."""
    provider = NativeWineProvider.__new__(NativeWineProvider)
    provider.prefix = Path(prefix)
    provider.identity = tuple(identity)
    provider.directory = Path(directory).resolve(strict=True)
    receipt = provider.directory / "wine-prefix-custody.json"
    info = receipt.lstat()
    if (
        not stat.S_ISREG(info.st_mode)
        or info.st_uid != os.getuid()
        or stat.S_IMODE(info.st_mode) != 0o600
    ):
        raise RuntimeError("Private Wine custody receipt ownership changed")
    custody = json.loads(receipt.read_text(encoding="utf-8"))
    provider.home = provider.directory / "wine-home"
    provider.home_identity = tuple(custody["home_identity"])
    if (
        custody["prefix"] != prefix
        or tuple(custody["identity"]) != provider.identity
        or custody["home"] != str(provider.home)
    ):
        raise RuntimeError("Fixed Wine custody receipt does not match")
    provider.assert_home()
    provider.env = provider.private_environment(os.environ)
    if wait:
        provider.wait_prefix(timeout)
        provider.assert_owner()
        return {"settled": True}
    return provider.processes(timeout=timeout)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("command", choices=("query", "wait"))
    parser.add_argument("--prefix", required=True)
    parser.add_argument("--identity", required=True)
    parser.add_argument("--directory", required=True)
    parser.add_argument("--timeout", type=float, default=10)
    arguments = parser.parse_args()
    try:
        identity = json.loads(arguments.identity)
        if (
            not isinstance(identity, list)
            or len(identity) != 3
            or any(type(value) is not int or value < 0 for value in identity)
        ):
            raise RuntimeError("Invalid fixed prefix identity")
        if not 0 < arguments.timeout <= 10:
            raise RuntimeError("Invalid bounded prefix query deadline")
        print(
            json.dumps(
                query_prefix(
                    arguments.prefix,
                    identity,
                    arguments.directory,
                    arguments.command == "wait",
                    arguments.timeout,
                )
            )
        )
    except (RuntimeError, OSError, ValueError, KeyError, TypeError):
        raise SystemExit("Owned Wine guest query failed; prefix retained") from None
