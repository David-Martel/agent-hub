"""Exercise exact-commit admission with closed API/ZIP fixtures, never credentials."""

import argparse
import base64
import copy
import hashlib
import importlib.util
import io
import json
from pathlib import Path
import re
import shutil
import stat
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch
import urllib.error
import zipfile

SPEC = importlib.util.spec_from_file_location(
    "release_reuse", Path(__file__).with_name("release_windows_artifact.py")
)
HELPER = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(HELPER)
REPO_ROOT = Path(__file__).resolve().parents[2]
RELEASE_SOURCE = REPO_ROOT / ".github/workflows/release.yml"
REPOSITORY = "David-Martel/agent-hub"
REVISION = "a" * 40
JOBS = [
    "Format Check",
    "Cloud Agentbus (TypeScript)",
    "Lefthook Gates Run",
    "Clippy Lint",
    "Unit Tests",
    "Integration Tests",
    "History Catalog Migration",
    "Release Build (linux-x64)",
    "Release Build (linux-arm64-1)",
    "Release Build (linux-arm64-2)",
    "CLI And HTTP Smoke",
    "Security Audit",
    "Windows Build And Unit Tests",
    "Benchmark Compile",
    "Docker Build (linux-x64)",
    "Docker Build (linux-arm64)",
]


def pe_fixture(marker):
    raw = bytearray(96)
    raw[:2] = b"MZ"
    raw[60:64] = (64).to_bytes(4, "little")
    raw[64:70] = b"PE\0\0\x64\x86"
    return bytes(raw) + marker.encode()


def archive_fixture(manifest_mutator=None, data_mutator=None, zip_mutator=None):
    binaries = {name: pe_fixture(name) for name in HELPER.BINARIES}
    manifest = {
        "revision": REVISION,
        "target": "x86_64-pc-windows-msvc",
        "validation": "native-windows",
        "runner": "hosted-windows-fixture",
        "binaries": {name: HELPER.sha256(raw) for name, raw in binaries.items()},
    }
    if manifest_mutator:
        manifest_mutator(manifest)
    data = {**binaries, "artifact-sha256.json": json.dumps(manifest).encode()}
    if data_mutator:
        data_mutator(data)
    stream = io.BytesIO()
    with zipfile.ZipFile(stream, "w", zipfile.ZIP_DEFLATED) as archive:
        for name, raw in data.items():
            archive.writestr(name, raw)
        if zip_mutator:
            zip_mutator(archive)
    return stream.getvalue()


class ApiFixture:
    def __init__(self, raw_ci, archive=None):
        self.source = raw_ci
        self.archive_bytes = archive or archive_fixture()
        self.run = {
            "id": 11,
            "workflow_id": 7,
            "name": "CI",
            "path": HELPER.CI_PATH,
            "head_sha": REVISION,
            "head_branch": "main",
            "event": "push",
            "run_number": 3,
            "run_attempt": 1,
            "status": "completed",
            "conclusion": "success",
            "created_at": "2026-01-01T00:00:00Z",
            "run_started_at": "2026-01-01T00:00:01Z",
            "updated_at": "2026-01-01T00:00:30Z",
            "repository": {"id": 2, "full_name": REPOSITORY},
            "head_repository": {"id": 2, "full_name": REPOSITORY},
        }
        self.runs = [self.run]
        self.jobs = [
            {
                "id": 100 + index,
                "name": name,
                "run_id": 11,
                "run_attempt": 1,
                "head_sha": REVISION,
                "status": "completed",
                "conclusion": "success",
                "started_at": "2026-01-01T00:00:02Z",
                "completed_at": "2026-01-01T00:00:20Z",
                "runner_name": "hosted-windows-fixture",
            }
            for index, name in enumerate(JOBS)
        ]
        self.jobs.append(
            {
                **self.jobs[0],
                "id": 199,
                "name": HELPER.MANUAL,
                "conclusion": "skipped",
                "started_at": None,
                "completed_at": None,
            }
        )
        self.artifact = {
            "id": 51,
            "name": HELPER.ARTIFACT,
            "expired": False,
            "size_in_bytes": len(self.archive_bytes),
            "digest": "sha256:" + HELPER.sha256(self.archive_bytes),
            "created_at": "2026-01-01T00:00:10Z",
            "updated_at": "2026-01-01T00:00:11Z",
            "workflow_run": {
                "id": 11,
                "head_sha": REVISION,
                "head_branch": "main",
                "repository_id": 2,
                "head_repository_id": 2,
            },
        }
        self.artifacts = [self.artifact]
        self.after_download = None
        self.downloads = 0

    def json(self, path):
        if path == "actions/workflows/ci.yml":
            value = {"id": 7, "name": "CI", "path": HELPER.CI_PATH}
        elif path.startswith("actions/workflows/7/runs?"):
            value = {"total_count": len(self.runs), "workflow_runs": self.runs}
        elif path == "actions/runs/11":
            value = self.run
        elif path.startswith("contents/"):
            value = {
                "type": "file",
                "path": HELPER.CI_PATH,
                "encoding": "base64",
                "size": len(self.source),
                "sha": hashlib.sha1(
                    b"blob " + str(len(self.source)).encode() + b"\0" + self.source
                ).hexdigest(),
                "content": base64.b64encode(self.source).decode(),
            }
        elif "/jobs?" in path:
            value = {"total_count": len(self.jobs), "jobs": self.jobs}
        elif path.startswith("actions/runs/11/artifacts?"):
            value = {"total_count": len(self.artifacts), "artifacts": self.artifacts}
        elif path == "actions/artifacts/51":
            value = self.artifact
        else:
            raise AssertionError("Unexpected fixed API path: " + path)
        return copy.deepcopy(value)

    def archive(self, artifact_id):
        assert artifact_id == 51
        self.downloads += 1
        if self.after_download:
            self.after_download(self)
        return self.archive_bytes


class AdmissionTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="release-ci-reuse-control-")
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        for path in (HELPER.CI_PATH, HELPER.POLICY_PATH):
            destination = self.root / path
            destination.parent.mkdir(parents=True, exist_ok=True)
            destination.write_bytes((REPO_ROOT / path).read_bytes())
        self.api = ApiFixture((self.root / HELPER.CI_PATH).read_bytes())

    def admit(self):
        return HELPER.admit(self.api, REPOSITORY, REVISION, self.root)

    def test_exact_success_reuses_bytes_and_stages_existing_contract(self):
        """Protects source/artifact reuse using closed API and archive fixtures."""
        data, receipt = self.admit()
        self.assertEqual(self.api.downloads, 1)
        self.assertFalse(receipt["recompiled"])
        self.assertEqual(set(receipt["jobs"]), {*JOBS, HELPER.MANUAL})
        artifact_dir, dist = self.root / "artifact", self.root / "dist"
        HELPER.write_bundle(artifact_dir, data, receipt)
        HELPER.refresh_and_stage(
            self.api,
            REPOSITORY,
            REVISION,
            self.root,
            artifact_dir,
            dist,
        )
        self.assertEqual(self.api.downloads, 1)
        self.assertEqual(
            json.loads((artifact_dir / "ci-stage-admission.json").read_text())[
                "status"
            ],
            "PASS",
        )
        for name in HELPER.BINARIES:
            staged = dist / (name[:-4] + "-windows-x64.exe")
            self.assertEqual(staged.read_bytes(), data[name])
            self.assertIn(
                HELPER.sha256(data[name]) + "  " + staged.name,
                (dist / "SHA256SUMS-windows-x64").read_text(),
            )
        build = (dist / "BUILD-windows-x64.txt").read_text()
        self.assertIn("recompiled=false\n", build)
        self.assertIn("abi=msvc\n", build)
        self.assertIn("ci_run_id=11\n", build)

    def test_reviewed_inventory_matches_independent_sixteen_job_oracle(self):
        rule, _ = HELPER.policy(self.root)
        self.assertEqual(rule["required_jobs"], JOBS)

    def test_changed_raw_producer_is_refused(self):
        self.api.source += b"\n# unreviewed source drift\n"
        with self.assertRaises(HELPER.Refused):
            self.admit()

    def test_manual_dispatch_semantics_cannot_be_replaced_with_always(self):
        source = (
            (self.root / HELPER.CI_PATH)
            .read_bytes()
            .replace(b"if: github.event_name == 'workflow_dispatch'", b"if: always()")
        )
        old = HELPER.sha256((self.root / HELPER.CI_PATH).read_bytes())
        (self.root / HELPER.CI_PATH).write_bytes(source)
        policy = self.root / HELPER.POLICY_PATH
        policy.write_text(policy.read_text().replace(old, HELPER.sha256(source)))
        self.api.source = source
        with self.assertRaisesRegex(HELPER.Refused, "manual skip predicate"):
            self.admit()

    def test_newer_failed_run_never_falls_back_to_old_success(self):
        self.api.runs.append(
            {
                **self.api.run,
                "id": 12,
                "created_at": "2026-01-01T00:01:00Z",
                "conclusion": "failure",
            }
        )
        old_json = self.api.json
        self.api.json = lambda path: (
            copy.deepcopy(self.api.runs[-1])
            if path == "actions/runs/12"
            else old_json(path)
        )
        with self.assertRaises(HELPER.Refused):
            self.admit()

    def test_existing_destination_and_post_admission_drift_fail_before_staging(self):
        data, receipt = self.admit()
        artifact_dir = self.root / "artifact"
        HELPER.write_bundle(artifact_dir, data, receipt)
        with self.assertRaises(HELPER.Refused):
            HELPER.write_bundle(artifact_dir, data, receipt)
        (artifact_dir / HELPER.BINARIES[0]).write_bytes(b"changed")
        dist = self.root / "dist"
        with self.assertRaises(HELPER.Refused):
            HELPER.stage(artifact_dir, dist, REVISION)
        self.assertFalse(dist.exists())

    def test_new_attempt_after_native_validation_refuses_staging(self):
        """Protects post-runtime freshness; detects new attempt before dist write."""
        data, receipt = self.admit()
        artifact_dir, dist = self.root / "artifact", self.root / "dist"
        HELPER.write_bundle(artifact_dir, data, receipt)
        self.api.run["run_attempt"] = 2
        with self.assertRaisesRegex(HELPER.Refused, "latest CI changed"):
            HELPER.refresh_and_stage(
                self.api,
                REPOSITORY,
                REVISION,
                self.root,
                artifact_dir,
                dist,
            )
        self.assertFalse(dist.exists())
        # The old offline-only stage accepts unchanged bytes despite stale CI:
        # this preserves the exact missing-admission boundary as a control.
        HELPER.stage(artifact_dir, self.root / "baseline-dist", REVISION)
        self.assertTrue((self.root / "baseline-dist").exists())

    def test_newer_failed_run_after_native_validation_refuses_staging(self):
        """Protects latest-run policy after native execution; detects stale success."""
        data, receipt = self.admit()
        artifact_dir, dist = self.root / "artifact", self.root / "dist"
        HELPER.write_bundle(artifact_dir, data, receipt)
        newer = {
            **self.api.run,
            "id": 12,
            "created_at": "2026-01-01T00:01:00Z",
            "conclusion": "failure",
        }
        self.api.runs.append(newer)
        old_json = self.api.json
        self.api.json = lambda path: (
            copy.deepcopy(newer) if path == "actions/runs/12" else old_json(path)
        )
        with self.assertRaisesRegex(HELPER.Refused, "not successful"):
            HELPER.refresh_and_stage(
                self.api,
                REPOSITORY,
                REVISION,
                self.root,
                artifact_dir,
                dist,
            )
        self.assertFalse(dist.exists())
        HELPER.stage(artifact_dir, self.root / "baseline-dist", REVISION)
        self.assertTrue((self.root / "baseline-dist").exists())


