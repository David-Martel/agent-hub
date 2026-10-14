"""Launch only a controlled configured MCP fixture in an owned Wine prefix."""

from __future__ import annotations

import argparse
import contextlib
import contextvars
import hashlib
import io
import json
import os
import stat
import struct
import subprocess
import sys
import threading
import time
import traceback
import uuid
from pathlib import Path

from wine_lifecycle import NativeWineProvider

STAGES = contextvars.ContextVar("mcp_wine_stages", default=None)


def static_failure(error):
    """Report code locations without exception messages or configured data."""
    number = getattr(error, "errno", None)
    return {
        "class": type(error).__name__,
        "errno": number if type(number) is int else None,
        "stack": [
            {
                "file": Path(frame.filename).name,
                "function": frame.name,
                "line": frame.lineno,
            }
            for frame in traceback.extract_tb(error.__traceback__)
        ],
    }


def stage(name, operation, *args, **kwargs):
    started = time.monotonic()
    event = {"stage": name, "status": "PASS"}
    try:
        return operation(*args, **kwargs)
    except BaseException as error:
        event.update(status="FAIL", failure=static_failure(error))
        raise
    finally:
        event["seconds"] = round(time.monotonic() - started, 6)
        events = STAGES.get()
        if events is not None:
            events.append(event)


def retain_failure(root, diagnostic):
    """Retain safe metadata exclusively in the revalidated private fixture."""
    root = fixture_root(root)
    path = root / f"mcp-wine-failure-{uuid.uuid4().hex}.json"
    descriptor = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
    with os.fdopen(descriptor, "w", encoding="utf-8") as stream:
        json.dump(diagnostic, stream)
        stream.flush()
        os.fsync(stream.fileno())
    return path.name


def fixture_root(value):
    path = Path(value)
    current = path.lstat()
    if (
        path.is_symlink()
        or not stat.S_ISDIR(current.st_mode)
        or current.st_uid != os.getuid()
        or stat.S_IMODE(current.st_mode) != 0o700
    ):
        raise RuntimeError("Private MCP fixture ownership is not established")
    return path.resolve(strict=True)


def local_path(root, value):
    path = Path(value).resolve(strict=True)
    if not path.is_relative_to(root):
        raise RuntimeError("Configured fixture path escapes its owned root")
    return path


def closed_route(value, scheme):
    from urllib.parse import urlsplit

    route = urlsplit(value)
    if (
        route.scheme != scheme
        or route.hostname != "localhost"
        or route.port != 1
        or route.password
        or route.query
        or route.fragment
    ):
        raise RuntimeError("MCP fixture backend is not closed")
    return value


def host_environment(root):
    return {
        "PATH": os.environ.get("PATH", ""),
        "HOME": str(root),
        "TMPDIR": str(root),
        "TEMP": str(root),
        "TMP": str(root),
        "AGENT_BUS_STARTUP_ENABLED": "false",
        "RUST_LOG": "error",
        "AGENT_BUS_REDIS_URL": "redis://localhost:1/0",
        "AGENT_BUS_DATABASE_URL": "postgresql://postgres@localhost:1/agent_bus_test",
        "AGENT_BUS_SERVER_URL": "http://localhost:1",
    }


def prepare(root):
    # Bootstrap is separate from the unchanged initialize/tools response deadlines.
    env = host_environment(root)
    with contextlib.redirect_stdout(io.StringIO()):
        provider = NativeWineProvider(root, env)
    receipt = {
        "prefix": str(provider.prefix),
        "identity": provider.identity,
        "home": str(provider.home),
        "home_identity": provider.home_identity,
        "root": str(root),
        "profile": "configured-mcp-wine-fixture-v1",
    }
    with (root / "mcp-wine-custody.json").open("x", encoding="utf-8") as stream:
        json.dump(receipt, stream)
    provider.wait_prefix(10)
    return {"prepared": True}


def provider_for(root):
    data = json.loads((root / "mcp-wine-custody.json").read_text(encoding="utf-8"))
    if (
        data.get("root") != str(root)
        or data.get("profile") != "configured-mcp-wine-fixture-v1"
        or not isinstance(data.get("identity"), list)
        or len(data["identity"]) != 3
        or any(type(value) is not int or value < 0 for value in data["identity"])
        or data.get("home") != str(root / "wine-home")
        or not isinstance(data.get("home_identity"), list)
        or len(data["home_identity"]) != 3
        or any(type(value) is not int or value < 0 for value in data["home_identity"])
    ):
        raise RuntimeError("MCP prefix receipt is invalid")
    provider = NativeWineProvider.__new__(NativeWineProvider)
    provider.directory = root
    provider.prefix = Path(data["prefix"])
    provider.identity = tuple(data["identity"])
    provider.home = Path(data["home"])
    provider.home_identity = tuple(data["home_identity"])
    provider.env = provider.private_environment(host_environment(root))
    provider.env.update(WINEPREFIX=str(provider.prefix), WINEDEBUG="-all")
    provider.assert_owner()
    provider.assert_home()
    return provider


