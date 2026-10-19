"""Unit tests for verify_deploy.py and deploy-agentbus-cloud.yml invariants.

Protects: the post-deploy readback fails when the newest deployment is not the
  merged commit, and the deploy workflow isolates npm setup and runner state.
Detects:  a readback that passes on a stale deployment (the mismatch tests are
  the positive control), a deploy job routed to persistent PR runners, or
  setup-npm.sh combined into the same step as npm ci.
Needs:    Python stdlib only (runs under both unittest and pytest), no network.
  Run (from cloud/agentbus):
    python3 -B -m unittest discover -s tools -p "test_verify_deploy.py"
Breadcrumb: .github/workflows/deploy-agentbus-cloud.yml
"""

from __future__ import annotations

import importlib.util
import os
import re
import sys
import unittest
from pathlib import Path
from unittest.mock import patch

TOOLS_DIR = Path(__file__).resolve().parent
REPO_ROOT = TOOLS_DIR.parents[2]
if str(TOOLS_DIR) not in sys.path:
    sys.path.insert(0, str(TOOLS_DIR))

SPEC = importlib.util.spec_from_file_location(
    "verify_deploy", TOOLS_DIR / "verify_deploy.py"
)
assert SPEC is not None and SPEC.loader is not None
vd = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(vd)

SHA = "a" * 40


def deployment(created: str, message: str, versions=None):
    return {
        "id": "d-" + created,
        "created_on": created,
        "annotations": {"workers/message": message},
        "versions": versions
        if versions is not None
        else [{"version_id": "v-" + created, "percentage": 100}],
    }


def payload(*deployments):
    return {"result": {"deployments": list(deployments)}}


class VerifyDeployTests(unittest.TestCase):
    def test_newest_deployment_naming_the_commit_passes(self):
        old = deployment("2026-10-15T00:00:00Z", "Source" + "b" * 40)
        new = deployment("2026-10-19T00:00:00Z", "Source" + SHA)
        self.assertEqual(
            vd.newest_deployment_names_commit(payload(old, new), SHA),
            "v-2026-10-19T00:00:00Z",
        )

    def test_stale_newest_deployment_fails_even_if_an_older_one_matches(self):
        older_match = deployment("2026-10-15T00:00:00Z", "Source" + SHA)
        newer_other = deployment("2026-10-19T00:00:00Z", "Source" + "b" * 40)
        with self.assertRaises(vd.DeployMismatch):
            vd.newest_deployment_names_commit(payload(older_match, newer_other), SHA)

    def test_split_rollout_fails(self):
        split = deployment(
            "2026-10-19T00:00:00Z",
            "Source" + SHA,
            [
                {"version_id": "x", "percentage": 50},
                {"version_id": "y", "percentage": 50},
            ],
        )
        with self.assertRaises(vd.DeployMismatch):
            vd.newest_deployment_names_commit(payload(split), SHA)

    def test_no_deployments_fails(self):
        with self.assertRaises(vd.DeployMismatch):
            vd.newest_deployment_names_commit(payload(), SHA)

    def test_missing_token_is_a_usage_error(self):
        env = {k: v for k, v in os.environ.items() if k != "CLOUDFLARE_API_TOKEN"}
        with patch.dict(os.environ, env, clear=True):
            self.assertEqual(vd.main(["--commit", SHA, "--account-id", "acct"]), 2)

    def test_workflow_isolates_runner_and_npm_bootstrap_step(self):
        workflow = (
            REPO_ROOT / ".github/workflows/deploy-agentbus-cloud.yml"
        ).read_text(encoding="utf-8")
        active = "\n".join(
            line for line in workflow.splitlines() if not line.lstrip().startswith("#")
        )
        self.assertIn("runs-on: ubuntu-latest", active)
        self.assertNotIn("vigil1", active)
        self.assertNotIn("self-hosted", active)
        steps = re.split(r"(?m)^      - ", active)[1:]
        setup_idx = next(
            (i for i, step in enumerate(steps) if "setup-npm.sh" in step), None
        )
        install_idx = next(
            (i for i, step in enumerate(steps) if "npm ci" in step), None
        )
        self.assertIsNotNone(setup_idx, "setup-npm.sh step must be present")
        self.assertIsNotNone(install_idx, "npm ci step must be present")
        self.assertLess(
            setup_idx,
            install_idx,
            "setup-npm.sh must run in a preceding step before npm ci",
        )
        self.assertNotIn("npm ci", steps[setup_idx])
        self.assertIn(
            'python3 -B -m unittest discover -s tools -p "test_verify_deploy.py"',
            steps[install_idx],
        )


if __name__ == "__main__":
    unittest.main()
