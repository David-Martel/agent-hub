"""Tests for archive_release_build.py and the CI job and upload steps that use it.

Breadcrumb: scripts/ci/archive_release_build.py; ci.yml `build-release` upload and
`archive-release-build` job; release.yml `windows-x64` and `linux` uploads.
"""

from __future__ import annotations

import contextlib
import hashlib
import io
import json
import re
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

import archive_release_build as arb

REPO_ROOT = Path(__file__).resolve().parents[2]
REVISION = "a" * 40
NAMES = ("agent-bus-linux-x64", "agent-bus-linux-arm64-1")


def make_artifact(source: Path, name: str, revision: str = REVISION) -> dict[str, str]:
    artifact = source / name
    artifact.mkdir(parents=True)
    hashes = {}
    for binary in arb.BINARIES:
        data = f"{name}/{binary}\n".encode()
        (artifact / binary).write_bytes(data)
        hashes[binary] = hashlib.sha256(data).hexdigest()
    (artifact / arb.HASH_FILE).write_text(
        "".join(f"{hashes[b]}  {b}\n" for b in arb.BINARIES), encoding="utf-8"
    )
    (artifact / arb.PROVENANCE_FILE).write_text(
        f"commit={revision}\nrunner=test\n", encoding="utf-8"
    )
    return hashes


def run(source: Path, dest: Path, *extra: str) -> tuple[int, str]:
    stderr = io.StringIO()
    with contextlib.redirect_stderr(stderr), contextlib.redirect_stdout(io.StringIO()):
        code = arb.main(
            [
                "--source",
                str(source),
                "--dest-root",
                str(dest),
                "--run-id",
                "41",
                "--attempt",
                "2",
                "--job",
                "build-release",
                "--revision",
                REVISION,
                *extra,
            ]
        )
    return code, stderr.getvalue()


