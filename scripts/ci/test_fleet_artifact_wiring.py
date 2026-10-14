"""CI artifacts go to the fleet artifact store; GitHub uploads are failure-only.

Protects: the CI release-build and Windows jobs store binaries through the vendored
fleet-artifact action (required, endpoint and key wired), smoke-functional fetches the
Windows binaries from it, any GitHub upload runs only under failure() with a 3-day
retention, and the action's own controls pass.
Detects: a return to actions/upload-artifact on success (the GitHub artifact quota
is exhausted), a call without endpoint or key, a download-artifact left behind, or a
failing control in the vendored action.
Needs: the tracked workflow and vendored action; stdlib and python3, no network.
Breadcrumb: .github/actions/fleet-artifact/action.yml, .github/workflows/ci.yml.
"""

from pathlib import Path
import re
import subprocess
import sys
import unittest

ROOT = Path(__file__).resolve().parents[2]
CI = (ROOT / ".github" / "workflows" / "ci.yml").read_text(encoding="utf-8")
ACTION = ROOT / ".github" / "actions" / "fleet-artifact"


def _steps(text):
    return re.split(r"\n(?=\s*- (?:name|uses|id):)", text)


class FleetArtifactWiring(unittest.TestCase):
    def test_fleet_steps_carry_endpoint_and_key(self):
        fleet = [s for s in _steps(CI) if "uses: ./.github/actions/fleet-artifact" in s]
        self.assertEqual(len(fleet), 4)
        for step in fleet:
            self.assertIn("endpoint: ${{ vars.FLEET_ARTIFACT_ENDPOINT }}", step)
            self.assertIn("key: ${{ secrets.FLEET_ARTIFACT_KEY }}", step)
            self.assertNotIn('required: "false"', step)

    def test_release_binaries_are_verified_by_a_download(self):
        self.assertIn('test "$UPLOADED" = "$DOWNLOADED"', CI)

    def test_windows_binaries_move_through_the_fleet_store(self):
        self.assertNotIn("actions/download-artifact", CI)
        self.assertIn("name: agent-bus-windows-x64\n          job: windows", CI)

    def test_github_uploads_are_failure_only_with_short_retention(self):
        uploads = [s for s in _steps(CI) if "actions/upload-artifact@" in s]
        self.assertTrue(uploads)
        for step in uploads:
            self.assertIn("if: failure()", step)
            self.assertIn("retention-days: 3", step)

    def test_vendored_action_controls_pass(self):
        done = subprocess.run(
            [sys.executable, "-B", "-m", "unittest", "-q", "test_fleet_artifact"],
            cwd=ACTION,
            capture_output=True,
            text=True,
            timeout=300,
            check=False,
        )
        self.assertEqual(done.returncode, 0, done.stderr[-2000:])


if __name__ == "__main__":
    unittest.main()
