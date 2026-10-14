"""Detect first-error masking and evidence loss using a synthetic launcher."""

import os
import json
from pathlib import Path
import shutil
import re
import subprocess
import sys
import tempfile
import unittest

SCRIPTS = Path(__file__).resolve().parents[1]
PWSH = shutil.which("pwsh")


@unittest.skipUnless(
    sys.platform == "linux" and PWSH, "Linux PowerShell launcher control"
)
class McpFailureDiagnosticsTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="mcp-failure-control-")
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.scripts = self.root / "scripts"
        (self.scripts / "ci").mkdir(parents=True)
        for name in (
            "test-agent-bus-mcp-smoke.ps1",
            "test-agent-client-config-validator.ps1",
            "validate-agent-client-configs.ps1",
            "install-mcp-clients.ps1",
        ):
            shutil.copyfile(SCRIPTS / name, self.scripts / name)
        shutil.copyfile(
            SCRIPTS / "ci/fixtures/mcp_failure_adapter.py",
            self.scripts / "ci/mcp_wine_adapter.py",
        )
        self.command = self.root / "agent-bus.exe"
        self.command.write_bytes(b"synthetic fixture; never executed")
        self.env = {
            key: value
            for key, value in os.environ.items()
            if not key.startswith("AGENT_BUS_")
        }
        self.env["TMPDIR"] = str(self.root)
        self.env["AGENT_BUS_TEST_MCP_WINE_ROOT"] = str(self.root)

    def invoke(self, script, arguments):
        return subprocess.run(
            [
                PWSH,
                "-NoLogo",
                "-NoProfile",
                "-NonInteractive",
                "-File",
                str(self.scripts / script),
                *arguments,
            ],
            env=self.env,
            cwd=self.root,
            text=True,
            capture_output=True,
            timeout=20,
        )

    def test_first_protocol_error_survives_failed_launcher_cleanup(self):
        """
        Protects: The first MCP failure and its separate cleanup witness.
        Detects: A nonzero launcher exit masking the earlier JSON-RPC error.
        Needs: Linux, PowerShell and an owned synthetic launcher; no Wine/services.
        Breadcrumb: scripts/test-agent-bus-mcp-smoke.ps1 catch/finally handling.
        """
        wrapper = self.scripts / "capture-failure.ps1"
        wrapper.write_text(
            "param([string]$Smoke,[string]$Command)\n"
            "try { & $Smoke -WineTestLauncher -Command $Command } catch {\n"
            "@{primary=$_.Exception.Message;"
            "cleanup=$_.Exception.Data['McpCleanupFailure']} | ConvertTo-Json\n"
            "exit 1\n}\n"
        )
        result = self.invoke(
            "capture-failure.ps1",
            [
                "-Smoke",
                str(self.scripts / "test-agent-bus-mcp-smoke.ps1"),
                "-Command",
                str(self.command),
            ],
        )
        self.assertNotEqual(result.returncode, 0)
        witness = json.loads(result.stdout)
        self.assertEqual(witness["primary"], "initialize returned a JSON-RPC error.")
        self.assertEqual(witness["cleanup"], "Configured Wine MCP launch failed.")

    def test_failed_validator_keeps_fixture_after_successful_finalization(self):
        """
        Protects: Failed fixture custody and the complete visible first-cause detail.
        Detects: Successful finalize deleting evidence or Format-Table truncation.
        Needs: Linux, PowerShell and an owned synthetic launcher; no Wine/services.
        Breadcrumb: scripts/test-agent-client-config-validator.ps1 finally and
        scripts/validate-agent-client-configs.ps1 result formatting.
        """
        result = self.invoke(
            "test-agent-client-config-validator.ps1",
            [
                "-WineTestLauncher",
                "-CliPath",
                str(self.command),
                "-McpBinaryPath",
                str(self.command),
            ],
        )
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("Failed validator fixture retained:", result.stdout)
        # Format-Table wraps inside words as well as at spaces. Remove only ANSI
        # presentation and whitespace; truncation still cannot satisfy the oracle.
        visible = re.sub(r"\s+", "", re.sub(r"\x1b\[[0-9;]*m", "", result.stdout))
        self.assertIn("initializereturnedaJSON-RPCerror.", visible)
        retained = list(self.root.glob("agent-bus-validator-*"))
        self.assertEqual(len(retained), 1)
        self.assertTrue((retained[0] / "multiline-args.toml").is_file())
        self.assertTrue(list(retained[0].glob("mcp-launch-*.json")))

    def test_first_validator_failure_survives_failed_finalization(self):
        """
        Protects: The first validator failure and independent finalize evidence.
        Detects: Failing finalize replacing the original error or losing custody.
        Needs: Linux, PowerShell and an owned synthetic launcher; no Wine/services.
        Breadcrumb: scripts/test-agent-client-config-validator.ps1 catch/finally.
        """
        (self.scripts / "ci/fail-finalize").touch()
        wrapper = self.scripts / "capture-validator-failure.ps1"
        wrapper.write_text(
            "param([string]$Validator,[string]$Binary)\n"
            "try { & $Validator -WineTestLauncher -CliPath $Binary "
            "-McpBinaryPath $Binary } catch {\n"
            "$meta=@{primary=$_.Exception.Message;"
            "cleanup=$_.Exception.Data['ValidatorCleanupFailure'];"
            "client=$_.Exception.Data['ValidatorCleanupClient']}\n"
            "Write-Output ('VALIDATOR_FAILURE_META='+"
            "(ConvertTo-Json -InputObject $meta -Compress -Depth 5))\n"
            "exit 1\n}\n"
        )
        result = self.invoke(
            "capture-validator-failure.ps1",
            [
                "-Validator",
                str(self.scripts / "test-agent-client-config-validator.ps1"),
                "-Binary",
                str(self.command),
            ],
        )
        self.assertNotEqual(result.returncode, 0)
        line = next(
            line
            for line in result.stdout.splitlines()
            if line.startswith("VALIDATOR_FAILURE_META=")
        )
        witness = json.loads(line.split("=", 1)[1])
        self.assertEqual(
            witness["primary"],
            "Agent client config validation found 1 failure(s) and 1 warning(s).",
        )
        self.assertEqual(
            witness["cleanup"],
            "Owned Wine fixture operation did not settle; prefix retained.",
        )
        self.assertEqual(witness["client"]["operation"], "finalize")
        self.assertEqual(witness["client"]["exit_code"], 9)
        self.assertIn("Failed validator fixture retained:", result.stdout)
        self.assertEqual(len(list(self.root.glob("agent-bus-validator-*"))), 1)


if __name__ == "__main__":
    unittest.main()
