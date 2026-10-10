"""Execute the exact workflow gate against closed, credential-free API fixtures.

No gh command or merge is executed. Required job names are an independent oracle
for the reviewed CI producer, not inferred from the gate under test.
"""

from __future__ import annotations

import argparse
import base64
import hashlib
import contextlib
import copy
import io
import json
import os
from pathlib import Path
import re
import subprocess
import tempfile
import textwrap
import unittest
from unittest.mock import patch

REPO = "David-Martel/agent-hub"
HEAD = "a" * 40
MERGE = "c" * 40
PRODUCER = b""
FUTURE_PRODUCER = b""
REVIEWED_HASH = "70b668a1c1365e32806933e2d16b190f2c06f1bb97a1d0afcba20bbe8e9fb526"
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

MANUAL = "Windows Native Validation (manual)"
SKIPS = {
    "CI": {
        "path": ".github/workflows/ci.yml",
        "source_sha256": (
            "70b668a1c1365e32806933e2d16b190f2c06f1bb97a1d0afcba20bbe8e9fb526"
        ),
        "job_name": "Windows Native Validation (manual)",
        "if_expression": "github.event_name == 'workflow_dispatch'",
        "event": "pull_request",
    }
}


def declared_json(text: str, key: str) -> object:
    matched = re.search(r"(?m)^      " + key + r": '(.+)'$", text)
    if matched is None:
        raise AssertionError("missing trusted workflow declaration: " + key)
    return json.loads(matched[1].replace("''", "'"))


def gate_source(workflow: Path) -> str:
    source = workflow.read_text(encoding="utf-8")
    matched = re.search(
        r"(?m)^        python3 - <<'PYGATE'\n(.*?)^        PYGATE$", source, re.S
    )
    if matched is None:
        raise AssertionError("exact workflow PYGATE block missing")
    return textwrap.dedent(matched[1])