def run_mutation(field, value):
    return lambda api: api.run.__setitem__(field, value)


CASES = {
    "wrong_commit": run_mutation("head_sha", "b" * 40),
    "pr_event": run_mutation("event", "pull_request"),
    "wrong_branch": run_mutation("head_branch", "feature"),
    "dispatch_event": run_mutation("event", "workflow_dispatch"),
    "incomplete_run": run_mutation("status", "in_progress"),
    "failed_run": run_mutation("conclusion", "failure"),
    "wrong_workflow_path": run_mutation("path", ".github/workflows/other.yml"),
    "foreign_repo": lambda api: api.run["head_repository"].__setitem__(
        "full_name", "foreign/repo"
    ),
    "missing_job": lambda api: api.jobs.pop(0),
    "duplicate_job_name": lambda api: api.jobs[1].__setitem__(
        "name", api.jobs[0]["name"]
    ),
    "failed_job": lambda api: api.jobs[0].__setitem__("conclusion", "failure"),
    "skipped_mandatory": lambda api: api.jobs[0].__setitem__("conclusion", "skipped"),
    "manual_success": lambda api: api.jobs[-1].__setitem__("conclusion", "success"),
    "wrong_job_attempt": lambda api: api.jobs[0].__setitem__("run_attempt", 2),
    "wrong_job_head": lambda api: api.jobs[0].__setitem__("head_sha", "b" * 40),
    "duplicate_job_id": lambda api: api.jobs[1].__setitem__("id", api.jobs[0]["id"]),
    "unknown_job": lambda api: api.jobs[0].__setitem__("name", "unreviewed job"),
    "missing_job_clock": lambda api: api.jobs[0].__setitem__("completed_at", None),
    "expired_artifact": lambda api: api.artifact.__setitem__("expired", True),
    "missing_http_digest": lambda api: api.artifact.__setitem__("digest", None),
    "bad_http_digest": lambda api: api.artifact.__setitem__(
        "digest", "sha256:" + "0" * 64
    ),
    "foreign_artifact_revision": lambda api: api.artifact["workflow_run"].__setitem__(
        "head_sha", "b" * 40
    ),
    "prior_attempt_artifact": lambda api: api.artifact.__setitem__(
        "created_at", "2025-12-31T00:00:00Z"
    ),
    "duplicate_artifact": lambda api: api.artifacts.append({**api.artifact, "id": 52}),
    "attempt_changed_after_download": lambda api: setattr(
        api, "after_download", run_mutation("run_attempt", 2)
    ),
    "job_clock_changed_after_download": lambda api: setattr(
        api,
        "after_download",
        lambda fixture: fixture.jobs[0].__setitem__(
            "completed_at", "2026-01-01T00:00:21Z"
        ),
    ),
    "artifact_changed_after_download": lambda api: setattr(
        api,
        "after_download",
        lambda fixture: fixture.artifact.__setitem__("expired", True),
    ),
}


