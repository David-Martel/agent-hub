#!/usr/bin/env python3
"""Execute real hook QA against immutable Git snapshots in owned containers.

Python 3.11+. Configuration is explicit, local and outside Git; no tool installs,
native Cargo fallback, receipt reuse, live configuration or daemon socket mount.
"""

from __future__ import annotations

import argparse
import hashlib
import io
import json
import os
from pathlib import Path, PurePosixPath
import re
import shutil
import stat
import subprocess
import sys
import tarfile
import tempfile
import uuid
import tomllib
import time


def require(condition, message):
    if not condition:
        raise RuntimeError(message)


def digest(path):
    return hashlib.sha256(Path(path).read_bytes()).hexdigest()


def linked(path):
    return path.is_symlink() or getattr(path, "is_junction", lambda: False)()


def clean_host_environment(source):
    names = {"PATH", "SYSTEMROOT", "WINDIR", "COMSPEC", "PATHEXT", "TEMP", "TMP"}
    result = {k: v for k, v in source.items() if k.upper() in names}
    result["GIT_NO_REPLACE_OBJECTS"] = "1"
    if "GIT_INDEX_FILE" in source:
        result["GIT_INDEX_FILE"] = source["GIT_INDEX_FILE"]
    return result


def git(repo, *args):
    return subprocess.check_output(
        ["git", "-C", str(repo), *args],
        env=clean_host_environment(os.environ),
        timeout=60,
    )


def outgoing_commits(text):
    require(bool(text.strip()), "Missing pre-push ref stdin; refusing HEAD inference")
    commits = []
    for line in text.splitlines():
        fields = line.split()
        require(len(fields) == 4, "Malformed pre-push ref record")
        local, oid, remote, old = fields
        require(
            re.fullmatch(r"[a-f0-9]{40}", oid) is not None
            and re.fullmatch(r"[a-f0-9]{40}", old) is not None,
            "Expected full SHA-1 pre-push identities",
        )
        require(
            (local.startswith("refs/") or local in {"HEAD", "(delete)"})
            and remote.startswith("refs/"),
            "Invalid pre-push ref names",
        )
        if oid != "0" * 40 and oid not in commits:
            commits.append(oid)
    return commits


def safe_name(name):
    path = PurePosixPath(name)
    require(
        not path.is_absolute()
        and bool(path.parts)
        and all(p not in {".", ".."} for p in path.parts)
        and "\\" not in name
        and ":" not in name,
        "Unsafe archive path",
    )
    for part in path.parts:
        stem = part.rstrip(" .").split(".", 1)[0].upper()
        require(
            part.casefold() != "$null"
            and part == part.rstrip(" .")
            and not any(ord(c) < 32 or c in '<>"|?*' for c in part)
            and not re.fullmatch(r"AUX|CON|NUL|PRN|COM[1-9]|LPT[1-9]", stem),
            "Reserved Windows snapshot path refused",
        )
    return path


def extract_snapshot(archive, destination, expected):
    """Only tracked regular blobs; reject symlinks, export omissions and rewrites."""
    seen = set()
    with tarfile.open(fileobj=io.BytesIO(archive), mode="r:") as stream:
        for entry in stream:
            relative = safe_name(entry.name)
            if entry.isdir():
                continue
            require(
                entry.isfile() and entry.name in expected and entry.name not in seen,
                "Unexpected, linked or duplicate snapshot entry",
            )
            seen.add(entry.name)
            data = stream.extractfile(entry).read()
            blob = hashlib.sha1(
                b"blob " + str(len(data)).encode() + b"\0" + data
            ).hexdigest()
            require(blob == expected[entry.name], "Archive differs from indexed blob")
            path = destination.joinpath(*relative.parts)
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_bytes(data)
            path.chmod(0o755 if entry.mode & 0o111 else 0o644)
    require(seen == set(expected), "Archive omitted tracked source")