class ArchiveTests(unittest.TestCase):
    def setUp(self) -> None:
        self.tmp = Path(tempfile.mkdtemp())
        self.addCleanup(
            lambda: __import__("shutil").rmtree(self.tmp, ignore_errors=True)
        )
        self.source = self.tmp / "download"
        self.dest = self.tmp / "archive"
        self.final = self.dest / "41" / "2" / "build-release"

    def test_verified_artifacts_are_copied_with_a_read_back_manifest(self) -> None:
        """Protects: the dtm-p1gen7 archive holds byte-identical Release Build output.

        Detects: a copy that drops a file, a manifest whose hashes are not the archived
        bytes, or an archive path that is not <run>/<attempt>/<job>/<artifact>.
        Needs: a temporary directory only.
        Breadcrumb: archive_release_build.archive; ci.yml archive-release-build.
        """
        hashes = {name: make_artifact(self.source, name) for name in NAMES}
        code, err = run(self.source, self.dest, *[f"--require={n}" for n in NAMES])
        self.assertEqual((code, err), (0, ""))
        manifest = json.loads(
            (self.final / "manifest.json").read_text(encoding="utf-8")
        )
        self.assertEqual(manifest["revision"], REVISION)
        self.assertEqual((manifest["run_id"], manifest["run_attempt"]), (41, 2))
        self.assertEqual(sorted(manifest["artifacts"]), sorted(NAMES))
        for name in NAMES:
            files = manifest["artifacts"][name]["files"]
            self.assertEqual(
                sorted(files),
                sorted([*arb.BINARIES, arb.HASH_FILE, arb.PROVENANCE_FILE]),
            )
            for file, entry in files.items():
                self.assertEqual(
                    entry["sha256"], arb.sha256_file(self.final / name / file)
                )
            for binary in arb.BINARIES:
                self.assertEqual(files[binary]["sha256"], hashes[name][binary])
        self.assertEqual(
            sorted(p.name for p in self.final.parent.iterdir()), ["build-release"]
        )

    def test_hash_mismatch_fails_and_leaves_no_archive(self) -> None:
        """Protects: a corrupted or substituted binary never reaches the archive.

        Detects: the job passing when a binary differs from artifact-sha256.txt, or a
        partial staging directory left behind. Positive control: the same tree passes
        before the byte is changed.
        Needs: a temporary directory only.
        Breadcrumb: archive_release_build.verify_artifact.
        """
        make_artifact(self.source, NAMES[0])
        arb.verify_artifact(self.source / NAMES[0], REVISION)
        (self.source / NAMES[0] / "agent-bus-http").write_bytes(b"tampered\n")
        code, err = run(self.source, self.dest)
        self.assertEqual(code, 1)
        self.assertIn("agent-bus-http sha256", err)
        self.assertFalse(self.dest.exists() and any(self.dest.rglob("*")))

    def test_an_existing_archive_is_never_overwritten(self) -> None:
        """Protects: an archived run attempt is immutable.

        Detects: a re-run or a colliding run id replacing archived bytes.
        Needs: a temporary directory only.
        Breadcrumb: archive_release_build.archive final.exists check.
        """
        make_artifact(self.source, NAMES[0])
        self.final.mkdir(parents=True)
        (self.final / "keep").write_text("original\n", encoding="utf-8")
        code, err = run(self.source, self.dest)
        self.assertEqual(code, 1)
        self.assertIn("never overwritten", err)
        self.assertEqual(sorted(p.name for p in self.final.iterdir()), ["keep"])
        self.assertEqual(
            sorted(p.name for p in self.final.parent.iterdir()), ["build-release"]
        )

    def test_missing_required_artifact_and_wrong_provenance_fail(self) -> None:
        """Protects: a successful Release Build is archived completely, for this commit.

        Detects: an upload that silently failed (the upload step is continue-on-error),
        provenance from another commit, and a hash list that omits a binary.
        Needs: a temporary directory only.
        Breadcrumb: archive_release_build --require; verify_artifact provenance
        and list checks.
        """
        make_artifact(self.source, NAMES[0])
        code, err = run(self.source, self.dest, f"--require={NAMES[1]}")
        self.assertEqual(code, 1)
        self.assertIn("required artifacts are missing", err)

        other = self.tmp / "other"
        make_artifact(other, NAMES[0], revision="b" * 40)
        code, err = run(other, self.dest)
        self.assertEqual(code, 1)
        self.assertIn("provenance commit", err)

        short = self.tmp / "short"
        make_artifact(short, NAMES[0])
        listing = short / NAMES[0] / arb.HASH_FILE
        listing.write_text(
            listing.read_text(encoding="utf-8").splitlines()[0] + "\n", encoding="utf-8"
        )
        code, err = run(short, self.dest)
        self.assertEqual(code, 1)
        self.assertIn("expected", err)
        self.assertFalse(self.final.exists())

    def test_no_artifacts_and_nothing_required_is_a_no_op(self) -> None:
        """Protects: a run whose Release Build failed everywhere invents no archive.

        Detects: an empty archive directory created for a run with nothing to keep.
        Needs: a temporary directory only.
        Breadcrumb: archive_release_build.archive early return.
        """
        self.assertEqual(run(self.source, self.dest), (0, ""))
        self.assertFalse(self.dest.exists())


def step_block(source: str, name: str) -> str:
    match = re.search(
        rf"(?m)^      - uses: actions/upload-artifact@\S+\n(?:        .*\n)*?"
        rf"          name: {re.escape(name)}\n(?:          .*\n)*",
        source,
    )
    return match[0] if match else ""


def job_block(source: str, job: str) -> str:
    match = re.search(rf"(?ms)^  {re.escape(job)}:\n(.*?)(?=^  \S|\Z)", source)
    return match[1] if match else ""


def best_effort_one_day(block: str) -> bool:
    return (
        "        continue-on-error: true\n" in block
        and re.search(r"(?m)^          retention-days: 1$", block) is not None
    )


