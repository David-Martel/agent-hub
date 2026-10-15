"""Qualify an existing/absent native cache without stopping or resetting it."""

import argparse
import copy
import hashlib
import json
import os
from pathlib import Path
import re
import subprocess
import tempfile
import urllib.parse


def digest(path):
    return hashlib.sha256(Path(path).read_bytes()).hexdigest()


def validate_identity(
    snapshot, version, directory, chain="disk", redis_endpoint=None, redis_prefix=None
):
    """Refuse a foreign daemon before issuing any compiler request."""
    if snapshot.get("version") != version:
        raise ValueError("serving daemon version differs from the pinned client")
    local = "Local disk: " + json.dumps(
        str(Path(directory).resolve()), ensure_ascii=False
    )
    if chain == "disk,redis":
        # sccache0.18 renders the top-level location as a summary only. The
        # serving daemon's concrete backend identities are in stats.multi_level.
        stats = snapshot.get("stats")
        levels = stats.get("multi_level") if isinstance(stats, dict) else None
        if (
            snapshot.get("cache_location") != "Multi-level (2 levels)"
            or not isinstance(levels, list)
            or len(levels) != 2
            or not all(isinstance(level, dict) for level in levels)
            or [level.get("name") for level in levels] != ["L0 (disk)", "L1 (redis)"]
        ):
            raise ValueError("serving cache level inventory differs from disk,redis")
        if levels[0].get("location") != local:
            raise ValueError(
                "serving cache directory differs from the private runner cache"
            )
        # The admitted Spark peer uses a credential-free endpoint. Never echo a
        # supplied endpoint, prefix or daemon payload in a refusal diagnostic.
        try:
            peer = urllib.parse.urlsplit(redis_endpoint or "")
            port = peer.port
        except ValueError:
            raise ValueError("expected Redis cache identity is invalid") from None
        if (
            peer.scheme not in ("redis", "rediss")
            or not peer.hostname
            or peer.username is not None
            or peer.password is not None
            or peer.query
            or peer.fragment
            or not re.fullmatch(r"(?:/\d+)?", peer.path)
            or not isinstance(redis_prefix, str)
            or not redis_prefix
            or "/" in redis_prefix
            or any(ord(c) < 32 for c in redis_prefix)
        ):
            raise ValueError("expected Redis cache identity is invalid")
        host = f"[{peer.hostname}]" if ":" in peer.hostname else peer.hostname
        canonical = urllib.parse.urlunsplit(
            (
                peer.scheme,
                host + (f":{port}" if port is not None else ""),
                peer.path,
                "",
                "",
            )
        )
        if canonical != redis_endpoint:
            raise ValueError("expected Redis cache identity is invalid")
        expected = f"redis, name: {canonical}, prefix: /{redis_prefix}/"
        if levels[1].get("location") != expected:
            raise ValueError(
                "serving Redis peer or key prefix differs from the admitted cache"
            )
    elif chain != "disk" or snapshot.get("cache_location") != local:
        raise ValueError(
            "serving cache directory differs from the private runner cache"
        )


def sanitized_snapshot(snapshot):
    """Retain counters while redacting Redis userinfo/query/prefix from receipts."""
    safe = copy.deepcopy(snapshot)
    for level in safe.get("stats", {}).get("multi_level") or []:
        if level.get("name") != "L1 (redis)":
            continue
        raw = level.get("location", "")
        match = re.fullmatch(r"redis, name: (.*), prefix: (.*)", raw)
        endpoint = "redacted"
        if match:
            try:
                peer = urllib.parse.urlsplit(match[1])
                host = peer.hostname or "redacted"
                host = f"[{host}]" if ":" in host else host
                netloc = host + (f":{peer.port}" if peer.port is not None else "")
                path = peer.path if re.fullmatch(r"(?:/\d+)?", peer.path) else ""
                endpoint = urllib.parse.urlunsplit((peer.scheme, netloc, path, "", ""))
            except ValueError:
                pass
        level["location"] = (
            "redis, name: "
            + endpoint
            + ", identity_sha256: "
            + hashlib.sha256(raw.encode()).hexdigest()
        )
    return safe


def validate_stats(before, after, version, directory, *identity):
    """Require real Rust reuse, daemon/cache binding and unchanged error counters."""
    validate_identity(after, version, directory, *identity)
    if identity and identity[0] == "disk,redis":
        validate_identity(before, version, directory, *identity)
        for old_level, new_level in zip(
            before["stats"]["multi_level"], after["stats"]["multi_level"], strict=True
        ):
            # Under sccache's L0 write policy a failed background Redis write
            # increments this per-level counter while top-level errors stay0.
            old_errors, new_errors = (
                level.get("write_failures") for level in (old_level, new_level)
            )
            if any(
                type(value) is not int or value < 0
                for value in (old_errors, new_errors)
            ):
                raise ValueError("invalid cache-level write_failures counter")
            if new_errors != old_errors:
                raise ValueError("cache preflight changed cache-level write_failures")
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
    identity = (
        os.environ.get("SCCACHE_MULTILEVEL_CHAIN", "disk"),
        os.environ.get("SCCACHE_REDIS_ENDPOINT"),
        os.environ.get("SCCACHE_REDIS_KEY_PREFIX"),
    )
    validate_identity(before, version, cache_directory, *identity)
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
        validate_stats(before, after, version, cache_directory, *identity)
        receipt = {
            "schema_version": 1,
            "server_version": after["version"],
            "cache_location": after["cache_location"],
            "sccache": sccache,
            "sccache_sha256": digest(sccache),
            "rustc": rustc,
            "rustc_sha256": digest(rustc),
            "artifact_sha256": first,
            "before": sanitized_snapshot(before),
            "after": sanitized_snapshot(after),
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