def tree_entries(raw):
    """Admit regular tracked files without Windows path aliases."""
    entries, prefixes = [], {}
    for record in raw.split(b"\0"):
        if record:
            meta, name = record.split(b"\t", 1)
            mode, kind, oid = meta.decode().split()
            require(
                mode in {"100644", "100755"} and kind == "blob",
                "Linked source or submodule requires separate reviewed custody",
            )
            require(re.fullmatch(r"[a-f0-9]{40}", oid), "Invalid indexed blob identity")
            name = name.decode("utf-8")
            relative = safe_name(name)
            require(relative.as_posix() == name, "Noncanonical indexed path")
            for count in range(1, len(relative.parts) + 1):
                prefix = "/".join(relative.parts[:count])
                canonical = prefix.casefold()
                require(
                    canonical not in prefixes or prefixes[canonical] == prefix,
                    "Windows case-colliding indexed path",
                )
                prefixes[canonical] = prefix
            require(name not in {e[0] for e in entries}, "Duplicate indexed path")
            entries.append((name, mode, oid))
    return entries


def parse_blob_batch(output, entries):
    """Check batch identity, order, count, size and raw Git blob hashes."""
    cursor, blobs = 0, []
    for name, mode, oid in entries:
        newline = output.find(b"\n", cursor)
        require(newline >= 0, "Truncated Git blob batch header")
        fields = output[cursor:newline].split()
        require(
            len(fields) == 3
            and fields[0] == oid.encode()
            and fields[1] == b"blob"
            and fields[2].isdigit(),
            "Git blob batch identity, type or order differs",
        )
        size = int(fields[2])
        cursor = newline + 1
        data = output[cursor : cursor + size]
        require(
            len(data) == size and output[cursor + size : cursor + size + 1] == b"\n",
            "Truncated Git blob batch body",
        )
        require(
            hashlib.sha1(b"blob " + str(size).encode() + b"\0" + data).hexdigest()
            == oid,
            "Raw blob differs from indexed identity",
        )
        blobs.append((name, mode, data))
        cursor += size + 1
    require(cursor == len(output), "Unexpected extra Git blob batch output")
    return blobs


def snapshot(repo, tree, destination):
    entries = tree_entries(git(repo, "ls-tree", "-rz", "--full-tree", tree))
    require(not any(destination.iterdir()), "Snapshot destination must start empty")
    command = ["git", "-C", str(repo), "cat-file", "--batch"]
    started = time.monotonic()
    process = subprocess.Popen(
        command,
        env=clean_host_environment(os.environ),
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
    )
    receipt = {"pid": process.pid, "timeout_seconds": 60, "state": "RUNNING"}
    try:
        requests = b"".join(oid.encode() + b"\n" for _, _, oid in entries)
        try:
            output, stderr = process.communicate(input=requests, timeout=60)
        except subprocess.TimeoutExpired:
            receipt["first_failure"] = "Owned Git batch deadline exceeded"
            process.kill()  # Exact owned Popen child; never global termination.
            output, stderr = process.communicate(timeout=15)
            receipt.update(exit_code=process.returncode, both_eof=True)
            raise RuntimeError(receipt["first_failure"]) from None
        receipt.update(
            exit_code=process.returncode,
            both_eof=True,
            elapsed_seconds=time.monotonic() - started,
            requested_blobs=len(entries),
            stdout_sha256=hashlib.sha256(output).hexdigest(),
            stderr_sha256=hashlib.sha256(stderr).hexdigest(),
        )
        require(process.returncode == 0, "Owned Git blob batch failed")
        blobs = parse_blob_batch(output, entries)
        for name, mode, data in blobs:
            path = destination.joinpath(*safe_name(name).parts)
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_bytes(data)
            path.chmod(0o755 if mode == "100755" else 0o644)
        receipt["state"] = "PASS"
        return {name: digest(destination / name) for name, _, _ in entries}
    except Exception as error:
        receipt.update(
            state="FAIL", first_failure=receipt.get("first_failure", str(error))
        )
        raise
    finally:
        receipt["original_child_settled"] = process.poll() is not None
        (destination.parent / "git-blob-export.json").write_text(
            json.dumps(receipt, indent=2)
        )


def inventory(directory):
    result = {}
    for path in directory.rglob("*"):
        require(not linked(path), "Linked input refused")
        if path.is_file():
            result[path.relative_to(directory).as_posix()] = digest(path)
    return result