def evaluate(code: str, scenario: str) -> tuple[str, list[str]]:
    run = dict(
        id=1,
        workflow_id=9,
        head_sha=HEAD,
        event="pull_request",
        run_number=1,
        run_attempt=1,
        status="completed",
        conclusion="success",
        path=".github/workflows/ci.yml",
        name="CI",
    )
    jobs = [
        dict(
            id=i + 1,
            name=name,
            head_sha=HEAD,
            run_id=1,
            run_attempt=1,
            status="completed",
            conclusion="success",
        )
        for i, name in enumerate(JOBS)
    ]
    windows = next(job for job in jobs if job["name"] == "Windows Build And Unit Tests")
    required: object = {
        "CI": {
            "path": ".github/workflows/ci.yml",
            "source_sha256": REVIEWED_HASH,
            "required_jobs": JOBS,
            "auxiliary_sources": {},
        }
    }
    reviewed_skips = copy.deepcopy(SKIPS)
    jobs.append(
        dict(
            id=97,
            name=MANUAL,
            head_sha=HEAD,
            run_id=1,
            run_attempt=1,
            status="completed",
            conclusion="skipped",
        )
    )
    manual = jobs[-1]
    source = PRODUCER
    merge_source = PRODUCER
    merge = MERGE if scenario in {"merge_success", "merge_changed_source"} else ""
    if scenario == "source_changed":
        source += b"\n# changed producer fixture\n"
    if scenario == "merge_changed_source":
        merge_source += b"\n# changed merge producer fixture\n"
    if scenario in {"future_producer", "future_hash_only"}:
        source = FUTURE_PRODUCER
        jobs.append(
            dict(
                id=98,
                name="Additional Producer Validation",
                head_sha=HEAD,
                run_id=1,
                run_attempt=1,
                status="completed",
                conclusion="success",
            )
        )
    if scenario == "future_hash_only":
        required["CI"]["source_sha256"] = hashlib.sha256(source).hexdigest()
        reviewed_skips["CI"]["source_sha256"] = hashlib.sha256(source).hexdigest()
    if scenario == "windows_skipped":
        windows["conclusion"] = "skipped"
    if scenario == "windows_failed":
        windows["conclusion"] = "failure"
    if scenario == "windows_pending":
        windows.update(status="queued", conclusion=None)
    if scenario == "run_pending":
        run.update(status="in_progress", conclusion=None)
    if scenario == "run_failed":
        run["conclusion"] = "failure"
    if scenario == "wrong_head":
        windows["head_sha"] = "b" * 40
    if scenario == "wrong_attempt":
        windows["run_attempt"] = 2
    if scenario == "duplicate_names":
        jobs[-1]["name"] = jobs[0]["name"]
    if scenario == "duplicate_ids":
        jobs[-1]["id"] = jobs[0]["id"]
    if scenario == "windows_missing":
        jobs.remove(windows)
    if scenario == "smoke_missing":
        jobs = [j for j in jobs if j["name"] != "CLI And HTTP Smoke"]
    if scenario == "matrix_missing":
        jobs = [j for j in jobs if j["name"] != "Release Build (linux-arm64-2)"]
    if scenario == "untrusted_skip":
        jobs.append(
            dict(
                id=99,
                name="Unproven optional job",
                head_sha=HEAD,
                run_id=1,
                run_attempt=1,
                status="completed",
                conclusion="skipped",
            )
        )
    if scenario == "extra_success":
        jobs.append(
            dict(
                id=99,
                name="Additional successful gate",
                head_sha=HEAD,
                run_id=1,
                run_attempt=1,
                status="completed",
                conclusion="success",
            )
        )
    if scenario == "inventory_absent":
        required = {}
    if scenario == "inventory_empty":
        required["CI"]["required_jobs"] = []
    if scenario == "inventory_duplicate":
        required["CI"]["required_jobs"] = JOBS + [JOBS[0]]
    if scenario == "inventory_not_mapping":
        required = JOBS
    if scenario == "success_without_manual":
        jobs.remove(manual)
    if scenario == "manual_success":
        manual["conclusion"] = "success"
    if scenario == "manual_failed":
        manual["conclusion"] = "failure"
    if scenario == "manual_wrong_head":
        manual["head_sha"] = "b" * 40
    if scenario == "manual_wrong_attempt":
        manual["run_attempt"] = 2
    if scenario == "manual_wrong_run":
        manual["run_id"] = 2
    if scenario == "manual_duplicate":
        jobs.append(dict(manual, id=96))
    if scenario == "manual_wrong_policy_job":
        reviewed_skips["CI"]["job_name"] = "Unproven job"
    if scenario == "manual_wrong_policy_path":
        reviewed_skips["CI"]["path"] = ".github/workflows/other.yml"
    if scenario == "manual_wrong_policy_hash":
        reviewed_skips["CI"]["source_sha256"] = "0" * 64
    if scenario == "manual_wrong_policy_event":
        reviewed_skips["CI"]["event"] = "push"
    if scenario == "manual_wrong_policy_expression":
        reviewed_skips["CI"]["if_expression"] = "always()"
    if scenario == "manual_unreviewed":
        reviewed_skips = {}
    if scenario == "history_missing":
        jobs = [j for j in jobs if j["name"] != "History Catalog Migration"]
    if scenario == "history_skipped":
        next(j for j in jobs if j["name"] == "History Catalog Migration")[
            "conclusion"
        ] = "skipped"
    calls: list[str] = []

    def fake_gh(argv: list[str], **kwargs: object) -> str:
        if argv[:2] != ["gh", "api"] or len(argv) != 3:
            raise AssertionError("non-read or unknown command attempted")
        path = argv[2]
        calls.append(path)
        if path.startswith(f"repos/{REPO}/actions/runs?head_sha="):
            response_run = copy.deepcopy(run)
            if scenario == "snapshot_race" and calls.count(path) > 1:
                response_run["run_attempt"] = 2
            return json.dumps(dict(total_count=1, workflow_runs=[response_run]))
        if path == f"repos/{REPO}/actions/runs/1":
            response_run = copy.deepcopy(run)
            if scenario == "attempt_race":
                response_run["run_attempt"] = 2
            return json.dumps(response_run)
        if path.startswith(f"repos/{REPO}/actions/runs/1/attempts/1/jobs?"):
            return json.dumps(dict(total_count=len(jobs), jobs=jobs))
        if path.startswith(f"repos/{REPO}/contents/.github/workflows/ci.yml?ref="):
            ref = path.rsplit("=", 1)[1]
            if ref not in {HEAD, MERGE}:
                raise AssertionError("unexpected source ref")
            raw = merge_source if ref == MERGE else source
            blob = hashlib.sha1(
                b"blob " + str(len(raw)).encode() + b"\0" + raw
            ).hexdigest()
            response = dict(
                type="file",
                path=".github/workflows/ci.yml",
                encoding="base64",
                size=len(raw),
                sha=blob,
                content=base64.b64encode(raw).decode(),
            )
            if scenario == "blob_mismatch":
                response["sha"] = "0" * 40
            if scenario == "size_mismatch":
                response["size"] += 1
            if scenario == "size_boolean":
                response["size"] = True
            if scenario == "path_mismatch":
                response["path"] = ".github/workflows/other.yml"
            if scenario == "encoding_mismatch":
                response["encoding"] = "none"
            if scenario == "type_mismatch":
                response["type"] = "symlink"
            if scenario == "invalid_base64":
                response["content"] = "!!"
            return json.dumps(response)
        raise AssertionError(f"unexpected mocked API route: {path}")

    with tempfile.TemporaryDirectory(prefix="agenthub-admission-") as directory:
        output = Path(directory) / "output.txt"
        environment = dict(
            GITHUB_REPOSITORY=REPO,
            HEAD_SHA=HEAD,
            MERGE_SHA=merge,
            EXPECTED_WORKFLOWS_JSON='["CI"]',
            CONDITIONAL_WORKFLOWS_JSON="{}",
            REVIEWED_JOB_INVENTORY_JSON=json.dumps(required),
            REVIEWED_SKIP_JOBS_JSON=json.dumps(reviewed_skips),
            CHANGED_FILES_JSON='["Cargo.toml"]',
            GITHUB_WORKFLOW_REF=f"{REPO}/.github/workflows/dependabot-automerge.yml@refs/heads/main",
            GITHUB_OUTPUT=str(output),
        )
        with (
            patch.dict(os.environ, environment, clear=True),
            patch.object(subprocess, "check_output", fake_gh),
            contextlib.redirect_stdout(io.StringIO()),
        ):
            exec(
                compile(code, "exact-workflow-PYGATE", "exec"), {"__name__": "__gate__"}
            )
        values = dict(line.split("=", 1) for line in output.read_text().splitlines())
        return values["verdict"], json.loads(values["details"])