class WorkflowContractTests(unittest.TestCase):
    ci = (REPO_ROOT / ".github/workflows/ci.yml").read_text(encoding="utf-8")
    release = (REPO_ROOT / ".github/workflows/release.yml").read_text(encoding="utf-8")

    def test_transport_uploads_are_best_effort_and_kept_one_day(self) -> None:
        """Protects: a GitHub artifact quota cannot turn a build red, and GitHub keeps
        transport copies for one day only (owner direction 2026-10-29).

        Detects: continue-on-error or retention-days 1 dropped from the Release Build
        upload in ci.yml or from either release.yml upload. Positive control: the
        checker rejects the block once retention is raised to 2.
        Needs: the actual ci.yml and release.yml.
        Breadcrumb: ci.yml build-release upload; release.yml windows-x64 and
        linux uploads.
        """
        block = step_block(self.ci, "agent-bus-${{ matrix.name }}")
        self.assertTrue(block)
        self.assertTrue(best_effort_one_day(block), block)
        raised = block.replace("retention-days: 1\n", "retention-days: 2\n")
        self.assertFalse(best_effort_one_day(raised))

    def test_release_uploads_are_fatal_and_kept_one_day(self) -> None:
        """Protects: a release never publishes a partial set, because a failed
        platform upload fails its job and the publish job needs every platform.

        Detects: continue-on-error returning to either release.yml upload step, or
        retention-days leaving 1. The check failed against the best-effort steps
        before they were made fatal.
        Needs: the actual release.yml.
        Breadcrumb: release.yml windows-x64 and linux uploads; publish needs.
        """
        blocks = {
            "release windows-x64": step_block(self.release, "agent-bus-windows-x64"),
            "release linux": step_block(self.release, "agent-bus-${{ matrix.name }}"),
        }
        for label, block in blocks.items():
            with self.subTest(label):
                self.assertTrue(block, label)
                self.assertNotIn("continue-on-error", block)
                self.assertRegex(block, r"(?m)^          retention-days: 1$")

    def test_archive_job_verifies_every_release_build_on_dtm_p1gen7(self) -> None:
        """Protects: every Release Build leg is hash-verified into the p1gen7 archive.

        Detects: the archive job moving off the dtm-p1gen7 runner, losing its
        dependency on build-release, running the helper without -I, or a matrix leg
        that the --require list does not name.
        Needs: the actual ci.yml.
        Breadcrumb: ci.yml archive-release-build; build-release matrix.
        """
        job = job_block(self.ci, "archive-release-build")
        self.assertIn(
            "    runs-on: [self-hosted, Windows, X64, agent-hub-native-smoke]\n", job
        )
        self.assertIn("    needs: [build-release]\n", job)
        self.assertIn("          pattern: agent-bus-linux-*\n", job)
        self.assertIn("python -I -B scripts/ci/archive_release_build.py", job)
        self.assertIn(r"C:\codedev\agent-hub-artifacts\ci", job)
        legs = re.findall(
            r"(?m)^          - name: (\S+)$", job_block(self.ci, "build-release")
        )
        self.assertEqual(legs, ["linux-x64", "linux-arm64-1", "linux-arm64-2"])
        required = re.findall(r"'--require', '(agent-bus-linux-[a-z0-9-]+)'", job)
        self.assertEqual(required, [f"agent-bus-{leg}" for leg in legs])

    def test_archive_job_and_its_upload_skip_pull_request_runs_together(self) -> None:
        """Protects: the archive job never waits on an artifact that PR runs do not
        upload (the quota fix skips the build-release upload on pull_request).

        Detects: the upload losing its pull_request skip, or the archive job losing
        the same condition, so PR runs fail on an empty download. Positive control:
        removing the condition from the archive job makes the check fail.
        Needs: the actual ci.yml.
        Breadcrumb: ci.yml build-release upload; archive-release-build if.
        """
        skip = "github.event_name != 'pull_request'"
        upload = step_block(self.ci, "agent-bus-${{ matrix.name }}")
        job = job_block(self.ci, "archive-release-build")
        self.assertIn(f"        if: {skip}\n", upload)
        self.assertRegex(job, rf"(?m)^    if: \$\{{\{{ {re.escape(skip)} && ")
        ungated = job.replace(f"{skip} && ", "")
        self.assertNotRegex(ungated, rf"(?m)^    if: \$\{{\{{ {re.escape(skip)} && ")


if __name__ == "__main__":
    unittest.main()
