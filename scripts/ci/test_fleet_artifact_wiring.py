"""Release binaries from CI go to the fleet artifact store.

Protects: the CI release-build job uploads through the vendored fleet-artifact
action, with the endpoint variable and key secret, and proves the object by
downloading it back.
Detects: a return to actions/upload-artifact for agent-bus-<matrix.name> (the
GitHub artifact quota is exhausted), a call without endpoint or key, or a verify
step that stops comparing digests.
Needs: the tracked workflow and vendored action text only; stdlib, no network.
Breadcrumb: .github/actions/fleet-artifact/action.yml, .github/workflows/ci.yml.
"""

from pathlib import Path
import unittest

ROOT = Path(__file__).resolve().parents[2]
CI = (ROOT / ".github" / "workflows" / "ci.yml").read_text(encoding="utf-8")
ACTION = ROOT / ".github" / "actions" / "fleet-artifact"


class FleetArtifactWiring(unittest.TestCase):
    def test_release_binaries_use_the_fleet_store(self):
        start = CI.index("Store release binaries on the fleet artifact store")
        block = CI[start : CI.index("  smoke-functional:", start)]
        self.assertIn("uses: ./.github/actions/fleet-artifact", block)
        endpoint = "endpoint: ${{ vars.FLEET_ARTIFACT_ENDPOINT }}"
        self.assertEqual(block.count(endpoint), 2)
        self.assertEqual(block.count("key: ${{ secrets.FLEET_ARTIFACT_KEY }}"), 2)
        self.assertIn("mode: download", block)
        self.assertIn('test "$UPLOADED" = "$DOWNLOADED"', block)
        legacy = "name: agent-bus-${{ matrix.name }}\n          if-no-files-found"
        self.assertNotIn(legacy, CI)

    def test_vendored_action_is_present_and_passes_the_key_via_env(self):
        self.assertTrue((ACTION / "fleet_artifact.py").is_file())
        manifest = (ACTION / "action.yml").read_text(encoding="utf-8")
        self.assertIn("FA_KEY: ${{ inputs.key }}", manifest)


if __name__ == "__main__":
    unittest.main()