def detecting_case(mutation):
    def test(self):
        mutation(self.api)
        with self.assertRaises(HELPER.Refused):
            self.admit()

    test.__doc__ = (
        "Protects fail-closed admission; detects an invalid API witness before use."
    )
    return test


for case_name, mutation in CASES.items():
    setattr(AdmissionTests, "test_refuses_" + case_name, detecting_case(mutation))


class ZipAndHttpControls(unittest.TestCase):
    def test_late_final_http_read_cannot_report_success(self):
        """Protects total deadline at EOF; detects success after a slow final read."""
        api = HELPER.Github(REPOSITORY, "closed-fixture-token")
        api.expires = 300
        with (
            patch.object(HELPER.time, "monotonic", side_effect=[299, 301]),
            self.assertRaisesRegex(HELPER.Refused, "total deadline"),
        ):
            api.read(io.BytesIO(b""), 4)

    def test_native_manifest_and_binary_rejection_controls(self):
        mutations = [
            lambda m: m.__setitem__("revision", "b" * 40),
            lambda m: m.__setitem__("target", "x86_64-pc-windows-gnu"),
            lambda m: m.__setitem__("validation", "wine"),
            lambda m: m["binaries"].__setitem__("agent-bus.exe", "0" * 64),
            lambda m: m["binaries"].pop("agent-bus-mcp.exe"),
        ]
        for mutate in mutations:
            with self.subTest(mutation=mutate), self.assertRaises(HELPER.Refused):
                HELPER.verified_members(
                    archive_fixture(manifest_mutator=mutate),
                    REVISION,
                    "hosted-windows-fixture",
                )

    def test_unexpected_traversing_and_link_members_are_refused(self):
        for leaf in ("../outside", "C:/outside", "extra.txt", "folder/agent-bus.exe"):
            with self.subTest(leaf=leaf), self.assertRaises(HELPER.Refused):
                HELPER.verified_members(
                    archive_fixture(
                        zip_mutator=lambda archive: archive.writestr(leaf, b"no")
                    ),
                    REVISION,
                    "hosted-windows-fixture",
                )

        def link(archive):
            entry = zipfile.ZipInfo("agent-bus.exe")
            entry.external_attr = (stat.S_IFLNK | 0o777) << 16
            archive.writestr(entry, b"elsewhere")

        with self.assertWarns(UserWarning), self.assertRaises(HELPER.Refused):
            HELPER.verified_members(
                archive_fixture(zip_mutator=link), REVISION, "hosted-windows-fixture"
            )

    def test_zip_inflation_and_json_duplicate_keys_are_refused(self):
        compressed = archive_fixture(
            data_mutator=lambda data: data.__setitem__(
                "agent-bus.exe", data["agent-bus.exe"] + bytes(16384)
            )
        )
        self.assertLess(len(compressed), 4096)
        with (
            patch.object(HELPER, "MAX_ARCHIVE", 4096),
            self.assertRaisesRegex(HELPER.Refused, "expanded archive size"),
        ):
            HELPER.verified_members(compressed, REVISION, "hosted-windows-fixture")
        with self.assertRaises(HELPER.Refused):
            HELPER.decode_json(b'{"revision":"a","revision":"b"}')

    def test_redirect_never_receives_api_bearer(self):
        api = HELPER.Github(REPOSITORY, "closed-fixture-token")
        requests = []

        class Response(io.BytesIO):
            status = 200

        class Opener:
            def open(self, request, timeout):
                requests.append((request, timeout))
                if len(requests) == 1:
                    raise urllib.error.HTTPError(
                        request.full_url,
                        302,
                        "Found",
                        {
                            "Location": "https://fixture.blob.core.windows.net/archive?owned-fixture"
                        },
                        io.BytesIO(),
                    )
                return Response(b"archive fixture")

        api.opener = Opener()
        self.assertEqual(api.archive(51), b"archive fixture")
        self.assertEqual(
            requests[0][0].get_header("Authorization"), "Bearer closed-fixture-token"
        )
        self.assertIsNone(requests[1][0].get_header("Authorization"))
        self.assertLessEqual(requests[1][1], 20)

    def test_unapproved_redirect_and_resource_bounds_fail(self):
        api = HELPER.Github(REPOSITORY, "closed-fixture-token")

        class Opener:
            def open(self, request, timeout):
                raise urllib.error.HTTPError(
                    request.full_url,
                    302,
                    "Found",
                    {"Location": "https://unapproved.invalid/archive"},
                    io.BytesIO(),
                )

        api.opener = Opener()
        with self.assertRaises(HELPER.Refused):
            api.archive(51)
        with self.assertRaises(HELPER.Refused):
            api.read(io.BytesIO(b"12345"), 4)
        api.expires = 0
        with self.assertRaises(HELPER.Refused):
            api.open(api.base + "actions/runs/11", True)


