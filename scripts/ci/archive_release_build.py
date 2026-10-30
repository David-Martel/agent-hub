"""Verify the Release Build artifacts of one CI run and archive them on dtm-p1gen7.

Owner rule (2026-10-29): agent-hub artifacts live on dtm-* hosts and in the owner's
Google Drive VIGIL_DATA folder, never on vigil1, the Sparks or the fleet share. GitHub
artifacts are only the transport between the build runners and this archive.

Each `agent-bus-linux-*` directory under --source must hold the three binaries, the
`artifact-sha256.txt` written by `sha256sum` in the build job, and a
`build-provenance.txt` whose `commit=` line names --revision. The files are copied to
`<dest-root>/<run-id>/<attempt>/<job>/<artifact>/` through a staging directory, hashed
again after the copy (read-back), and a `manifest.json` records every file. An existing
destination is never overwritten. Any mismatch exits 1 and leaves no final directory.

Usage (CI, on the agent-hub-native-smoke runner):
    python -I -B scripts/ci/archive_release_build.py --source DIR --dest-root DIR \
        --run-id N --attempt N --job build-release --revision SHA [--require NAME ...]
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import re
import shutil
import sys
from pathlib import Path

BINARIES = ("agent-bus", "agent-bus-http", "agent-bus-mcp")
HASH_FILE = "artifact-sha256.txt"
PROVENANCE_FILE = "build-provenance.txt"
ARTIFACT_NAME = re.compile(r"agent-bus-linux-[a-z0-9-]+")
HASH_LINE = re.compile(r"([0-9a-f]{64}) [ *](\S+)")
REVISION = re.compile(r"[0-9a-f]{40}")
NUMBER = re.compile(r"[1-9][0-9]*")
JOB = re.compile(r"[a-z][a-z0-9-]*")


class ArchiveError(Exception):
    """A verification failure; the message names the file and the reason."""


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for block in iter(lambda: handle.read(1 << 20), b""):
            digest.update(block)
    return digest.hexdigest()


def expected_hashes(artifact: Path) -> dict[str, str]:
    """Parse sha256sum output; it must list exactly the three binaries."""
    hashes: dict[str, str] = {}
    for line in (artifact / HASH_FILE).read_text(encoding="utf-8").splitlines():
        if not line.strip():
            continue
        match = HASH_LINE.fullmatch(line.strip())
        if match is None:
            raise ArchiveError(f"{artifact.name}/{HASH_FILE}: unparsable line {line!r}")
        name = match[2].removeprefix("./")
        if name in hashes:
            raise ArchiveError(f"{artifact.name}/{HASH_FILE}: {name} listed twice")
        hashes[name] = match[1]
    if set(hashes) != set(BINARIES):
        raise ArchiveError(
            f"{artifact.name}/{HASH_FILE}: lists {sorted(hashes)}, "
            f"expected {list(BINARIES)}"
        )
    return hashes


def verify_artifact(artifact: Path, revision: str) -> dict[str, str]:
    """Check one downloaded artifact and return the sha256 of every file in it."""
    if not (artifact / HASH_FILE).is_file():
        raise ArchiveError(f"{artifact.name}: {HASH_FILE} is missing")
    expected = expected_hashes(artifact)
    provenance = artifact / PROVENANCE_FILE
    if not provenance.is_file():
        raise ArchiveError(f"{artifact.name}: {PROVENANCE_FILE} is missing")
    commits = re.findall(r"(?m)^commit=(\S+)$", provenance.read_text(encoding="utf-8"))
    if commits != [revision]:
        raise ArchiveError(
            f"{artifact.name}: provenance commit {commits} is not {revision}"
        )
    files = sorted(p for p in artifact.iterdir())
    if any(not p.is_file() or p.is_symlink() for p in files):
        raise ArchiveError(f"{artifact.name}: holds something other than regular files")
    actual = {p.name: sha256_file(p) for p in files}
    for name, digest in expected.items():
        if name not in actual:
            raise ArchiveError(f"{artifact.name}: {name} is missing")
        if actual[name] != digest:
            raise ArchiveError(
                f"{artifact.name}: {name} sha256 {actual[name]} != {digest}"
            )
    return actual


def archive(args: argparse.Namespace) -> list[Path]:
    for value, pattern, flag in (
        (args.revision, REVISION, "--revision"),
        (args.run_id, NUMBER, "--run-id"),
        (args.attempt, NUMBER, "--attempt"),
        (args.job, JOB, "--job"),
    ):
        if not pattern.fullmatch(value):
            raise ArchiveError(f"{flag} {value!r} is not valid")
    source = Path(args.source)
    present = (
        sorted(p for p in source.iterdir() if p.is_dir()) if source.is_dir() else []
    )
    loose = (
        sorted(p.name for p in source.iterdir() if not p.is_dir())
        if source.is_dir()
        else []
    )
    if loose:
        raise ArchiveError(
            f"loose files in the source, not under an artifact directory: {loose}"
        )
    unexpected = [p.name for p in present if not ARTIFACT_NAME.fullmatch(p.name)]
    if unexpected:
        raise ArchiveError(f"unexpected artifact directories: {unexpected}")
    missing = sorted(set(args.require) - {p.name for p in present})
    if missing:
        raise ArchiveError(f"required artifacts are missing: {missing}")
    if not present:
        print("archive_release_build: no Release Build artifacts to archive")
        return []

    verified = {
        artifact.name: verify_artifact(artifact, args.revision) for artifact in present
    }
    final = Path(args.dest_root) / args.run_id / args.attempt / args.job
    if final.exists():
        raise ArchiveError(f"{final} already exists; an archive is never overwritten")
    staging = final.with_name(f"{final.name}.partial-{os.getpid()}")
    staging.mkdir(parents=True)
    try:
        for artifact in present:
            target = staging / artifact.name
            target.mkdir()
            for name, digest in verified[artifact.name].items():
                shutil.copyfile(artifact / name, target / name)
                if sha256_file(target / name) != digest:
                    raise ArchiveError(
                        f"read-back of {artifact.name}/{name} does not match"
                    )
        manifest = {
            "schema": "agent-hub-release-build-archive/1",
            "revision": args.revision,
            "run_id": int(args.run_id),
            "run_attempt": int(args.attempt),
            "job": args.job,
            "artifacts": {
                name: {
                    "files": {
                        file: {
                            "sha256": digest,
                            "bytes": (staging / name / file).stat().st_size,
                        }
                        for file, digest in files.items()
                    }
                }
                for name, files in verified.items()
            },
        }
        (staging / "manifest.json").write_text(
            json.dumps(manifest, indent=2, sort_keys=True) + "\n", encoding="utf-8"
        )
        os.rename(staging, final)
    except BaseException:
        shutil.rmtree(staging, ignore_errors=True)
        raise
    for name in verified:
        print(f"archive_release_build: {name} -> {final / name}")
    return [final / name for name in verified]


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--source", required=True)
    parser.add_argument("--dest-root", required=True)
    parser.add_argument("--run-id", required=True)
    parser.add_argument("--attempt", required=True)
    parser.add_argument("--job", required=True)
    parser.add_argument("--revision", required=True)
    parser.add_argument("--require", action="append", default=[])
    args = parser.parse_args(argv)
    try:
        archive(args)
    except (ArchiveError, OSError) as error:
        print(f"archive_release_build: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