def validate_config(config):
    require(
        re.fullmatch(r"sha256:[a-f0-9]{64}", config["image"]) is not None,
        "Pinned local image identity required",
    )
    require(
        config["endpoint"].startswith(("unix://", "npipe://")),
        "Only explicit local Docker socket endpoints admitted",
    )
    require(30 <= config["timeout_seconds"] <= 3600, "Invalid QA deadline")
    require(1 <= config["jobs"] <= 4, "Invalid job limit")
    for key in ("cargo_home", "target_dir", "tools_dir", "evidence_root"):
        path = Path(config[key])
        require(
            path.is_absolute() and path.is_dir() and not linked(path),
            "Owned regular absolute directories required",
        )
    paths = [
        Path(config[k]).resolve()
        for k in ("cargo_home", "target_dir", "tools_dir", "evidence_root")
    ]
    for i, left in enumerate(paths):
        for right in paths[i + 1 :]:
            require(
                left != right
                and left not in right.parents
                and right not in left.parents,
                "Mount roots must not overlap",
            )
    for name in ("credentials", "credentials.toml", "config", "config.toml"):
        require(
            not (Path(config["cargo_home"]) / name).exists(),
            "Cargo credentials or host configuration refused",
        )
    require(
        inventory(Path(config["tools_dir"])) == config["tool_files_sha256"],
        "Read-only tool inventory differs from reviewed configuration",
    )


def validate_envelope(metadata, image, label, phase, mounts):
    """Refuse engine-side changes to the admitted ownership/isolation envelope."""
    host, container = metadata["HostConfig"], metadata["Config"]
    require(
        metadata["Image"] == image and container["Image"] == image,
        "Actual container image differs",
    )
    require(
        container["User"] == "1000:1000" and host["ReadonlyRootfs"] is True,
        "Actual UID or root filesystem isolation differs",
    )
    require(
        container["Labels"].get("agent-bus.hook-run") == label,
        "Actual container owner label differs",
    )
    require(
        host["Memory"] == 4294967296
        and host["MemorySwap"] == 4294967296
        and host["NanoCpus"] == 4000000000
        and host["PidsLimit"] == 512,
        "Actual memory, swap, CPU or PID envelope differs",
    )
    require(
        host["NetworkMode"] == ("bridge" if phase == "audit" else "none"),
        "Actual container network differs",
    )
    require(
        host["Privileged"] is False
        and not host.get("CapAdd")
        and host.get("CapDrop") == ["ALL"]
        and not host.get("Devices")
        and not host.get("DeviceRequests")
        and not host.get("DeviceCgroupRules")
        and host.get("PidMode", "") != "host",
        "Unexpected privilege, capability, device or host PID access",
    )
    require(
        host.get("SecurityOpt") in (["no-new-privileges"], ["no-new-privileges:true"]),
        "Actual no-new-privileges policy differs",
    )
    require(
        host.get("Tmpfs")
        == {"/tmp": "rw,exec,nosuid,nodev,size=268435456,uid=1000,gid=1000,mode=700"},
        "Actual owned temporary filesystem differs",
    )

    def canonical(value):
        return str(value).replace("\\", "/").rstrip("/")

    expected = {
        guest: (canonical(source), readonly) for source, guest, readonly in mounts
    }
    configured = host.get("Mounts", [])
    require(len(configured) == len(expected), "Unexpected configured mount count")
    require(
        {m["Target"] for m in configured} == set(expected),
        "Missing or duplicated configured bind target",
    )
    for mount in configured:
        guest = mount["Target"]
        require(
            guest in expected
            and mount["Type"] == "bind"
            and (canonical(mount["Source"]), bool(mount.get("ReadOnly")))
            == expected[guest],
            "Configured bind mount differs",
        )
    effective = [m for m in metadata["Mounts"] if m["Type"] == "bind"]
    require(len(effective) == len(expected), "Unexpected effective bind count")
    require(
        {m["Destination"] for m in effective} == set(expected),
        "Missing or duplicated effective bind target",
    )
    for mount in effective:
        guest = mount["Destination"]
        require(
            guest in expected
            and (canonical(mount["Source"]), not mount["RW"]) == expected[guest],
            "Effective bind mount differs",
        )
    for mount in metadata["Mounts"]:
        require(
            mount["Type"] == "bind"
            or (mount["Type"] == "tmpfs" and mount["Destination"] == "/tmp"),
            "Unexpected effective volume or temporary mount",
        )


