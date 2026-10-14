"""Qualify an existing/absent native cache without stopping or resetting it."""

import argparse
import hashlib
import json
import os
from pathlib import Path
import subprocess
import tempfile


def digest(path):
    return hashlib.sha256(Path(path).read_bytes()).hexdigest()


def validate_identity(snapshot, version, directory):
    """Refuse a foreign daemon before issuing any compiler request."""
    if snapshot.get("version") != version:
        raise ValueError("serving daemon version differs from the pinned client")
    if json.dumps(str(Path(directory).resolve())) not in snapshot.get(
        "cache_location", ""
    ):
        raise ValueError(
            "serving cache directory differs from the private runner cache"
        )


def validate_stats(before, after, version, directory):
    """Require real Rust reuse, daemon/cache binding and unchanged error counters."""
    validate_identity(after, version, directory)
    old, new = before["stats"], after["stats"]
    old_hits = old["cache_hits"]["counts"].get("Rust", 0)
    new_hits = new["cache_hits"]["counts"].get("Rust", 0)
    if new_hits <= old_hits:
        raise ValueError(
            "two compiler requests did not establish a real Rust cache hit"
        )
    for name in (
        "cache_timeouts",
        "cache_read_errors",
        "cache_write_errors",
        "compile_fails",
        "requests_unsupported_compiler",
        "requests_not_cacheable",
    ):
        if new[name] != old[name]:
            raise ValueError(f"cache preflight increased {name}")
    if new["cache_errors"] != old["cache_errors"]:
        raise ValueError("cache preflight increased backend errors")


def qualify(sccache, rustc, directory, cache_directory, version):
    def run(args):
        return subprocess.check_output(
            args, text=True, timeout=45, env=os.environ.copy()
        )

    before = json.loads(run([sccache, "--show-stats", "--stats-format=json"]))
    validate_identity(before, version, cache_directory)
    # Unique owned source prevents accidentally accepting another invocation's
    # output. Keep the two real requests identical so the second must reuse it.
    Path(directory).mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(
        prefix="native-cache-proof-", dir=directory
    ) as temp:
        root = Path(temp)
        source = root / "preflight.rs"
        source.write_text("pub fn native_cache_proof() -> u64 { 42 }\n")
        args = [
            sccache,
            rustc,
            "--crate-name",
            "agent_hub_native_cache_proof",
            "--edition=2024",
            str(source),
            "--crate-type=rlib",
            "--emit=link",
            "--out-dir",
            str(root),
            "-Dwarnings",
        ]
        run(args)
        artifact = root / "libagent_hub_native_cache_proof.rlib"
        first = digest(artifact)
        artifact.unlink()
        run(args)
        if artifact.stat().st_size == 0 or first != digest(artifact):
            raise ValueError(
                "cached artifact is empty or differs from the original compile"
            )
        after = json.loads(run([sccache, "--show-stats", "--stats-format=json"]))
        validate_stats(before, after, version, cache_directory)
        receipt = {
            "schema_version": 1,
            "server_version": after["version"],
            "cache_location": after["cache_location"],
            "sccache": sccache,
            "sccache_sha256": digest(sccache),
            "rustc": rustc,
            "rustc_sha256": digest(rustc),
            "artifact_sha256": first,
            "before": before,
            "after": after,
        }
        (Path(directory) / "native-cache-proof.json").write_text(
            json.dumps(receipt, indent=2) + "\n"
        )
        print(json.dumps(receipt, indent=2))


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    for key in ("sccache", "rustc", "directory", "cache-directory", "version"):
        parser.add_argument("--" + key, required=True)
    args = parser.parse_args()
    qualify(
        args.sccache, args.rustc, args.directory, args.cache_directory, args.version
    )
