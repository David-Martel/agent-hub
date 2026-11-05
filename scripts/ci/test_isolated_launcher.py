"""The isolated test launcher must not hand live bearer tokens to the tests.

Why this exists: `Settings::validate` refuses AGENT_BUS_PREVIOUS_AUTH_TOKEN without
AGENT_BUS_AUTH_TOKEN (docs/hub-token-rotation.md). The launcher
(scripts/test-agent-bus-isolated.sh) already strips the current token, so a host
that exports both during a rotation would give the tests a previous token alone,
an invalid configuration, and a retired credential would also reach test code.
The launcher is exercised with a stub `cargo` that records what it inherited.
"""

import os
import shutil
import subprocess
import tempfile
import unittest
from pathlib import Path

LAUNCHER = Path(__file__).resolve().parents[1] / "test-agent-bus-isolated.sh"
BASH = shutil.which("bash")


@unittest.skipUnless(BASH, "bash is required to run the launcher")
class IsolatedLauncherTests(unittest.TestCase):
    def inherited(self, extra_env):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            record = root / "seen.txt"
            stub = root / "cargo"
            report = (
                "printf '%s|%s' "
                '"${AGENT_BUS_AUTH_TOKEN-unset}" '
                '"${AGENT_BUS_PREVIOUS_AUTH_TOKEN-unset}" '
                f"> '{record.as_posix()}'\n"
            )
            stub.write_text("#!/bin/sh\n" + report, encoding="utf-8", newline="\n")
            stub.chmod(0o755)
            env = dict(os.environ)
            env["PATH"] = f"{root.as_posix()}{os.pathsep}{env.get('PATH', '')}"
            env.update(extra_env)
            subprocess.run(
                [BASH, str(LAUNCHER)],
                env=env,
                check=True,
                capture_output=True,
                timeout=60,
            )
            return record.read_text(encoding="utf-8")

    def test_current_and_previous_tokens_are_not_inherited(self):
        seen = self.inherited(
            {
                "AGENT_BUS_AUTH_TOKEN": "current-synthetic-token",
                "AGENT_BUS_PREVIOUS_AUTH_TOKEN": "previous-synthetic-token",
            }
        )
        self.assertEqual(seen, "unset|unset")

    def test_a_previous_token_alone_is_not_inherited(self):
        seen = self.inherited(
            {"AGENT_BUS_PREVIOUS_AUTH_TOKEN": "previous-synthetic-token"}
        )
        self.assertEqual(seen, "unset|unset")


if __name__ == "__main__":
    unittest.main()