def handoff_names(phase):
    """Enumerate fixed, host-owned files written by the container actor."""
    count = {"pre-commit": 6, "pre-push": 3, "audit": 4}[phase]
    return ["actor-receipt.json"] + [
        f"command-{number}.{stream}.log"
        for number in range(count)
        for stream in ("stdout", "stderr")
    ]


def existing_handoff(path):
    """Truncate a held regular file without creating or replacing its custody."""
    before = path.lstat()
    require(
        stat.S_ISREG(before.st_mode) and not path.is_symlink(), "Invalid handoff file"
    )
    stream = path.open("r+b")
    held = os.fstat(stream.fileno())
    if (before.st_dev, before.st_ino) != (held.st_dev, held.st_ino):
        stream.close()
        raise RuntimeError("Handoff identity changed")
    stream.truncate(0)
    return stream


def actor(job):
    """Container entry: fixed commands, private HOME, no inherited environment."""
    source, output = Path("/source"), Path("/work")
    output.mkdir(exist_ok=True)
    home = output / "home"
    home.mkdir(mode=0o700)
    config = home / "config.json"
    config.write_text("{}")
    env = {
        "PATH": "/tools/bin:/usr/local/cargo/bin:/usr/bin:/bin",
        "HOME": str(home),
        "LOCALAPPDATA": str(home),
        "TMPDIR": "/tmp",
        "CARGO_HOME": "/output/focused-core/cargo-home",
        "CARGO_TARGET_DIR": "/output/focused-core/target",
        "RUSTUP_HOME": "/usr/local/rustup",
        "RUSTUP_AUTO_INSTALL": "0",
        "RUSTUP_NO_UPDATE_CHECK": "1",
        "CARGO_BUILD_JOBS": str(job["jobs"]),
        "CARGO_INCREMENTAL": "0",
        "CARGO_PROFILE_DEV_CODEGEN_UNITS": "16",
        "CARGO_PROFILE_TEST_CODEGEN_UNITS": "16",
        "RUSTC_WRAPPER": "",
        "AGENT_BUS_CONFIG": str(config),
        "AGENT_BUS_STARTUP_ENABLED": "false",
        "AGENT_BUS_REDIS_URL": "redis://localhost:1/0",
        "AGENT_BUS_DATABASE_URL": "postgresql://postgres@localhost:1/none",
        "AGENT_BUS_HUB_CACHE_TTL_SECONDS": "0",
        "GIT_CONFIG_NOSYSTEM": "1",
        "GIT_CONFIG_GLOBAL": "/dev/null",
        "GIT_TERMINAL_PROMPT": "0",
    }
    if job["phase"] == "audit":
        audit_cargo = home / "cargo"
        audit_cargo.mkdir(mode=0o700)
        env["CARGO_HOME"] = str(audit_cargo)
        env["CARGO_TARGET_DIR"] = "/work/unused-target"
    channel = tomllib.loads((source / "rust-toolchain.toml").read_text())["toolchain"][
        "channel"
    ]
    sysroot = "/usr/local/rustup/toolchains/" + channel + "-x86_64-unknown-linux-gnu"
    env.update(SYSROOT=sysroot, LD_LIBRARY_PATH=sysroot + "/lib")
    if job["phase"] != "audit":
        env["CARGO_NET_OFFLINE"] = "true"
    commands = {
        "pre-commit": [
            ["cargo", "fmt", "--all", "--check"],
            [
                "cargo",
                "clippy",
                "--workspace",
                "--all-targets",
                "--locked",
                "--offline",
                "--",
                "-D",
                "warnings",
            ],
        ],
        "pre-push": [["bash", "scripts/test-agent-bus-isolated.sh"]],
        "audit": [["cargo", "audit", "--db", "/work/advisory-db"]],
    }[job["phase"]]
    versions = [["rustc", "--version"], ["cargo", "--version"]]
    versions += {
        "pre-commit": [["cargo", "fmt", "--version"], ["cargo", "clippy", "--version"]],
        "pre-push": [],
        "audit": [["cargo", "audit", "--version"]],
    }[job["phase"]]
    record = {"phase": job["phase"], "commands": [], "versions": [], "state": "RUNNING"}
    try:
        for number, command in enumerate(versions + commands):
            with (
                existing_handoff(output / f"command-{number}.stdout.log") as stdout,
                existing_handoff(output / f"command-{number}.stderr.log") as stderr,
            ):
                completed = subprocess.run(
                    command,
                    cwd=source,
                    env=env,
                    stdout=stdout,
                    stderr=stderr,
                    check=False,
                )
            record["versions" if number < len(versions) else "commands"].append(
                {"argv": command, "exit_code": completed.returncode}
            )
            if number < 2 and completed.returncode == 0:
                version = (output / f"command-{number}.stdout.log").read_text()
                require(
                    version.startswith(command[0] + " " + channel + " "),
                    "Compiler/Cargo version differs from source toolchain",
                )
            if completed.returncode:
                record["state"] = "FAIL"
                return completed.returncode
        if job["phase"] == "audit":
            commit = (
                subprocess.check_output(
                    ["git", "-C", "/work/advisory-db", "rev-parse", "HEAD"],
                    env=env,
                    timeout=15,
                )
                .decode()
                .strip()
            )
            require(
                re.fullmatch(r"[a-f0-9]{40}", commit) is not None,
                "Advisory DB identity missing",
            )
            record["advisory_commit"] = commit
        record["state"] = "PASS"
        return 0
    finally:
        with existing_handoff(output / "actor-receipt.json") as receipt:
            receipt.write(json.dumps(record, indent=2).encode())


