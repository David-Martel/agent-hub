"""Run finite CI test/version groups in one exclusive, settled Wine prefix."""

from __future__ import annotations

import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import tempfile
import time

from wine_lifecycle import NativeWineProvider

TARGET = "x86_64-pc-windows-gnu"
UNIT_COMMANDS = (
    (
        "test",
        "--target",
        TARGET,
        "--workspace",
        "--lib",
        "--bins",
        "--",
        "--test-threads=4",
    ),
    (
        "test",
        "--target",
        TARGET,
        "-p",
        "agent-bus",
        "--test",
        "integration_isolation_test",
    ),
)


def sha256(path):
    return hashlib.sha256(Path(path).read_bytes()).hexdigest()


def tool_bindings(source, proof, cargo):
    """Consume the already-qualified setup proof, not a second bootstrap."""
    if (
        proof.get("target") != TARGET
        or proof.get("abi") != "gnu"
        or proof.get("host_os") != "Linux"
        or proof.get("host_arch") != "X64"
    ):
        raise RuntimeError("Qualified cross proof required")
    owner = proof["dedicated_cache_owner"]
    result = {}
    # Explicit image-home bindings are needed because our Wine HOME is private.
    for key, field in (("CARGO_HOME", "cargo_home"), ("RUSTUP_HOME", "rustup_home")):
        path = Path(proof[field])
        if (
            not path.is_absolute()
            or not path.is_dir()
            or path.is_symlink()
            or str(path.resolve(strict=True)) != proof[field]
            or (source.get(key) and source[key] != proof[field])
        ):
            raise RuntimeError("CI tool home differs from qualified image proof")
        result[key] = proof[field]
    rustc = source["RUSTC"]
    wrapper = source["RUSTC_WRAPPER"]
    linker = source["CARGO_TARGET_X86_64_PC_WINDOWS_GNU_LINKER"]
    for path, expected in (
        (cargo, proof["cargo_sha256"]),
        (rustc, proof["rustc_sha256"]),
        (linker, proof["linker_sha256"]),
        (wrapper, owner["executable_sha256"]),
    ):
        if sha256(path).lower() != expected.lower():
            raise RuntimeError("CI executable differs from accepted setup proof")
    if (
        source["SCCACHE_DIR"] != owner["directory"]
        or source["SCCACHE_CONF"] != owner["config"]
        or source["SCCACHE_SERVER_PORT"] != "4228"
        or owner.get("idle_timeout") != "1800"
        or source.get("SCCACHE_IDLE_TIMEOUT") != "1800"
        or source["CARGO_BUILD_RUSTC"] != rustc
        or linker != proof["linker"]
        or cargo != proof["cargo"]
        or rustc != proof["rustc"]
    ):
        raise RuntimeError("CI compiler/cache bindings changed")
    for key in (
        "RUSTC_WORKSPACE_WRAPPER",
        "CARGO_BUILD_RUSTC_WORKSPACE_WRAPPER",
        "CARGO_BUILD_RUSTC_WRAPPER",
    ):
        if source.get(key) != "":
            raise RuntimeError("Inherited compiler wrapper refused")
        result[key] = ""
    # Do not make credentials usable just because a Cargo directory was bound.
    if any(
        (Path(result["CARGO_HOME"]) / leaf).exists()
        for leaf in ("credentials", "credentials.toml")
    ):
        raise RuntimeError("Credential-bearing Cargo home refused")
    result.update(
        RUSTC=rustc,
        CARGO_BUILD_RUSTC=rustc,
        RUSTC_WRAPPER=wrapper,
        CARGO_TARGET_X86_64_PC_WINDOWS_GNU_LINKER=linker,
        SCCACHE_DIR=owner["directory"],
        SCCACHE_CONF=owner["config"],
        SCCACHE_SERVER_PORT="4228",
        SCCACHE_IDLE_TIMEOUT="1800",
        CARGO_INCREMENTAL="0",
        CARGO_BUILD_JOBS="2",
        CARGO_TARGET_DIR=str(Path(source["CARGO_TARGET_DIR"]).resolve(strict=True)),
        CARGO_TARGET_X86_64_PC_WINDOWS_GNU_RUNNER="wine",
        CARGO_TERM_COLOR="always",
    )
    return result


def closed_environment(directory, source):
    """Keep only the qualified tool path and explicit closed-store settings."""
    return {
        "PATH": source["PATH"],
        "LANG": "C.UTF-8",
        "LC_ALL": "C.UTF-8",
        "TEMP": str(directory),
        "TMP": str(directory),
        "TMPDIR": str(directory),
        "AGENT_BUS_CONFIG": str(directory / "agent-bus-config.json"),
        "AGENT_BUS_BUILD_REVISION": source["AGENT_BUS_BUILD_REVISION"],
        "AGENT_BUS_STARTUP_ENABLED": "false",
        "AGENT_BUS_REDIS_URL": "redis://localhost:1/0",
        "AGENT_BUS_DATABASE_URL": "postgresql://postgres@localhost:1/none",
        "AGENT_BUS_HUB_CACHE_TTL_SECONDS": "0",
        "RUST_LOG": "error",
    }