class AdmissionTests(unittest.TestCase):
    code: str
    workflow_text: str

    @classmethod
    def setUpClass(cls) -> None:
        global PRODUCER, FUTURE_PRODUCER
        root = Path(__file__).resolve().parents[2]
        if not PRODUCER:
            PRODUCER = (root / ".github/workflows/ci.yml").read_bytes()
        if not FUTURE_PRODUCER:
            FUTURE_PRODUCER = PRODUCER + (
                b"\n  additional-validation:\n"
                b"    name: Additional Producer Validation\n"
                b"    runs-on: ubuntu-latest\n    steps:\n      - run: true\n"
            )
        if not hasattr(cls, "workflow_text"):
            cls.workflow_text = (
                root / ".github/workflows/dependabot-automerge.yml"
            ).read_text(encoding="utf-8")
        if not hasattr(cls, "code"):
            cls.code = gate_source(root / ".github/workflows/dependabot-automerge.yml")

    def test_trusted_policy_inventory_matches_independent_producer_oracle(self) -> None:
        policy = declared_json(self.workflow_text, "REVIEWED_JOB_INVENTORY_JSON")
        self.assertEqual(
            policy,
            {
                "CI": {
                    "path": ".github/workflows/ci.yml",
                    "source_sha256": REVIEWED_HASH,
                    "required_jobs": JOBS,
                    "auxiliary_sources": {},
                }
            },
        )
        self.assertEqual(
            declared_json(self.workflow_text, "REVIEWED_SKIP_JOBS_JSON"), SKIPS
        )
        self.assertEqual(len(JOBS), 16)
        self.assertNotIn(MANUAL, JOBS)
        self.assertEqual(hashlib.sha256(PRODUCER).hexdigest(), REVIEWED_HASH)
        self.assertNotEqual(hashlib.sha256(FUTURE_PRODUCER).hexdigest(), REVIEWED_HASH)
        self.assertIn(b"History Catalog Migration", PRODUCER)
        self.assertRegex(
            PRODUCER.decode(), r"(?m)^    name: Windows Native Validation \(manual\)$"
        )
        self.assertIn(b"    if: github.event_name == 'workflow_dispatch'", PRODUCER)