def execute_container(config, source, run, phase, script):
    """One captured ID; natural rm only after exact child and PID settlement."""
    phase_dir = run / phase
    phase_dir.mkdir(mode=0o700)
    # Docker Desktop may assign a different Windows owner to guest-created
    # files. The host creates only these explicit nonsecret output leaves;
    # the actor opens them in place, preserving their host ownership and ACL.
    handoff_identity = {}
    for name in handoff_names(phase):
        target = phase_dir / name
        descriptor = os.open(target, os.O_CREAT | os.O_EXCL | os.O_WRONLY, 0o600)
        os.close(descriptor)
        metadata = target.stat()
        handoff_identity[name] = (metadata.st_dev, metadata.st_ino)
    docker_config = phase_dir / "docker-config"
    docker_config.mkdir(mode=0o700)
    (docker_config / "config.json").write_text("{}")
    job_file = phase_dir / "job.json"
    job_file.write_text(json.dumps({"phase": phase, "jobs": config["jobs"]}))
    docker = [
        shutil.which("docker"),
        "--host",
        config["endpoint"],
        "--config",
        str(docker_config),
    ]
    require(docker[0], "Docker is missing; no native fallback")
    environment = clean_host_environment(os.environ)

    def control(*args):
        return subprocess.check_output(docker + list(args), env=environment, timeout=45)

    require(
        control("info", "--format", "{{.OSType}}").strip() == b"linux",
        "Linux engine required",
    )
    image = json.loads(control("image", "inspect", config["image"]))[0]
    require(image["Id"] == config["image"], "Local image identity changed")
    label, cid, original = uuid.uuid4().hex, None, None
    receipt = {
        "state": "STARTING",
        "phase": phase,
        "cleanup_errors": [],
        "first_failure": None,
    }
    code = 1
    try:
        command = [
            "create",
            "--cidfile",
            str(phase_dir / "container.cid"),
            "--pull=never",
            "--label",
            "agent-bus.hook-run=" + label,
            "--user",
            "1000:1000",
            "--read-only",
            "--cap-drop=ALL",
            "--security-opt",
            "no-new-privileges",
            "--memory",
            "4g",
            "--memory-swap",
            "4g",
            "--cpus",
            "4",
            "--pids-limit",
            "512",
            "--network",
            "bridge" if phase == "audit" else "none",
            "--tmpfs",
            "/tmp:rw,exec,nosuid,nodev,size=268435456,uid=1000,gid=1000,mode=700",
        ]
        mounts = [
            (source, "/source", True),
            (config["tools_dir"], "/tools", True),
            (config["cargo_home"], "/output/focused-core/cargo-home", False),
            (config["target_dir"], "/output/focused-core/target", False),
            (script, "/controller/run-hook-qa.py", True),
            (job_file, "/controller/job.json", True),
            (phase_dir, "/work", False),
        ]
        if phase == "audit":
            # Networked advisory checking cannot modify compilation caches.
            mounts = [
                m
                for m in mounts
                if m[1]
                not in {
                    "/output/focused-core/cargo-home",
                    "/output/focused-core/target",
                }
            ]
        for host, guest, readonly in mounts:
            require("," not in str(host), "Comma in bind path refused")
            command += [
                "--mount",
                f"type=bind,src={host},dst={guest}" + (",readonly" if readonly else ""),
            ]
        command += [
            "--entrypoint",
            "/usr/bin/python3",
            config["image"],
            "-I",
            "-B",
            "/controller/run-hook-qa.py",
            "--actor",
            "/controller/job.json",
        ]
        try:
            control(*command)
        finally:
            cid_file = phase_dir / "container.cid"
            if cid_file.exists():
                cid = cid_file.read_text().strip()
                require(
                    re.fullmatch(r"[a-f0-9]{64}", cid) is not None,
                    "Invalid captured container ID",
                )
        require(cid, "No captured container ID")
        metadata = json.loads(control("inspect", cid))[0]
        (phase_dir / "pre-start-inspect.json").write_text(
            json.dumps(metadata, indent=2)
        )
        validate_envelope(metadata, config["image"], label, phase, mounts)
        receipt["actual_envelope_verified"] = True
        original = subprocess.Popen(
            docker + ["start", "--attach", cid],
            env=environment,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
        )
        receipt["original_cli_pid"] = original.pid
        try:
            stdout, stderr = original.communicate(timeout=config["timeout_seconds"])
        except subprocess.TimeoutExpired:
            receipt["first_failure"] = "QA deadline exceeded"
            control("stop", "--time", "30", cid)
            stdout, stderr = original.communicate(timeout=45)
        (phase_dir / "attached.stdout.log").write_bytes(stdout)
        (phase_dir / "attached.stderr.log").write_bytes(stderr)
        receipt.update(
            original_cli_exit=original.returncode, original_cli_both_eof=True
        )
        terminal = json.loads(control("inspect", cid))[0]
        (phase_dir / "terminal-inspect.json").write_text(json.dumps(terminal, indent=2))
        validate_envelope(terminal, config["image"], label, phase, mounts)
        state = terminal["State"]
        receipt["terminal_state"] = state
        require(not state["Running"] and state["Pid"] == 0, "Container has not settled")
        require(
            original.returncode == 0
            and state["ExitCode"] == 0
            and not state.get("OOMKilled")
            and not receipt["first_failure"],
            "Actual QA failed",
        )
        for name, identity in handoff_identity.items():
            target = phase_dir / name
            metadata = target.lstat()
            require(stat.S_ISREG(metadata.st_mode) and not target.is_symlink()
                    and (metadata.st_dev, metadata.st_ino) == identity,
                    "Actor replaced a host-owned handoff file")
            with target.open("rb") as stream:
                stream.read(1)
        receipt["host_handoff_readback"] = True
        result = json.loads((phase_dir / "actor-receipt.json").read_text())
        require(result["state"] == "PASS", "Actor did not complete actual QA")
        code = 0
    except Exception as error:
        receipt["first_failure"] = receipt["first_failure"] or str(error)
    finally:
        if cid:
            try:
                metadata = json.loads(control("inspect", cid))[0]
                require(
                    metadata["Config"]["Labels"].get("agent-bus.hook-run") == label,
                    "Cleanup custody mismatch",
                )
                if metadata["State"]["Running"]:
                    control("stop", "--time", "30", cid)
                if original is not None and original.poll() is None:
                    stdout, stderr = original.communicate(timeout=45)
                    (phase_dir / "attached.stdout.log").write_bytes(stdout)
                    (phase_dir / "attached.stderr.log").write_bytes(stderr)
                    receipt.update(
                        original_cli_exit=original.returncode,
                        original_cli_both_eof=True,
                    )
                state = json.loads(control("inspect", cid))[0]["State"]
                require(
                    not state["Running"]
                    and state["Pid"] == 0
                    and (original is None or original.poll() is not None),
                    "Cleanup settlement missing",
                )
                control("rm", cid)
                require(
                    not control(
                        "ps", "--all", "--quiet", "--filter", "id=" + cid
                    ).strip(),
                    "Captured container still present",
                )
                receipt["removed_captured_id"] = cid
            except Exception as error:
                receipt["cleanup_errors"].append(str(error))
                code = 1
        receipt["state"] = "PASS" if code == 0 else "FAIL"
        (phase_dir / "controller-receipt.json").write_text(
            json.dumps(receipt, indent=2)
        )
    return code