def group(
    provider,
    commands,
    budget,
    env,
    *,
    revision=None,
    version_count=0,
    finish=None,
    clock=time.monotonic,
):
    """Keep command and prefix settlement inside the existing group budget."""
    started = clock()
    first_failure = None
    outputs = []
    try:
        provider.assert_owner()
        provider.assert_home()
        for index, command in enumerate(commands):
            remaining = budget - (clock() - started) - 10
            if remaining <= 0:
                raise RuntimeError("Wine group deadline exhausted; prefix retained")
            # Native Cargo can legitimately run for the original 15-minute job
            # budget. It uses the same owned direct-client/drain semantics as
            # finite queries, without replacing that budget with a short one.
            output = provider.command(command, env=env, timeout=remaining)
            outputs.append(output)
            if index < version_count and revision[:12].lower() not in output.lower():
                raise RuntimeError("Wine artifact build revision differs")
    except Exception as failure:
        first_failure = failure
    try:
        remaining = budget - (clock() - started)
        if remaining <= 0:
            raise RuntimeError("No owned-prefix settlement budget remains")
        # Successful Unix/Cargo exit is insufficient: wait for this exact
        # owned Wine server (and therefore its guests), never kill by name/tree.
        provider.wait_prefix(min(10, remaining))
        provider.assert_owner()
        provider.assert_home()
        if first_failure is None and finish:
            finish(outputs)
            provider.assert_owner()
            provider.assert_home()
        if clock() - started >= budget:
            raise RuntimeError("Wine group settled after deadline; prefix retained")
    except Exception as failure:
        if first_failure is None:
            first_failure = failure
    if first_failure is not None:
        raise first_failure
    provider.retire_prefix()
    return outputs


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("mode", choices=("units", "versions"))
    parser.add_argument("--binary", action="append", type=Path, default=[])
    parser.add_argument("--wine-version-file", type=Path)
    args = parser.parse_args()
    source = dict(os.environ)
    revision = source.get("AGENT_BUS_BUILD_REVISION", "")
    if not re.fullmatch(r"[a-f0-9]{40}", revision):
        raise RuntimeError("Reviewed source revision required")
    target = Path(source["CARGO_TARGET_DIR"]).resolve(strict=True)
    proof = json.loads(
        (target / "cross-toolchain-proof.json").read_text(encoding="utf-8")
    )
    cargo = proof["cargo"]
    bindings = tool_bindings(source, proof, cargo)
    if args.mode == "units":
        if args.binary or args.wine_version_file:
            raise RuntimeError("Units do not accept version artifacts")
        commands = [[cargo, *command] for command in UNIT_COMMANDS]
    else:
        if not args.binary:
            raise RuntimeError("Version artifacts required")
        binaries = [binary.resolve(strict=True) for binary in args.binary]
        if any(
            not binary.is_relative_to(target / TARGET / "release")
            or binary.suffix != ".exe"
            for binary in binaries
        ):
            raise RuntimeError(
                "Version artifact escapes the reviewed release directory"
            )
        commands = [["wine", str(binary), "--version"] for binary in binaries]
        if args.wine_version_file:
            commands.append(["wine", "--version"])
    directory = Path(
        tempfile.mkdtemp(prefix="agent-bus-wine-job-", dir=source["RUNNER_TEMP"])
    )
    descriptor = os.open(
        directory / "agent-bus-config.json", os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600
    )
    with os.fdopen(descriptor, "w", encoding="utf-8") as stream:
        stream.write("{}")
    env = closed_environment(directory, source)
    budget = 900 if args.mode == "units" else 300
    started = time.monotonic()
    provider = NativeWineProvider(directory, env)
    env = provider.guest_environment(env)
    env.update(bindings)

    def finish(outputs):
        if args.wine_version_file:
            with args.wine_version_file.open("x", encoding="utf-8") as stream:
                stream.write("wine_version=" + outputs[-1].strip() + "\n")

    outputs = group(
        provider,
        commands,
        budget - (time.monotonic() - started),
        env,
        revision=revision,
        version_count=len(args.binary),
        finish=finish,
    )
    for output in outputs:
        print(output, end="" if output.endswith("\n") else "\n")
    shutil.rmtree(directory)


if __name__ == "__main__":
    try:
        main()
    except (RuntimeError, OSError, ValueError, KeyError, TypeError):
        raise SystemExit("Owned Wine job failed; see owned custody evidence") from None
