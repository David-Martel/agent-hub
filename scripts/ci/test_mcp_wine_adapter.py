"""Deterministic offline admission controls; real Wine is not exercised."""

import io
import json
import sys
import tempfile
import unittest
from pathlib import Path
from types import SimpleNamespace
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parent))
import mcp_wine_adapter as adapter


class Provider:
    def __init__(self, rows=None):
        self.rows = rows or [{}]
        self.calls = []
        self.checks = 0

    def processes(self, timeout=10):
        self.calls.append(("query", timeout))
        return self.rows.pop(0) if len(self.rows) > 1 else self.rows[0]

    def wait_prefix(self, timeout):
        self.calls.append(("wait", timeout))

    def assert_owner(self):
        self.checks += 1

    def assert_home(self):
        pass


class Child:
    def __init__(self, status):
        self.stdin = io.BytesIO()
        self.returncode = status

    def poll(self):
        return self.returncode


class AdmissionControls(unittest.TestCase):
    def test_closed_backend_accepts_only_explicit_port_one(self):
        for value, scheme in (
            ("redis://localhost:1/0", "redis"),
            ("postgresql://postgres@localhost:1/fixture", "postgresql"),
            ("http://localhost:1", "http"),
        ):
            self.assertEqual(adapter.closed_route(value, scheme), value)

    def test_backend_rejects_live_default(self):
        for value in (
            "http://localhost:8400",
            "http://localhost",
            "http://192.168.50.2:1",
            "http://127.0.0.1:1",
            "https://localhost:1",
            "http://user:secret@localhost:1",
            "http://localhost:1?token=fixture",
        ):
            with self.assertRaises((RuntimeError, ValueError)):
                adapter.closed_route(value, "http")

    def test_host_environment_drops_inherited_auth_routes_and_home(self):
        root = Path("owned-private-home")
        with patch.dict(
            adapter.os.environ,
            {
                "PATH": "qualified-tools",
                "HOME": "foreign",
                "AGENT_BUS_AUTH_TOKEN": "synthetic",
                "AGENT_BUS_CONFIG": "foreign.json",
                "AGENT_BUS_SERVER_URL": "http://localhost:8400",
                "WINEPREFIX": "foreign",
            },
            clear=True,
        ):
            result = adapter.host_environment(root)
        self.assertNotIn("AGENT_BUS_AUTH_TOKEN", result)
        self.assertNotIn("WINEPREFIX", result)
        self.assertNotIn("AGENT_BUS_CONFIG", result)
        self.assertEqual(result["HOME"], str(root))
        self.assertEqual(result["AGENT_BUS_SERVER_URL"], "http://localhost:1")

    def test_environment_rejects_unknown_or_real_auth_key(self):
        for env in (
            {"AGENT_BUS_AUTH_TOKEN": "synthetic"},
            {"UNKNOWN": "x"},
            {"AGENT_BUS_STARTUP_ENABLED": True},
            {"AGENT_BUS_STARTUP_ENABLED": "true"},
        ):
            with self.assertRaises(RuntimeError):
                adapter.guest_environment(SimpleNamespace(env={}), env)

    def test_guest_identity_binds_unique_actual_image(self):
        provider = Provider([{77: "agent-bus-mcp.exe", 1: "services.exe"}])
        self.assertEqual(
            adapter.corroborate(provider, Child(None), "agent-bus-mcp.exe"), 77
        )

    def test_guest_identity_rejects_missing_duplicate_or_exited_launcher(self):
        for rows, status in (
            ({}, None),
            ({1: "agent-bus-mcp.exe", 2: "agent-bus-mcp.exe"}, None),
            ({1: "agent-bus-mcp.exe"}, 0),
            ({1: "other.exe"}, None),
        ):
            with self.assertRaises(RuntimeError):
                adapter.corroborate(
                    Provider([rows]), Child(status), "agent-bus-mcp.exe"
                )

    def test_settlement_requires_guest_absence_and_fixed_prefix_wait(self):
        provider = Provider([{77: "agent-bus-mcp.exe"}, {}])
        ticks = iter([0, 1, 2, 3])
        child = Child(0)
        adapter.settle(
            provider,
            child,
            "agent-bus-mcp.exe",
            77,
            clock=lambda: next(ticks),
            pause=lambda _: None,
        )
        self.assertTrue(child.stdin.closed)
        self.assertEqual(
            [name for name, _ in provider.calls], ["query", "query", "wait"]
        )
        self.assertEqual(provider.checks, 1)

    def test_launcher_exited_but_guest_retained_refuses_wait(self):
        provider = Provider([{77: "agent-bus-mcp.exe"}])
        ticks = iter([0, 1, 11])
        with self.assertRaises(RuntimeError):
            adapter.settle(
                provider,
                Child(0),
                "agent-bus-mcp.exe",
                77,
                clock=lambda: next(ticks),
                pause=lambda _: None,
            )
        self.assertFalse(any(name == "wait" for name, _ in provider.calls))

    def test_original_pid_absent_but_same_image_successor_refuses(self):
        provider = Provider([{88: "agent-bus-mcp.exe"}])
        ticks = iter([0, 1, 11])
        with self.assertRaises(RuntimeError):
            adapter.settle(
                provider,
                Child(0),
                "agent-bus-mcp.exe",
                77,
                clock=lambda: next(ticks),
                pause=lambda _: None,
            )
        self.assertFalse(any(name == "wait" for name, _ in provider.calls))

    def test_expected_nonzero_guest_still_can_prove_cleanup_without_success(self):
        provider = Provider([{}])
        ticks = iter([0, 1, 2])
        child = Child(1)
        adapter.settle(
            provider,
            child,
            "agent-bus-mcp.exe",
            None,
            clock=lambda: next(ticks),
            pause=lambda _: None,
        )
        self.assertEqual(child.returncode, 1)
        self.assertEqual([name for name, _ in provider.calls], ["query", "wait"])

    def test_replaced_prefix_refuses_after_wait(self):
        provider = Provider([{}])
        provider.assert_owner = lambda: (_ for _ in ()).throw(
            RuntimeError("identity changed")
        )
        ticks = iter([0, 1, 2])
        with self.assertRaisesRegex(RuntimeError, "identity changed"):
            adapter.settle(
                provider,
                Child(0),
                "agent-bus-mcp.exe",
                77,
                clock=lambda: next(ticks),
                pause=lambda _: None,
            )

    def test_roundtrip_rejects_changed_mapping(self):
        provider = SimpleNamespace(
            command=lambda args: "Z:\\owned" if args[1] == "-w" else str(Path.cwd())
        )
        with self.assertRaises(RuntimeError):
            adapter.map_path(provider, Path(__file__).resolve())

    def test_bad_guest_path_output_is_rejected(self):
        for value in ("not-windows", "Z:\\owned\nextra", "Z:\\owned\x00"):
            with self.assertRaises(RuntimeError):
                adapter.map_path(
                    SimpleNamespace(command=lambda args, value=value: value), Path.cwd()
                )

    def test_private_synthetic_config_translation_preserves_configured_env(self):
        root = Path(__file__).resolve().parent
        path = root / "synthetic-auth.json"
        provider = SimpleNamespace(
            directory=root, home=root / "wine-home", env=adapter.host_environment(root)
        )
        content = '{"auth_token":"validator-fixture-token"}'
        provider.command = lambda args, **kwargs: content
        with (
            patch.object(adapter, "local_path", return_value=path),
            patch.object(Path, "read_text", return_value=content),
            patch.object(
                adapter,
                "map_path",
                side_effect=lambda owner, value: "Z:\\private\\" + value.name,
            ),
        ):
            result = adapter.guest_environment(
                provider,
                {
                    "AGENT_BUS_CONFIG": str(path),
                    "AGENT_BUS_SERVER_HOST": "0.0.0.0",
                    "AGENT_BUS_ALLOW_REMOTE": "true",
                    "AGENT_BUS_SERVICE_AGENT_ID": 'fixture # = \\ "quote"',
                    "AGENT_BUS_STARTUP_ENABLED": "false",
                },
            )
        self.assertEqual(result["AGENT_BUS_CONFIG"], "Z:\\private\\synthetic-auth.json")
        self.assertEqual(result["AGENT_BUS_SERVICE_AGENT_ID"], 'fixture # = \\ "quote"')
        self.assertEqual(result["AGENT_BUS_ALLOW_REMOTE"], "true")

    def test_real_token_config_and_wrong_guest_readback_are_rejected(self):
        root = Path(__file__).resolve().parent
        provider = SimpleNamespace(
            directory=root,
            home=root / "wine-home",
            env=adapter.host_environment(root),
            command=lambda args, **kwargs: '{"auth_token":"different-fixture"}',
        )
        for content in (
            '{"auth_token":"synthetic-other"}',
            '{"auth_token":"validator-fixture-token"}',
        ):
            with (
                patch.object(
                    adapter, "local_path", return_value=root / "synthetic.json"
                ),
                patch.object(Path, "read_text", return_value=content),
                patch.object(
                    adapter, "map_path", return_value="Z:\\private\\synthetic.json"
                ),
            ):
                with self.assertRaises(RuntimeError):
                    adapter.guest_environment(
                        provider, {"AGENT_BUS_CONFIG": "synthetic.json"}
                    )

    def test_custody_receipt_rejects_foreign_home_and_invalid_identity(self):
        root = Path(__file__).resolve().parent
        data = {
            "root": str(root),
            "profile": "configured-mcp-wine-fixture-v1",
            "prefix": "owned-prefix",
            "identity": [1, 2, 0],
            "home": str(root / "wine-home"),
            "home_identity": [1, 3, 0],
        }
        for update in (
            {"home": "foreign-home"},
            {"home_identity": [1, True, 0]},
            {"identity": [1, 2]},
            {"root": "foreign-root"},
        ):
            with patch.object(
                Path, "read_text", return_value=json.dumps(dict(data, **update))
            ):
                with self.assertRaises(RuntimeError):
                    adapter.provider_for(root)

    def test_home_replacement_refuses_settlement(self):
        provider = Provider([{}])
        provider.assert_home = lambda: (_ for _ in ()).throw(
            RuntimeError("home changed")
        )
        ticks = iter([0, 1, 2])
        with self.assertRaisesRegex(RuntimeError, "home changed"):
            adapter.settle(
                provider,
                Child(0),
                "agent-bus-mcp.exe",
                77,
                clock=lambda: next(ticks),
                pause=lambda _: None,
            )

    def test_bridge_executable_rejects_native_python_and_non_exe(self):
        with self.assertRaises(RuntimeError):
            adapter.executable(__file__)

    def launch_fixture(self, directory):
        root = Path(directory)
        command = root / "shipping.exe"
        command.write_bytes(b"controlled-shipping-fixture")
        config = root / "mcp-wine-empty.json"
        config.write_text("{}", encoding="utf-8")
        provider = Provider()
        provider.directory = root
        provider.prefix = root / "wine-prefix"
        provider.home = root / "wine-home"
        provider.identity = (1, 2, 3)
        provider.home_identity = (1, 4, 3)
        output = io.StringIO()
        with patch.object(adapter.sys, "stdout", output):
            verify = adapter.launch_ready(
                provider,
                command,
                adapter.hashlib.sha256(command.read_bytes()).hexdigest(),
                ["serve", "--transport", "stdio"],
                {},
                {"HOME": str(provider.home)},
                SimpleNamespace(pid=77),
            )
        ready = json.loads(output.getvalue())
        return provider, command, config, ready, verify

    def test_ready_frame_binds_private_receipt_before_protocol_input(self):
        with tempfile.TemporaryDirectory() as directory:
            provider, command, config, ready, verify = self.launch_fixture(directory)
            self.assertEqual(ready["id"], 0)
            proof = ready["result"]
            receipt_path = provider.directory / proof["receipt"]
            payload = receipt_path.read_bytes()
            self.assertEqual(
                adapter.hashlib.sha256(payload).hexdigest(), proof["sha256"]
            )
            receipt = json.loads(payload)
            self.assertEqual(receipt["command"], str(command))
            self.assertEqual(receipt["config"], str(config))
            self.assertEqual(receipt["identity"], [1, 2, 3])
            self.assertEqual(receipt["home_identity"], [1, 4, 3])
            self.assertEqual(receipt["launcher_pid"], 77)
            if sys.platform != "win32":
                self.assertEqual(receipt_path.stat().st_mode & 0o777, 0o600)
            verify()

    def test_ready_receipt_refuses_config_executable_or_receipt_changes(self):
        for mutation in ("config", "command", "receipt"):
            with (
                self.subTest(mutation=mutation),
                tempfile.TemporaryDirectory() as directory,
            ):
                provider, command, config, ready, verify = self.launch_fixture(
                    directory
                )
                target = {
                    "config": config,
                    "command": command,
                    "receipt": provider.directory / ready["result"]["receipt"],
                }[mutation]
                target.write_bytes(b"replaced")
                with self.assertRaisesRegex(RuntimeError, "receipt changed"):
                    verify()

    def test_ready_receipt_refuses_changed_prefix_or_home_identity(self):
        for check in ("assert_owner", "assert_home"):
            with self.subTest(check=check), tempfile.TemporaryDirectory() as directory:
                provider, _, _, _, verify = self.launch_fixture(directory)
                setattr(
                    provider,
                    check,
                    lambda: (_ for _ in ()).throw(RuntimeError("identity changed")),
                )
                with self.assertRaisesRegex(RuntimeError, "identity changed"):
                    verify()


if __name__ == "__main__":
    unittest.main(verbosity=2)
