"""Gate behaviour of the vendored nextest-ci action used by the Unit Tests job."""

import contextlib
import importlib.util
import io
import os
import shutil
import stat
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
ACTION_DIR = ROOT / ".github/actions/nextest-ci"
SPEC = importlib.util.spec_from_file_location(
    "nextest_flaky_summary", ACTION_DIR / "nextest_flaky_summary.py"
)
SUMMARY = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(SUMMARY)

FLAKY_JUNIT = """<testsuites><testsuite name="agent-bus-core">
<testcase classname="agent-bus-core::settings" name="steady"/>
<testcase classname="agent-bus-core::settings" name="racy">
<flakyFailure message="x"/></testcase>
</testsuite></testsuites>"""
CLEAN_JUNIT = """<testsuites><testsuite name="agent-bus-core">
<testcase classname="agent-bus-core::settings" name="steady"/>
</testsuite></testsuites>"""
POSIX_SHELL = sys.platform != "win32" and shutil.which("bash") is not None


def step_script(marker):
    """Return the dedented `run:` script of the action step containing marker."""
    steps, current = [], []
    for line in (ACTION_DIR / "action.yml").read_text().splitlines():
        if line.startswith("    - "):
            steps.append(current)
            current = []
        current.append(line)
    steps.append(current)
    (step,) = [s for s in steps if any(marker in line for line in s)]
    start = next(i for i, line in enumerate(step) if line.strip() == "run: |") + 1
    body = [
        line[8:]
        for line in step[start:]
        if line.startswith("        ") or not line.strip()
    ]
    script = "\n".join(body) + "\n"
    script = script.replace("${{ github.action_path }}", str(ACTION_DIR))
    return script.replace("python3 -I", f'"{sys.executable}" -I')


class NextestCiActionTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.summary = self.root / "summary.md"

    def junit(self, text):
        path = self.root / "junit.xml"
        path.write_text(text)
        return path

    def summarize(self, *argv):
        out = io.StringIO()
        with contextlib.redirect_stdout(out):
            rc = SUMMARY.main(list(argv))
        return rc, out.getvalue()

    # Protects: Unit Tests staying as strict as the retry-free `cargo test` it replaced.
    # Detects: a test that passed only on retry leaving the job green or unannotated.
    # Needs: the vendored summary script and a JUnit fixture; no cargo or services.
    # Breadcrumb: .github/actions/nextest-ci/nextest_flaky_summary.py, flaky branch.
    def test_flaky_report_fails_by_default(self):
        rc, printed = self.summarize(str(self.junit(FLAKY_JUNIT)), str(self.summary))
        self.assertEqual(rc, 1)
        self.assertIn("agent-bus-core::settings::racy", self.summary.read_text())
        self.assertIn(
            "::error title=flaky test::agent-bus-core::settings::racy", printed
        )
        self.assertNotIn("steady", self.summary.read_text())

    # Protects: a run with no flaky test staying green under the fail-on-flaky default.
    # Detects: the summary step failing a clean run, which would make the gate unusable.
    # Needs: the vendored summary script and a JUnit fixture; no cargo or services.
    # Breadcrumb: .github/actions/nextest-ci/nextest_flaky_summary.py, clean branch.
    def test_clean_report_passes(self):
        rc, _ = self.summarize(
            "--require", str(self.junit(CLEAN_JUNIT)), str(self.summary)
        )
        self.assertEqual(rc, 0)
        self.assertIn(
            "None: every test passed on its first attempt.", self.summary.read_text()
        )

    # Protects: the explicit lenient opt-out, which reports flaky tests without failing.
    # Detects: --allow-flaky still failing the job, or dropping the flaky test's report.
    # Needs: the vendored summary script and a JUnit fixture; no cargo or services.
    # Breadcrumb: .github/actions/nextest-ci/nextest_flaky_summary.py --allow-flaky.
    def test_allow_flaky_reports_without_failing(self):
        rc, printed = self.summarize(
            "--allow-flaky", str(self.junit(FLAKY_JUNIT)), str(self.summary)
        )
        self.assertEqual(rc, 0)
        self.assertIn(
            "::warning title=flaky test::agent-bus-core::settings::racy", printed
        )

    # Protects: the action's fail-on-flaky input reaching the summary script as written.
    # Detects: "false" not opting out, "true" not failing, or a typo silently accepted.
    # Needs: bash and the action's own summary-step text; no cargo, network or services.
    # Breadcrumb: .github/actions/nextest-ci/action.yml summary step, FAIL_ON_FLAKY.
    @unittest.skipUnless(
        POSIX_SHELL, "the action's steps are bash; covered on the Linux job"
    )
    def test_action_fail_on_flaky_input_controls_the_exit(self):
        script = step_script("FAIL_ON_FLAKY")
        junit = self.junit(FLAKY_JUNIT)
        for value, expected in (("true", 1), ("false", 0), ("yes", 2)):
            env = dict(
                os.environ,
                JUNIT=str(junit),
                RUN_OUTCOME="success",
                FAIL_ON_FLAKY=value,
                GITHUB_STEP_SUMMARY=str(self.summary),
            )
            result = subprocess.run(
                ["bash", "-e", "-c", script],
                env=env,
                capture_output=True,
                text=True,
                check=False,
            )
            self.assertEqual(
                result.returncode, expected, (value, result.stdout, result.stderr)
            )

    # Protects: tests running with the runner's AGENT_BUS_* routing variables removed.
    # Detects: unset-env not reaching `cargo nextest run`, so a test inherits a live hub
    # route; the second run is the control showing the fake cargo sees the variable.
    # Needs: bash and a fake `cargo` on PATH; no Rust toolchain, network or services.
    # Breadcrumb: .github/actions/nextest-ci/action.yml run step, unset-env input.
    @unittest.skipUnless(
        POSIX_SHELL, "the action's steps are bash; covered on the Linux job"
    )
    def test_action_unset_env_removes_the_variable(self):
        fake = self.root / "bin" / "cargo"
        fake.parent.mkdir()
        fake.write_text(
            '#!/bin/sh\necho "candidates=${AGENT_BUS_SERVER_CANDIDATES-unset}"\n'
        )
        fake.chmod(fake.stat().st_mode | stat.S_IEXEC)
        script = step_script("UNSET_ENV")
        seen = {}
        for unset in ("AGENT_BUS_SERVER_CANDIDATES", ""):
            env = dict(
                os.environ,
                PATH=f"{fake.parent}{os.pathsep}{os.environ['PATH']}",
                AGENT_BUS_SERVER_CANDIDATES="garbage",
                UNSET_ENV=unset,
                NEXTEST_ARGS="",
                LOG_FILE="",
            )
            result = subprocess.run(
                ["bash", "-e", "-c", script],
                env=env,
                capture_output=True,
                text=True,
                check=True,
            )
            seen[unset] = result.stdout.strip()
        self.assertEqual(seen["AGENT_BUS_SERVER_CANDIDATES"], "candidates=unset")
        self.assertEqual(seen[""], "candidates=garbage")


if __name__ == "__main__":
    unittest.main()