def release_contract(source):
    job = re.search(r"(?ms)^  windows-x64:\r?\n(.*?)(?=^  linux:\r?$)", source)
    if not job:
        return False
    body = job[1]
    return (
        all(
            needle in body
            for needle in (
                "runs-on: windows-latest",
                "actions: read",
                "release_windows_artifact.py download",
                "test-agent-client-config-validator.ps1",
                "release_windows_artifact.py stage",
                "$env:PATH",
                "Read-ReusedNativeVersion",
                "10000 - $deadline.ElapsedMilliseconds",
                "$deadline.ElapsedMilliseconds -ge 10000",
                "Kill($false)",
                "IsCompletedSuccessfully",
                "Substring(0, 12)",
            )
        )
        and not any(
            needle in body
            for needle in (
                "cargo build",
                "windows-cross",
                "WineTestLauncher",
                "setup-windows-cross",
                "wine_job.py",
                "isolated-services.py",
            )
        )
        and body.index("test-agent-client-config-validator.ps1")
        < body.index("release_windows_artifact.py stage")
    )


class ReleaseSourceControls(unittest.TestCase):
    def test_actions_token_is_scoped_to_api_steps_not_native_validation(self):
        """
        Protects: Read-only Actions credentials never enter the reused binaries.
        Detects: Job-wide token or token forwarding in the native validator step.
        Needs: Actual release workflow; no credentials, network or services.
        Breadcrumb: Download/stage helper env; token-free native MCP environment.
        """
        source = RELEASE_SOURCE.read_text()
        body = re.search(r"(?ms)^  windows-x64:\r?\n(.*?)(?=^  linux:\r?$)", source)[1]

        def credential_is_scoped(body):
            header, steps = body.split("    steps:", 1)
            download = re.search(
                r"(?ms)^      - name: Reuse exact-commit accepted main CI "
                r"Windows artifact\r?\n"
                r"(.*?)(?=^      - )",
                steps,
            )[1]
            native = re.search(
                r"(?ms)^      - name: Verify reused native revision and configured "
                r"MCP before staging\r?\n"
                r"(.*?)(?=^      - )",
                steps,
            )[1]
            stage = re.search(
                r"(?ms)^      - name: Refresh accepted CI metadata and stage "
                r"identical artifact bytes\r?\n(.*?)(?=^      - )",
                steps,
            )[1]
            return (
                "GH_TOKEN" not in header
                and "          GH_TOKEN: ${{ github.token }}" in download
                and "GH_TOKEN" not in native
                and "github.token" not in native
                and "          GH_TOKEN: ${{ github.token }}" in stage
                and "release_windows_artifact.py stage" in stage
                and "test-agent-client-config-validator" not in stage
                and "Read-ReusedNativeVersion" not in stage
                and "agent-bus.exe" not in stage
                and body.count("GH_TOKEN: ${{ github.token }}") == 2
            )

        self.assertTrue(credential_is_scoped(body))
        self.assertFalse(
            credential_is_scoped(
                body.replace(
                    "    steps:",
                    "    env:\n      GH_TOKEN: ${{ github.token }}\n    steps:",
                    1,
                )
            )
        )
        self.assertFalse(
            credential_is_scoped(
                body.replace(
                    "        timeout-minutes: 10",
                    "        env:\n          GH_TOKEN: ${{ github.token }}\n"
                    "        timeout-minutes: 10",
                    1,
                )
            )
        )

    def test_reused_windows_ci_artifact_outlives_the_tag_window(self):
        """
        Protects: A v* tag cut up to 7 days after its main CI run can still reuse
        that run's agent-bus-windows-x64 artifact (the helper refuses expired ones).
        Detects: The Windows CI upload's retention-days dropping below 7, or the step
        losing it (agent-hub #193 review: 2 days broke release reuse after 2 days).
        Needs: Actual ci.yml; no credentials, network or services.
        Breadcrumb: ci.yml Windows Build And Unit Tests upload;
        release_windows_artifact.py expired check; release.yml windows-x64 download.
        """

        def retention(source):
            step = re.search(
                r"(?ms)^      - uses: actions/upload-artifact@\S+\r?\n"
                r"        with:\r?\n"
                r"          name: agent-bus-windows-x64\r?\n(.*?)(?=^      - |^  \S)",
                source,
            )
            if step is None:
                return None
            days = re.search(r"(?m)^          retention-days: (\d+)\r?$", step[1])
            return int(days[1]) if days else None

        source = (REPO_ROOT / ".github/workflows/ci.yml").read_text()
        days = retention(source)
        self.assertIsNotNone(days)
        self.assertGreaterEqual(days, 7)
        self.assertEqual(
            retention(
                source.replace(f"retention-days: {days}\n", "retention-days: 2\n", 1)
            ),
            2,
        )

    def test_candidate_source_contract_and_original_cross_rebuild_control(self):
        source = RELEASE_SOURCE.read_text()
        self.assertTrue(release_contract(source))
        legacy = (
            "  windows-x64:\n"
            "    runs-on: [self-hosted, Linux, X64, windows-cross]\n"
            "    steps:\n      - run: cargo build --release\n"
            "  linux:\n"
        )
        self.assertFalse(release_contract(legacy))
        for needle, replacement in (
            (
                "runs-on: windows-latest",
                "runs-on: [self-hosted, Linux, X64, windows-cross]",
            ),
            ("release_windows_artifact.py download", "cargo build"),
            ("test-agent-client-config-validator.ps1", "removed-native-validator"),
            ("10000 - $deadline.ElapsedMilliseconds", "100000"),
            ("Kill($false)", "Kill($true)"),
        ):
            with self.subTest(needle=needle):
                self.assertFalse(
                    release_contract(source.replace(needle, replacement, 1))
                )