def add_case(name: str, expected: str) -> None:
    def test(self: AdmissionTests) -> None:
        verdict, details = evaluate(self.code, name)
        self.assertEqual(verdict, expected, details)
        if expected == "pass":
            self.assertEqual(details, [])
        else:
            self.assertTrue(details, "refusal must retain actionable evidence")

    setattr(AdmissionTests, f"test_{name}", test)


for case in ("success", "merge_success", "success_without_manual"):
    add_case(case, "pass")
for case in ("windows_pending", "run_pending"):
    add_case(case, "pending")
for case in (
    "windows_skipped",
    "windows_failed",
    "run_failed",
    "wrong_head",
    "wrong_attempt",
    "duplicate_names",
    "duplicate_ids",
    "windows_missing",
    "smoke_missing",
    "matrix_missing",
    "untrusted_skip",
    "extra_success",
    "source_changed",
    "merge_changed_source",
    "future_producer",
    "future_hash_only",
    "blob_mismatch",
    "size_mismatch",
    "size_boolean",
    "path_mismatch",
    "encoding_mismatch",
    "type_mismatch",
    "invalid_base64",
    "inventory_absent",
    "inventory_empty",
    "inventory_duplicate",
    "inventory_not_mapping",
    "snapshot_race",
    "attempt_race",
    "manual_success",
    "manual_failed",
    "manual_wrong_head",
    "manual_wrong_attempt",
    "manual_wrong_run",
    "manual_duplicate",
    "manual_wrong_policy_job",
    "manual_wrong_policy_path",
    "manual_wrong_policy_hash",
    "manual_wrong_policy_event",
    "manual_wrong_policy_expression",
    "manual_unreviewed",
    "history_missing",
    "history_skipped",
):
    add_case(case, "fail")


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument(
        "--workflow",
        type=Path,
        default=Path(__file__).resolve().parents[2]
        / ".github/workflows/dependabot-automerge.yml",
    )
    parser.add_argument(
        "--producer",
        type=Path,
        default=Path(__file__).resolve().parents[2] / ".github/workflows/ci.yml",
    )
    parser.add_argument("--future-producer", type=Path)
    parser.add_argument("--only", nargs="*")
    args = parser.parse_args()
    PRODUCER = args.producer.read_bytes()
    FUTURE_PRODUCER = (
        args.future_producer.read_bytes()
        if args.future_producer
        else PRODUCER
        + (
            b"\n  additional-validation:\n    name: Additional Producer Validation\n"
            b"    runs-on: ubuntu-latest\n    steps:\n      - run: true\n"
        )
    )
    AdmissionTests.workflow_text = args.workflow.read_text(encoding="utf-8")
    AdmissionTests.code = gate_source(args.workflow)
    result = unittest.TextTestRunner(verbosity=2).run(
        unittest.TestSuite(AdmissionTests("test_" + name) for name in args.only)
        if args.only
        else unittest.defaultTestLoader.loadTestsFromTestCase(AdmissionTests)
    )
    raise SystemExit(0 if result.wasSuccessful() else 1)
