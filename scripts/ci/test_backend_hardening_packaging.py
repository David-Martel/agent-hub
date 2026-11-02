"""Run the backend-hardening packaging fixture from the existing Unit Tests step.

Protects: the Redis AOF preflight, the PostgreSQL dump and their root installer
(packaging/README.md, "Backend hardening").
Detects: any failing case in packaging/tests/backend-hardening-test.sh, which
exits non-zero on the first wrong verdict, exit code or recorded command.
Needs: bash on Linux. Every privileged command goes to a recorder that runs
nothing, so this never needs root, a live Redis or a live PostgreSQL.
Breadcrumb: .github/workflows/ci.yml "Unit Tests" runs
`python3 -B -m unittest discover -s scripts/ci`. Routing the fixture through
that step keeps ci.yml byte-identical, so the reviewed CI producer hash in
.github/workflows/dependabot-automerge.yml stays valid.
"""

import shutil
import subprocess
import sys
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
FIXTURE = ROOT / "packaging" / "tests" / "backend-hardening-test.sh"


@unittest.skipUnless(
    sys.platform.startswith("linux"),
    "backend hardening installs systemd units; Linux only",
)
class BackendHardeningPackagingTests(unittest.TestCase):
    def test_fixture_passes(self):
        bash = shutil.which("bash")
        # A Linux runner without bash is a broken runner, not a reason to skip.
        self.assertIsNotNone(bash, "bash is required on Linux CI runners")
        self.assertTrue(FIXTURE.is_file(), f"missing fixture {FIXTURE}")
        result = subprocess.run(
            [bash, str(FIXTURE)],
            cwd=ROOT,
            capture_output=True,
            text=True,
            timeout=300,
            check=False,
        )
        self.assertEqual(
            result.returncode,
            0,
            f"stdout:\n{result.stdout[-4000:]}\nstderr:\n{result.stderr[-4000:]}",
        )
        # Positive control: the fixture prints a per-case PASS line; zero cases
        # passing with exit 0 would mean it ran nothing.
        self.assertIn("PASS", result.stdout + result.stderr)


if __name__ == "__main__":
    unittest.main()