def map_paths(provider, paths):
    import re

    # Wine 11 winepath processes every operand in order; -0 preserves boundaries.
    # Batch only these controlled paths, retaining each actual guest round-trip.
    def records(output):
        if not output.endswith("\x00"):
            raise RuntimeError("Wine mapping output is incomplete")
        result = output[:-1].split("\x00")
        if len(result) != len(paths) or any(not value for value in result):
            raise RuntimeError("Wine mapping output cardinality differs")
        return result

    converted = records(
        stage(
            "mapping-forward",
            provider.command,
            ["winepath", "-w", "-0", *map(str, paths)],
        )
    )
    if any(not re.fullmatch(r"[A-Za-z]:\\[^\r\n\x00]+", value) for value in converted):
        raise RuntimeError("Wine mapping is unsupported")
    reverse = records(
        stage("mapping-reverse", provider.command, ["winepath", "-u", "-0", *converted])
    )
    if any(
        "\r" in value or "\n" in value or Path(value).resolve(strict=True) != path
        for path, value in zip(paths, reverse, strict=True)
    ):
        raise RuntimeError("Wine mapping does not round-trip")
    return converted


def map_path(provider, path):
    return map_paths(provider, [path])[0]


def guest_environment(provider, configured):
    allowed = {
        "AGENT_BUS_CONFIG",
        "AGENT_BUS_SERVICE_AGENT_ID",
        "AGENT_BUS_SERVER_HOST",
        "AGENT_BUS_ALLOW_REMOTE",
        "AGENT_BUS_STARTUP_ENABLED",
        "RUST_LOG",
        "AGENT_BUS_REDIS_URL",
        "AGENT_BUS_DATABASE_URL",
        "AGENT_BUS_SERVER_URL",
    }
    if not isinstance(configured, dict) or any(
        key not in allowed or not isinstance(value, str)
        for key, value in configured.items()
    ):
        raise RuntimeError("Configured MCP environment is not a controlled fixture")
    if configured.get("AGENT_BUS_STARTUP_ENABLED", "false") != "false":
        raise RuntimeError("MCP fixture startup must remain disabled")
    result = dict(provider.env)
    result.update(configured)
    for key, scheme in (
        ("AGENT_BUS_REDIS_URL", "redis"),
        ("AGENT_BUS_DATABASE_URL", "postgresql"),
        ("AGENT_BUS_SERVER_URL", "http"),
    ):
        closed_route(result[key], scheme)
    config = configured.get("AGENT_BUS_CONFIG")
    if config:
        path = local_path(provider.directory, config)
    else:
        path = provider.directory / "mcp-wine-empty.json"
        if not path.exists():
            with path.open("x", encoding="utf-8") as stream:
                stream.write("{}")
    content = path.read_text(encoding="utf-8-sig")
    if json.loads(content) not in ({}, {"auth_token": "validator-fixture-token"}):
        raise RuntimeError("Only explicit private fixture configuration is admitted")
    result["AGENT_BUS_CONFIG"], temporary_path = map_paths(
        provider, [path, provider.directory]
    )
    # HOME/TMPDIR are consumed by the Unix Wine launcher. Windows TEMP/TMP and
    # the Rust configuration are guest paths; never feed a drive path to host HOME.
    result["HOME"] = str(provider.home)
    for key in ("TEMP", "TMP"):
        result[key] = temporary_path
    observed = stage(
        "config-readback",
        provider.command,
        ["wine", "cmd", "/c", "type", result["AGENT_BUS_CONFIG"]],
        env=result,
    )
    if observed.lstrip("\ufeff").strip() != content.strip():
        raise RuntimeError("Wine guest configuration readback differs")
    return result


def executable(value):
    path = Path(value).resolve(strict=True)
    if not path.is_file() or path.suffix.lower() != ".exe":
        raise RuntimeError("Configured shipping command must be an actual Windows EXE")
    with path.open("rb") as stream:
        header = stream.read(64)
        if len(header) != 64 or header[:2] != b"MZ":
            raise RuntimeError("Configured command has no PE header")
        stream.seek(struct.unpack_from("<I", header, 60)[0])
        pe = stream.read(6)
    if pe != b"PE\x00\x00\x64\x86":
        raise RuntimeError("Configured command is not Windows X64")
    return path, hashlib.sha256(path.read_bytes()).hexdigest()