@unittest.skipUnless(
    sys.platform == "win32" and shutil.which("pwsh") and shutil.which("rustup"),
    "Native Windows PowerShell and pinned Rust SDK required",
)
class NativeVersionClientControls(unittest.TestCase):
    def test_actual_version_client_success_failure_and_timeout_are_bounded(self):
        """
        Protects: Native revision subprocess clocks, private credentials and pipes.
        Detects: Unbounded waits, tree kills, output leakage or inherited GH token.
        Needs: Owned tiny native Rust fixture plus PowerShell; no network/backend.
        Breadcrumb: release.yml Read-ReusedNativeVersion original-handle checks.
        """
        with tempfile.TemporaryDirectory(
            prefix="release-version-native-control-"
        ) as temp:
            root = Path(temp)
            pin = re.search(
                r'(?m)^channel = "([^"]+)"',
                (REPO_ROOT / "rust-toolchain.toml").read_text(),
            )[1]
            rustc = subprocess.check_output(
                [shutil.which("rustup"), "which", "--toolchain", pin, "rustc"],
                text=True,
                timeout=20,
            ).strip()
            source = root / "version-fixture.rs"
            source.write_text(r"""
use std::{env, path::Path, process, thread, time::Duration};
fn main() {
    if env::args().skip(1).collect::<Vec<_>>() != ["--version"]
        || env::var_os("GH_TOKEN").is_some() { process::exit(9); }
    let exe = env::args().next().unwrap();
    let mode = Path::new(&exe).file_stem().unwrap().to_string_lossy();
    eprintln!("private version fixture stderr");
    if mode == "fixture-pipe-child" { thread::sleep(Duration::from_millis(10500)); }
    if mode == "fixture-pipes" {
        let child = Path::new(&exe).with_file_name("fixture-pipe-child.exe");
        process::Command::new(child).arg("--version").spawn().unwrap();
        thread::sleep(Duration::from_secs(9));
    }
    if mode == "fixture-slow" { thread::sleep(Duration::from_secs(30)); }
    if mode == "fixture-fail" { process::exit(7); }
    println!("fixture native revision aaaaaaaaaaaa");
}
""")
            executable = root / "fixture-ok.exe"
            compile_result = subprocess.run(
                [rustc, "--edition=2024", str(source), "-o", str(executable)],
                capture_output=True,
                text=True,
                timeout=60,
            )
            self.assertEqual(compile_result.returncode, 0, compile_result.stderr)
            for name in (
                "fixture-fail.exe",
                "fixture-slow.exe",
                "fixture-pipes.exe",
                "fixture-pipe-child.exe",
            ):
                shutil.copyfile(executable, root / name)
            workflow = RELEASE_SOURCE.read_text()
            body = re.search(
                r"(?ms)^          function Read-ReusedNativeVersion.*?"
                r"(?=^          foreach \(\$name)",
                workflow,
            )[0]
            script = root / "check-version.ps1"
            script.write_text(
                "$ErrorActionPreference='Stop'\n$env:GH_TOKEN='closed-fixture-token'\n"
                + body
                + "\nRead-ReusedNativeVersion $args[0]\n"
            )
            for name, expected in (
                ("fixture-ok.exe", 0),
                ("fixture-fail.exe", 1),
                ("fixture-slow.exe", 1),
                ("fixture-pipes.exe", 1),
            ):
                result = subprocess.run(
                    [
                        shutil.which("pwsh"),
                        "-NoLogo",
                        "-NoProfile",
                        "-File",
                        str(script),
                        str(root / name),
                    ],
                    capture_output=True,
                    text=True,
                    timeout=16,
                )
                self.assertEqual(result.returncode, expected, result.stderr)
                self.assertNotIn(
                    "private version fixture stderr", result.stdout + result.stderr
                )
                self.assertNotIn("closed-fixture-token", result.stdout + result.stderr)
                if expected == 0:
                    self.assertIn("aaaaaaaaaaaa", result.stdout)
                else:
                    self.assertIn("deadline/exit/pipe proof failed", result.stderr)

            # Preserve the observed pre-fix acceptance boundary: the original
            # exits at9s and inherited pipes close at10.5s, within cleanup's2s.
            # Omitting only the final total-clock predicate must accept late.
            original = root / "check-version-before-final-clock.ps1"
            original.write_text(
                script.read_text().replace(
                    " -or $deadline.ElapsedMilliseconds -ge 10000",
                    "",
                    1,
                )
            )
            late = subprocess.run(
                [
                    shutil.which("pwsh"),
                    "-NoLogo",
                    "-NoProfile",
                    "-File",
                    str(original),
                    str(root / "fixture-pipes.exe"),
                ],
                capture_output=True,
                text=True,
                timeout=16,
            )
            self.assertEqual(late.returncode, 0, late.stderr)
            self.assertIn("aaaaaaaaaaaa", late.stdout)


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("--repo-root", type=Path, default=REPO_ROOT)
    parser.add_argument("--release-workflow", type=Path)
    args, remaining = parser.parse_known_args()
    REPO_ROOT = args.repo_root
    RELEASE_SOURCE = (
        args.release_workflow or REPO_ROOT / ".github/workflows/release.yml"
    )
    unittest.main(argv=[__file__, *remaining])
