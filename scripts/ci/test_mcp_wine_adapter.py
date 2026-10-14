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

    def test_guest_environment_reuses_verified_private_temp_mapping(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory).resolve()
            config = root / "fixture.json"
            config.write_text("{}", encoding="utf-8")
            paths = {
                str(config): r"Z:\owned\fixture.json",
                str(root): r"Z:\owned",
            }
            reverse_paths = {value: key for key, value in paths.items()}
            calls = []

            def command(arguments, **kwargs):
                calls.append(arguments)
                if arguments[:2] == ["winepath", "-w"]:
                    self.assertEqual(arguments[2], "-0")
                    return "".join(paths[value] + "\x00" for value in arguments[3:])
                if arguments[:2] == ["winepath", "-u"]:
                    self.assertEqual(arguments[2], "-0")
                    return "".join(
                        reverse_paths[value] + "\x00" for value in arguments[3:]
                    )
                self.assertEqual(
                    arguments, ["wine", "cmd", "/c", "type", paths[str(config)]]
                )
                self.assertEqual(kwargs["env"]["AGENT_BUS_CONFIG"], paths[str(config)])
                return config.read_text(encoding="utf-8")

            provider = SimpleNamespace(
                directory=root,
                home=root / "wine-home",
                env=adapter.host_environment(root),
                command=command,
            )
            result = adapter.guest_environment(
                provider, {"AGENT_BUS_CONFIG": str(config)}
            )
            self.assertEqual(result["TEMP"], paths[str(root)])
            self.assertEqual(result["TMP"], result["TEMP"])
            self.assertEqual(result["HOME"], str(provider.home))
            self.assertEqual(result["AGENT_BUS_CONFIG"], paths[str(config)])
            self.assertEqual(calls[0], ["winepath", "-w", "-0", str(config), str(root)])
            self.assertEqual(
                calls[1], ["winepath", "-u", "-0", paths[str(config)], paths[str(root)]]
            )
            self.assertEqual(len(calls), 3)

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
            command=lambda args: (
                "Z:\\owned\x00" if args[1] == "-w" else str(Path.cwd()) + "\x00"
            )
        )
        with self.assertRaises(RuntimeError):
            adapter.map_path(provider, Path(__file__).resolve())

    def test_bad_guest_path_output_is_rejected(self):
        for value in ("not-windows", "Z:\\owned\nextra\x00", "Z:\\owned\x00extra\x00"):
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
                "map_paths",
                side_effect=lambda owner, values: [
                    "Z:\\private\\" + value.name for value in values
                ],
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
                    adapter,
                    "map_paths",
                    return_value=["Z:\\private\\synthetic.json", "Z:\\private"],
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

    def test_bridge_preserves_protocol_failure_when_settlement_also_fails(self):
        """
        Protects: First protocol failure alongside independent cleanup evidence.
        Detects: Settlement or HOLD-write errors replacing the runtime failure.
        Needs: Mocked subprocesses and temporary files; no Wine or services.
        Breadcrumb: mcp_wine_adapter.py bridge exception and settlement handling.
        """
        for hold_fails in (False, True):
            with self.subTest(hold_fails=hold_fails):
                self.check_bridge_first_failure(hold_fails)

    def check_bridge_first_failure(self, hold_fails):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            child = Child(None)
            child.stdout = [b"[]\n"]
            child.stderr = io.BytesIO()
            thread = SimpleNamespace(start=lambda: None)
            with (
                patch.object(adapter, "provider_for", return_value=Provider()),
                patch.object(
                    adapter, "executable", return_value=(root / "mcp.exe", "digest")
                ),
                patch.object(adapter, "guest_environment", return_value={}),
                patch.object(adapter.subprocess, "Popen", return_value=child),
                patch.object(adapter.threading, "Thread", return_value=thread),
                patch.object(adapter, "launch_ready", return_value=lambda: None),
                patch.object(
                    adapter, "settle", side_effect=RuntimeError("cleanup failed")
                ),
                patch.object(Path, "touch", side_effect=OSError("hold failed"))
                if hold_fails
                else patch.object(Path, "touch", wraps=(root / "mcp-wine-HOLD").touch),
            ):
                with self.assertRaisesRegex(
                    RuntimeError, "response is not an object"
                ) as caught:
                    adapter.bridge(root, "mcp.exe", [], {})
            self.assertEqual(
                caught.exception.mcp_cleanup_failure["class"], "RuntimeError"
            )
            if hold_fails:
                self.assertEqual(
                    caught.exception.mcp_cleanup_failure["hold_failure"]["class"],
                    "OSError",
                )
            else:
                self.assertTrue((root / "mcp-wine-HOLD").exists())

    def test_batched_mapping_refuses_incomplete_extra_blank_or_reordered_records(self):
        """
        Protects: Complete ordered mapping records and every owned-path round-trip.
        Detects: Missing terminators, extra or blank records, and swapped paths.
        Needs: Mocked Wine commands and temporary paths; no Wine or services.
        Breadcrumb: mcp_wine_adapter.py map_paths record and reverse validation.
        """
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory).resolve()
            config = root / "fixture with spaces.json"
            config.write_text("{}")
            forward = "Z:\\owned\\fixture with spaces.json\x00Z:\\owned\x00"
            reverse = str(config) + "\x00" + str(root) + "\x00"
            for output, back in (
                (forward[:-1], reverse),
                (forward + "Z:\\extra\x00", reverse),
                ("Z:\\owned\x00\x00", reverse),
                (forward, str(root) + "\x00" + str(config) + "\x00"),
                (forward, reverse[:-1]),
                (forward, reverse + str(root) + "\x00"),
            ):
                with self.subTest(output=output, back=back):
                    provider = SimpleNamespace(
                        command=lambda args: output if args[1] == "-w" else back
                    )
                    with self.assertRaises(RuntimeError):
                        adapter.map_paths(provider, [config, root])

    def test_stage_failure_reports_static_metadata_without_message_or_environment(self):
        """
        Protects: Useful stage timing and static failure evidence without secrets.
        Detects: Exception messages leaking into recorded diagnostic metadata.
        Needs: Synthetic exceptions and an isolated context; no Wine or services.
        Breadcrumb: mcp_wine_adapter.py stage and static_failure.
        """
        secret = "synthetic-sensitive-marker"
        token = adapter.STAGES.set([])
        try:
            with self.assertRaises(OSError):
                adapter.stage(
                    "mapping-forward", lambda: (_ for _ in ()).throw(OSError(5, secret))
                )
            events = adapter.STAGES.get()
            self.assertEqual(events[0]["stage"], "mapping-forward")
            self.assertEqual(events[0]["status"], "FAIL")
            self.assertEqual(events[0]["failure"]["errno"], 5)
            self.assertGreaterEqual(events[0]["seconds"], 0)
            self.assertNotIn(secret, json.dumps(events))
        finally:
            adapter.STAGES.reset(token)

    def test_main_failure_redacts_exception_messages_and_resets_stage_context(self):
        """
        Protects: Safe top-level failure output and per-invocation stage isolation.
        Detects: Raw exception text or stage state leaking into later invocations.
        Needs: Mocked fixture admission and bridge; no Wine or services.
        Breadcrumb: mcp_wine_adapter.py main failure rendering and context reset.
        """
        secret = "synthetic-sensitive-marker"
        root = Path("private-root")
        output = io.StringIO()
        outer_events = []
        token = adapter.STAGES.set(outer_events)
        try:
            with (
                patch.object(adapter, "fixture_root", return_value=root),
                patch.object(adapter.sys, "platform", "linux"),
                patch.object(adapter.sys, "stderr", output),
                patch.object(adapter, "bridge", side_effect=OSError(5, secret)),
            ):
                code = adapter.main(
                    ["bridge", "--root", str(root), "--command", "private.exe"]
                )
            self.assertEqual(code, 1)
            diagnostic = json.loads(output.getvalue().splitlines()[0])
            self.assertEqual(diagnostic["failure"]["class"], "OSError")
            self.assertEqual(diagnostic["failure"]["errno"], 5)
            self.assertNotIn(secret, output.getvalue())
            self.assertIs(adapter.STAGES.get(), outer_events)
        finally:
            adapter.STAGES.reset(token)

    def test_retained_failure_is_private_redacted_and_never_overwrites(self):
        """
        Protects: Private redacted failure receipts and preservation of prior bytes.
        Detects: Reused names overwriting evidence or overly broad file permissions.
        Needs: Temporary files; POSIX permission checks on POSIX; no services.
        Breadcrumb: mcp_wine_adapter.py retain_failure exclusive mode-0600 write.
        """
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            error = OSError(5, "synthetic-sensitive-marker")
            diagnostic = {"failure": adapter.static_failure(error), "stages": []}
            with patch.object(adapter, "fixture_root", return_value=root):
                first = adapter.retain_failure(root, diagnostic)
                second = adapter.retain_failure(root, diagnostic)
                self.assertNotEqual(first, second)
                payload = (root / first).read_bytes()
                self.assertNotIn(b"synthetic-sensitive-marker", payload)
                self.assertEqual(json.loads(payload), diagnostic)
                if sys.platform != "win32":
                    self.assertEqual((root / first).stat().st_mode & 0o777, 0o600)
                # Force a UUID collision; the first receipt must remain byte-identical.
                identifier = first.removeprefix("mcp-wine-failure-").removesuffix(
                    ".json"
                )
                with patch.object(
                    adapter.uuid, "uuid4", return_value=SimpleNamespace(hex=identifier)
                ):
                    with self.assertRaises(FileExistsError):
                        adapter.retain_failure(root, {"different": True})
                self.assertEqual((root / first).read_bytes(), payload)

    def test_failed_receipt_write_does_not_mask_first_failure(self):
        """
        Protects: First runtime failure when private receipt retention fails.
        Detects: Storage faults masking the cause or exposing exception messages.
        Needs: Mocked runtime and retention exceptions; no Wine or services.
        Breadcrumb: mcp_wine_adapter.py main retention_failure handling.
        """
        output = io.StringIO()
        with (
            patch.object(adapter, "fixture_root", return_value=Path("private-root")),
            patch.object(adapter.sys, "platform", "linux"),
            patch.object(adapter.sys, "stderr", output),
            patch.object(
                adapter, "bridge", side_effect=RuntimeError("private first marker")
            ),
            patch.object(
                adapter,
                "retain_failure",
                side_effect=OSError(13, "private write marker"),
            ),
        ):
            code = adapter.main(
                ["bridge", "--root", "private-root", "--command", "private.exe"]
            )
        self.assertEqual(code, 1)
        diagnostic = json.loads(output.getvalue().splitlines()[0])
        self.assertEqual(diagnostic["failure"]["class"], "RuntimeError")
        self.assertEqual(diagnostic["retention_failure"]["class"], "PermissionError")
        self.assertEqual(diagnostic["retention_failure"]["errno"], 13)
        self.assertNotIn("private first marker", output.getvalue())
        self.assertNotIn("private write marker", output.getvalue())

    def test_receipt_requires_revalidated_owned_root(self):
        """
        Protects: Diagnostic writes staying inside an admitted private fixture.
        Detects: Writing evidence after the fixture ownership check rejects it.
        Needs: Mocked ownership failure and temporary paths; no Wine or services.
        Breadcrumb: mcp_wine_adapter.py retain_failure fixture_root revalidation.
        """
        with tempfile.TemporaryDirectory() as directory:
            with patch.object(
                adapter, "fixture_root", side_effect=RuntimeError("owner changed")
            ):
                with self.assertRaisesRegex(RuntimeError, "owner changed"):
                    adapter.retain_failure(Path(directory), {})
            self.assertEqual(list(Path(directory).iterdir()), [])

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