def settle(
    provider,
    child,
    image,
    guest_pid,
    *,
    timeout=10,
    clock=time.monotonic,
    pause=time.sleep,
):
    deadline = clock() + timeout
    child.stdin.close()
    while True:
        remaining = deadline - clock()
        if remaining <= 0:
            raise RuntimeError("MCP guest settlement failed; prefix retained")
        guests = provider.processes(timeout=min(remaining, 10))
        if (
            guest_pid not in guests
            and image not in guests.values()
            and child.poll() is not None
        ):
            break
        pause(0.1)
    remaining = deadline - clock()
    if remaining <= 0:
        raise RuntimeError("MCP prefix settlement exceeded its deadline")
    provider.wait_prefix(remaining)
    provider.assert_owner()
    provider.assert_home()


def corroborate(provider, child, image):
    candidates = [pid for pid, name in provider.processes().items() if name == image]
    if len(candidates) != 1 or child.poll() is not None:
        raise RuntimeError("Configured MCP guest identity is not established")
    return candidates[0]


def launch_ready(provider, path, digest, arguments, configured, env, child):
    """Bind completed Wine setup before the client's protocol clock starts."""
    config = (
        local_path(provider.directory, configured["AGENT_BUS_CONFIG"])
        if configured.get("AGENT_BUS_CONFIG")
        else provider.directory / "mcp-wine-empty.json"
    )
    provider.assert_owner()
    provider.assert_home()
    if hashlib.sha256(path.read_bytes()).hexdigest() != digest:
        raise RuntimeError("Configured MCP executable changed before readiness")
    config_digest = hashlib.sha256(config.read_bytes()).hexdigest()
    receipt = {
        "profile": "configured-mcp-wine-launch-ready-v1",
        "root": str(provider.directory),
        "command": str(path),
        "command_sha256": digest,
        "arguments_sha256": hashlib.sha256(
            json.dumps(arguments, sort_keys=True).encode()
        ).hexdigest(),
        "environment_sha256": hashlib.sha256(
            json.dumps(env, sort_keys=True).encode()
        ).hexdigest(),
        "config": str(config),
        "config_sha256": config_digest,
        "prefix": str(provider.prefix),
        "identity": provider.identity,
        "home": str(provider.home),
        "home_identity": provider.home_identity,
        "launcher_pid": child.pid,
    }
    payload = json.dumps(receipt, sort_keys=True).encode()
    receipt_path = provider.directory / f"mcp-launch-{uuid.uuid4().hex}.json"
    descriptor = os.open(receipt_path, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
    with os.fdopen(descriptor, "wb") as stream:
        stream.write(payload)
    receipt_digest = hashlib.sha256(payload).hexdigest()

    def verify():
        provider.assert_owner()
        provider.assert_home()
        if (
            hashlib.sha256(receipt_path.read_bytes()).hexdigest() != receipt_digest
            or hashlib.sha256(path.read_bytes()).hexdigest() != digest
            or hashlib.sha256(config.read_bytes()).hexdigest() != config_digest
        ):
            raise RuntimeError("Configured MCP launch receipt changed")

    verify()
    print(
        json.dumps(
            {
                "id": 0,
                "result": {
                    "profile": receipt["profile"],
                    "receipt": receipt_path.name,
                    "sha256": receipt_digest,
                    "command_sha256": digest,
                },
            }
        ),
        flush=True,
    )
    return verify


def bridge(root, command, arguments, configured):
    provider = stage("provider", provider_for, root)
    path, digest = stage("executable", executable, command)
    env = stage("guest-environment", guest_environment, provider, configured)
    image = path.name
    if image in stage("preflight-inventory", provider.processes).values():
        raise RuntimeError("Another same-image guest already exists in this prefix")
    provider.assert_home()
    child = stage(
        "guest-launch",
        subprocess.Popen,
        ["wine", str(path), *arguments],
        env=env,
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
    )
    failed = threading.Event()
    ended = threading.Event()
    guest_pid = None
    first_error = None

    def input_loop():
        try:
            for line in sys.stdin.buffer:
                child.stdin.write(line)
                child.stdin.flush()
        except (OSError, ValueError):
            failed.set()
        finally:
            try:
                child.stdin.close()
            except (OSError, ValueError):
                pass
            ended.set()

    # No raw stderr is emitted or retained: it may contain configured material.
    def drain_error():
        while child.stderr.read(4096):
            pass

    input_thread = threading.Thread(target=input_loop, daemon=True)
    error_thread = threading.Thread(target=drain_error, daemon=True)
    input_thread.start()
    error_thread.start()
    try:
        # This private adapter frame is consumed before PS sends initialize.
        # No Wine path/tasklist preflight consumes an MCP response deadline.
        verify_launch = stage(
            "launch-ready",
            launch_ready,
            provider,
            path,
            digest,
            arguments,
            configured,
            env,
            child,
        )
        for line in child.stdout:
            response = json.loads(line)
            if not isinstance(response, dict):
                raise RuntimeError("Configured MCP response is not an object")
            if response.get("id") == 2 and "result" in response:
                verify_launch()
                # PS remains the protocol/count authority. Bind its response
                # to the actual guest in the fresh prefix before forwarding it.
                guest_pid = stage("guest-identity", corroborate, provider, child, image)
            sys.stdout.buffer.write(line)
            sys.stdout.buffer.flush()
            if response.get("id") == 2:
                # Close after tools/list is consumed and client input closes.
                if not ended.wait(10) or failed.is_set() or guest_pid is None:
                    raise RuntimeError("MCP client/guest handshake did not settle")
                break
        if (
            guest_pid is None
            or child.poll() not in (None, 0)
            or hashlib.sha256(path.read_bytes()).hexdigest() != digest
        ):
            raise RuntimeError("Configured MCP executable/protocol proof is incomplete")
    except BaseException as error:
        first_error = error
        raise
    finally:
        # Intentional rejected-config/runtime assertions still fail, while exact cleanup
        # is independently attempted. Only incomplete cleanup poisons later fixtures.
        try:
            stage("settlement", settle, provider, child, image, guest_pid)
            error_thread.join(2)
            if error_thread.is_alive():
                raise RuntimeError("MCP stderr pipe remains open; prefix retained")
            provider.assert_owner()
            provider.assert_home()
        except BaseException as cleanup_error:
            # No global/prefix kill: retain both fixture and prefix on incomplete proof.
            cleanup_failure = static_failure(cleanup_error)
            try:
                (root / "mcp-wine-HOLD").touch(exist_ok=True)
            except OSError as hold_error:
                cleanup_failure["hold_failure"] = static_failure(hold_error)
            if first_error is None:
                cleanup_error.mcp_cleanup_failure = cleanup_failure
                raise
            first_error.mcp_cleanup_failure = cleanup_failure
    if child.returncode != 0:
        raise RuntimeError("Configured MCP launcher failed")
    verify_launch()


def main(argv=None):
    os.umask(0o077)
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("operation", choices=("prepare", "bridge", "finalize"))
    parser.add_argument("--root", required=True)
    parser.add_argument("--command")
    parser.add_argument("--arguments-json", default="[]")
    args = parser.parse_args(argv)
    token = STAGES.set([])
    root = None
    try:
        root = fixture_root(args.root)
        if sys.platform != "linux":
            raise RuntimeError("Wine fixture requires Linux")
        if args.operation == "prepare":
            print(json.dumps(prepare(root)))
        elif args.operation == "finalize":
            if (root / "mcp-wine-HOLD").exists():
                raise RuntimeError("MCP prefix is on HOLD")
            provider = provider_for(root)
            provider.wait_prefix(10)
            provider.retire_prefix()
            print(json.dumps({"settled": True}))
        else:
            configured = json.loads(
                os.environ.get("AGENT_BUS_TEST_MCP_CONFIGURED_ENV", "{}")
            )
            arguments = json.loads(args.arguments_json)
            if not isinstance(arguments, list) or any(
                not isinstance(value, str) for value in arguments
            ):
                raise RuntimeError("Configured MCP arguments are invalid")
            bridge(root, args.command, arguments, configured)
        return 0
    except (OSError, RuntimeError, ValueError, subprocess.SubprocessError) as error:
        diagnostic = {
            "profile": "configured-mcp-wine-failure-v1",
            "failure": static_failure(error),
            "stages": STAGES.get(),
        }
        if getattr(error, "mcp_cleanup_failure", None) is not None:
            diagnostic["cleanup_failure"] = error.mcp_cleanup_failure
        if root is not None:
            try:
                diagnostic["receipt"] = retain_failure(root, diagnostic)
            except (OSError, RuntimeError) as retention_error:
                diagnostic["retention_failure"] = static_failure(retention_error)
        print(json.dumps(diagnostic), file=sys.stderr)
        print(
            "Controlled Wine MCP fixture failed; owned prefix retained", file=sys.stderr
        )
        return 1
    finally:
        STAGES.reset(token)


if __name__ == "__main__":
    raise SystemExit(main())
