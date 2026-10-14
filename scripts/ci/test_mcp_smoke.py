"""PowerShell MCP response contract regressions; fake stdio only, no backends."""

import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest

SOURCE = Path(__file__).resolve().parents[1] / "test-agent-bus-mcp-smoke.ps1"
PWSH = shutil.which("pwsh")
FIXTURES = Path(__file__).with_name("fixtures")


@unittest.skipUnless(PWSH, "PowerShell 7 is required for the maintained smoke script")
class McpSmokeTests(unittest.TestCase):
    def setUp(self):
        self.fixture = tempfile.TemporaryDirectory(prefix="agent bus smoke ")
        self.addCleanup(self.fixture.cleanup)
        self.root = Path(self.fixture.name)
        (self.root / "fake_mcp.py").write_bytes(
            (FIXTURES / "mcp_smoke_server.py").read_bytes()
        )
        (self.root / "invoke-smoke.ps1").write_bytes(
            (FIXTURES / "invoke-mcp-smoke.ps1").read_bytes()
        )

    def invoke(self, case):
        env = {k: v for k, v in os.environ.items() if not k.startswith("AGENT_BUS_")}
        return subprocess.run(
            [
                PWSH,
                "-NoLogo",
                "-NoProfile",
                "-NonInteractive",
                "-File",
                str(self.root / "invoke-smoke.ps1"),
                "-SmokePath",
                str(SOURCE),
                "-PythonPath",
                sys.executable,
                "-FakePath",
                str(self.root / "fake_mcp.py"),
                "-Case",
                case,
            ],
            capture_output=True,
            text=True,
            encoding="utf-8",
            errors="replace",
            env=env,
            cwd=self.root,
            timeout=12,
        )

    def success(self, case):
        result = self.invoke(case)
        self.assertEqual(result.returncode, 0, result.stderr)
        data = json.loads(result.stdout)
        self.assertTrue(data["ok"])
        self.assertEqual(data["protocolVersion"], "2024-11-05")
        self.assertEqual(data["serverName"], "agent-bus")
        self.assertEqual(data["serverVersion"], "fixture")
        self.assertEqual(data["toolCount"], 17)
        self.assertEqual(data["tools"], [f"fixture_tool_{i}" for i in range(17)])

    def failure(self, case, message):
        result = self.invoke(case)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn(message, result.stderr)
        self.assertNotIn("PropertyNotFound", result.stderr)
        self.assertNotIn("cannot be found on this object", result.stderr)

    def test_success_without_error(self):
        """
        Protects: Successful JSON-RPC responses without an optional error field.
        Detects: StrictMode property lookup regressions.
        Needs: PowerShell 7 and an owned synthetic stdio child; no services.
        Breadcrumb: scripts/test-agent-bus-mcp-smoke.ps1
        Get-McpProperty and initialize/tools list validation..
        """
        self.success("success")

    def test_notification_without_id(self):
        """
        Protects: Notifications before an expected response.
        Detects: Missing notification id aborting the handshake.
        Needs: PowerShell 7 and an owned synthetic stdio child; no services.
        Breadcrumb: scripts/test-agent-bus-mcp-smoke.ps1
        Read-McpResponse..
        """
        self.success("notification")

    def test_null_error_is_success(self):
        """
        Protects: Explicit null error success compatibility.
        Detects: Treating null error as a failure.
        Needs: PowerShell 7 and an owned synthetic stdio child; no services.
        Breadcrumb: scripts/test-agent-bus-mcp-smoke.ps1
        Get-McpProperty error checks..
        """
        self.success("null_error")

    def test_initialize_error(self):
        """
        Protects: Server initialization error rejection.
        Detects: Accepting a JSON-RPC error as successful initialization.
        Needs: PowerShell 7 and an owned synthetic stdio child; no services.
        Breadcrumb: scripts/test-agent-bus-mcp-smoke.ps1
        initialize error contract..
        """
        self.failure("initialize_error", "initialize returned a JSON-RPC error")

    def test_tools_error(self):
        """
        Protects: Tool-list error rejection.
        Detects: Accepting a failed tools/list reply.
        Needs: PowerShell 7 and an owned synthetic stdio child; no services.
        Breadcrumb: scripts/test-agent-bus-mcp-smoke.ps1
        tools/list error contract..
        """
        self.failure("tools_error", "tools/list returned a JSON-RPC error")

    def test_missing_server_info(self):
        """
        Protects: Required server identity.
        Detects: Missing nested fields raising incidental property exceptions.
        Needs: PowerShell 7 and an owned synthetic stdio child; no services.
        Breadcrumb: scripts/test-agent-bus-mcp-smoke.ps1
        server name contract..
        """
        self.failure("missing_server_info", "Unexpected MCP server name")

    def test_missing_server_version(self):
        """
        Protects: Required server version.
        Detects: Missing version passing or raising incidental property exceptions.
        Needs: PowerShell 7 and an owned synthetic stdio child; no services.
        Breadcrumb: scripts/test-agent-bus-mcp-smoke.ps1
        server version contract..
        """
        self.failure("missing_server_version", "MCP server version is missing")

    def test_missing_tools(self):
        """
        Protects: Seventeen-tool registration requirement.
        Detects: Missing tools passing or raising incidental property exceptions.
        Needs: PowerShell 7 and an owned synthetic stdio child; no services.
        Breadcrumb: scripts/test-agent-bus-mcp-smoke.ps1
        tool-count contract..
        """
        self.failure("missing_tools", "Unexpected MCP tool count")

    def test_missing_tool_name(self):
        """
        Protects: Nonempty registered tool names.
        Detects: Empty-string pipeline results failing to trigger rejection.
        Needs: PowerShell 7 and an owned synthetic stdio child; no services.
        Breadcrumb: scripts/test-agent-bus-mcp-smoke.ps1
        tool-name contract..
        """
        self.failure("missing_tool_name", "One or more MCP tools have a missing name")

    def test_null_result(self):
        """
        Protects: Required initialization result.
        Detects: Null result raising incidental property exceptions.
        Needs: PowerShell 7 and an owned synthetic stdio child; no services.
        Breadcrumb: scripts/test-agent-bus-mcp-smoke.ps1
        protocol version contract..
        """
        self.failure("null_result", "Unexpected MCP protocol version")

    def test_invalid_json(self):
        """
        Protects: Valid JSON response requirement.
        Detects: Malformed stdio output being silently accepted.
        Needs: PowerShell 7 and an owned synthetic stdio child; no services.
        Breadcrumb: scripts/test-agent-bus-mcp-smoke.ps1
        Read-McpResponse parse contract..
        """
        self.failure("invalid_json", "initialize returned invalid JSON")


if __name__ == "__main__":
    unittest.main()