def main(mode):
    repo = Path(git(Path.cwd(), "rev-parse", "--show-toplevel").decode().strip())
    script = Path(__file__).resolve()
    configuration = Path(os.environ["AGENT_BUS_HOOK_QA_CONFIG"])
    config = json.loads(configuration.read_text())
    validate_config(config)
    config_sha, script_sha = digest(configuration), digest(script)
    head = git(repo, "rev-parse", "HEAD").decode().strip()
    index = git(repo, "write-tree").decode().strip()
    commits = [None] if mode == "pre-commit" else outgoing_commits(sys.stdin.read())
    lease = Path(config["evidence_root"]) / "active-hook-qa.lock"
    lease_bytes = json.dumps({"pid": os.getpid(), "nonce": uuid.uuid4().hex}).encode()
    descriptor = os.open(lease, os.O_CREAT | os.O_EXCL | os.O_WRONLY, 0o600)
    with os.fdopen(descriptor, "wb") as stream:
        stream.write(lease_bytes)
    root = Path(tempfile.mkdtemp(prefix="hook-qa-", dir=config["evidence_root"]))
    receipt = {
        "mode": mode,
        "head": head,
        "index_tree": index,
        "state": "RUNNING",
        "config_sha256": config_sha,
        "adapter_sha256": script_sha,
        "snapshots": [],
    }
    try:
        for number, commit in enumerate(commits):
            if commit:
                kind = git(repo, "cat-file", "-t", commit).strip()
                require(
                    kind == b"commit",
                    "Non-commit push objects need explicit separate admission",
                )
            tree = (
                index
                if commit is None
                else git(repo, "rev-parse", commit + "^{tree}").decode().strip()
            )
            run = root / str(number)
            source = run / "source"
            source.mkdir(parents=True)
            before = snapshot(repo, tree, source)
            require(
                before.get("scripts/ci/run-hook-qa.py") == script_sha,
                "Executing adapter differs from snapshot; "
                "stage/commit the reviewed adapter first",
            )
            receipt["snapshots"].append(
                {"commit": commit, "tree": tree, "files_sha256": before}
            )
            phases = ["pre-commit"] if mode == "pre-commit" else ["pre-push", "audit"]
            for phase in phases:
                code = execute_container(config, source, run, phase, script)
                require(
                    inventory(source) == before, "Read-only source inventory changed"
                )
                require(code == 0, "Container gate failed: " + phase)
        require(
            git(repo, "rev-parse", "HEAD").decode().strip() == head
            and git(repo, "write-tree").decode().strip() == index,
            "HEAD or index moved during QA",
        )
        require(
            digest(configuration) == config_sha and digest(script) == script_sha,
            "Hook adapter or configuration changed during QA",
        )
        validate_config(config)
        receipt["state"] = "PASS"
        return 0
    except Exception as error:
        receipt.update(state="FAIL", first_failure=str(error))
        return 1
    finally:
        (root / "receipt.json").write_text(json.dumps(receipt, indent=2))
        settled = True
        for controller in root.rglob("controller-receipt.json"):
            state = json.loads(controller.read_text())
            settled = settled and not state["cleanup_errors"]
        if settled and lease.read_bytes() == lease_bytes:
            lease.unlink()
        else:
            print(
                "Retaining exact hook lease pending lifecycle reconciliation",
                file=sys.stderr,
            )
        print(
            "Container hook QA " + receipt["state"] + ": " + str(root / "receipt.json"),
            file=sys.stderr,
        )


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("mode", nargs="?", choices=["pre-commit", "pre-push"])
    parser.add_argument("--actor", type=Path)
    args = parser.parse_args()
    try:
        if args.actor:
            result = actor(json.loads(args.actor.read_text()))
        else:
            require(args.mode, "Hook mode is required")
            result = main(args.mode)
    except Exception as failure:
        print("Container hook QA refused: " + str(failure), file=sys.stderr)
        result = 1
    sys.exit(result)
