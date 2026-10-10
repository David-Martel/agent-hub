"""Hardware-free controls for job ownership, deadlines and proof consumption."""

from pathlib import Path
import tempfile
from types import SimpleNamespace
import unittest

import wine_job as job


class FakeProvider:
    def __init__(self):
        self.events = []
        self.outputs = ["ok"]
        self.failure = None
        self.retained_guest = False
        self.prefix_replaced = False
        self.home_replaced = False
        self.unrelated_prefix = {"running": True, "identity": (7, 8, 9)}
        self.retired = False

    def assert_owner(self):
        if self.prefix_replaced:
            raise RuntimeError("Exclusive Wine prefix ownership changed")

    def assert_home(self):
        if self.home_replaced:
            raise RuntimeError("Private Wine HOME ownership changed")

    def command(self, args, *, env, timeout):
        self.events.append(("command", list(args), timeout, dict(env)))
        if self.failure:
            raise self.failure
        return self.outputs[
            min(
                len([e for e in self.events if e[0] == "command"]) - 1,
                len(self.outputs) - 1,
            )
        ]

    def wait_prefix(self, timeout):
        self.events.append(("wait", timeout))
        if self.retained_guest:
            raise RuntimeError("Exact prefix still has a retained guest")

    def retire_prefix(self):
        self.assert_owner()
        self.retired = True
        self.events.append(("retire",))


class GroupControls(unittest.TestCase):
    def test_success_requires_independent_wait_before_retirement(self):
        provider = FakeProvider()
        self.assertEqual(job.group(provider, [["cargo", "test"]], 900, {}), ["ok"])
        self.assertEqual([e[0] for e in provider.events], ["command", "wait", "retire"])
        self.assertGreater(provider.events[0][2], 800)
        self.assertEqual(
            provider.unrelated_prefix, {"running": True, "identity": (7, 8, 9)}
        )

    def test_launcher_zero_retained_guest_fails_without_prefix_retirement(self):
        provider = FakeProvider()
        provider.retained_guest = True
        with self.assertRaisesRegex(RuntimeError, "retained guest"):
            job.group(provider, [["wine", "shipping.exe", "--version"]], 300, {})
        self.assertFalse(provider.retired)
        self.assertTrue(provider.unrelated_prefix["running"])

    def test_original_client_failure_survives_settlement_failure(self):
        provider = FakeProvider()
        provider.failure = RuntimeError("Original finite client failed")
        provider.retained_guest = True
        with self.assertRaises(RuntimeError) as caught:
            job.group(provider, [["cargo", "test"]], 900, {})
        self.assertIs(caught.exception, provider.failure)
        self.assertEqual([e[0] for e in provider.events], ["command", "wait"])

    def test_wrong_version_retains_prefix_even_if_wait_succeeds(self):
        provider = FakeProvider()
        with self.assertRaisesRegex(RuntimeError, "build revision differs"):
            job.group(
                provider,
                [["wine", "app.exe"]],
                300,
                {},
                revision="a" * 40,
                version_count=1,
            )
        self.assertFalse(provider.retired)

    def test_wine_launcher_version_is_not_mistaken_for_artifact_revision(self):
        provider = FakeProvider()
        provider.outputs = ["app " + "a" * 12, "wine-9.0"]
        result = job.group(
            provider,
            [["wine", "app.exe"], ["wine", "--version"]],
            300,
            {},
            revision="a" * 40,
            version_count=1,
        )
        self.assertEqual(result[-1], "wine-9.0")
        self.assertTrue(provider.retired)

    def test_prefix_replaced_during_wait_refuses_success(self):
        provider = FakeProvider()
        original = provider.wait_prefix

        def mutate(timeout):
            original(timeout)
            provider.prefix_replaced = True

        provider.wait_prefix = mutate
        with self.assertRaisesRegex(RuntimeError, "prefix ownership changed"):
            job.group(provider, [["cargo", "test"]], 900, {})
        self.assertFalse(provider.retired)

    def test_private_home_replaced_during_wait_refuses_success(self):
        provider = FakeProvider()

        def mutate(timeout):
            provider.home_replaced = True

        provider.wait_prefix = mutate
        with self.assertRaisesRegex(RuntimeError, "HOME ownership changed"):
            job.group(provider, [["cargo", "test"]], 900, {})
        self.assertFalse(provider.retired)

    def test_fully_settled_late_group_is_rejected(self):
        provider = FakeProvider()
        values = iter([0, 0, 1, 301])
        with self.assertRaisesRegex(RuntimeError, "after deadline"):
            job.group(
                provider, [["cargo", "test"]], 300, {}, clock=lambda: next(values)
            )
        self.assertFalse(provider.retired)

    def test_exhausted_command_budget_does_not_launch_client(self):
        provider = FakeProvider()
        with self.assertRaisesRegex(RuntimeError, "deadline exhausted"):
            job.group(provider, [["cargo", "test"]], 9, {})
        self.assertEqual([e[0] for e in provider.events], ["wait"])
        self.assertFalse(provider.retired)

    def test_publication_failure_keeps_prefix(self):
        provider = FakeProvider()

        def finish(_):
            raise FileExistsError("Existing evidence cannot be overwritten")

        with self.assertRaises(FileExistsError):
            job.group(provider, [["wine", "app.exe"]], 300, {}, finish=finish)
        self.assertFalse(provider.retired)


class ProofControls(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name).resolve()
        self.cargo_home = self.root / "cargo-home"
        self.rustup_home = self.root / "rustup-home"
        self.target = self.root / "target"
        for path in (self.cargo_home, self.rustup_home, self.target):
            path.mkdir()
        self.tool = self.root / "qualified-tool"
        self.tool.write_bytes(b"synthetic qualified tool bytes")
        self.digest = job.sha256(self.tool)
        tool = str(self.tool)
        self.proof = {
            "target": job.TARGET,
            "abi": "gnu",
            "host_os": "Linux",
            "host_arch": "X64",
            "cargo_home": str(self.cargo_home),
            "rustup_home": str(self.rustup_home),
            "cargo": tool,
            "rustc": tool,
            "linker": tool,
            "cargo_sha256": self.digest,
            "rustc_sha256": self.digest,
            "linker_sha256": self.digest,
            "dedicated_cache_owner": {
                "directory": "admitted-cache-data",
                "config": "admitted-cache-config",
                "executable_sha256": self.digest,
                "idle_timeout": "1800",
            },
        }
        self.source = {
            "RUSTC": tool,
            "CARGO_BUILD_RUSTC": tool,
            "RUSTC_WRAPPER": tool,
            "CARGO_TARGET_X86_64_PC_WINDOWS_GNU_LINKER": tool,
            "SCCACHE_DIR": "admitted-cache-data",
            "SCCACHE_CONF": "admitted-cache-config",
            "SCCACHE_SERVER_PORT": "4228",
            "SCCACHE_IDLE_TIMEOUT": "1800",
            "CARGO_TARGET_DIR": str(self.target),
            "RUSTC_WORKSPACE_WRAPPER": "",
            "CARGO_BUILD_RUSTC_WORKSPACE_WRAPPER": "",
            "CARGO_BUILD_RUSTC_WRAPPER": "",
            "PATH": "qualified image tool path",
            "AGENT_BUS_BUILD_REVISION": "a" * 40,
        }

    def bindings(self):
        return job.tool_bindings(self.source, self.proof, str(self.tool))

    def test_receipt_homes_preserved_without_inherited_home_fallback(self):
        self.source["HOME"] = "unqualified inherited home"
        bindings = self.bindings()
        self.assertEqual(bindings["CARGO_HOME"], str(self.cargo_home))
        self.assertEqual(bindings["RUSTUP_HOME"], str(self.rustup_home))
        self.assertEqual(bindings["CARGO_TARGET_X86_64_PC_WINDOWS_GNU_RUNNER"], "wine")

    def test_conflicting_inherited_cargo_home_is_rejected(self):
        self.source["CARGO_HOME"] = str(self.root)
        with self.assertRaisesRegex(RuntimeError, "tool home differs"):
            self.bindings()

    def test_missing_or_changed_idle_timeout_is_rejected(self):
        owner = self.proof["dedicated_cache_owner"]
        for mapping, key in (
            (owner, "idle_timeout"),
            (self.source, "SCCACHE_IDLE_TIMEOUT"),
        ):
            for value in (None, "wrong", "0", "1500", "600", 1800):
                with self.subTest(key=key, value=value):
                    original = mapping.pop(key)
                    if value is not None:
                        mapping[key] = value
                    try:
                        with self.assertRaisesRegex(RuntimeError, "bindings changed"):
                            self.bindings()
                    finally:
                        mapping[key] = original

    def test_qualified_idle_timeout_reaches_guest_group_commands(self):
        (self.root / "agent-bus-config.json").write_text("{}", encoding="utf-8")
        env = job.closed_environment(self.root, self.source)
        self.assertNotIn("SCCACHE_IDLE_TIMEOUT", env)
        mappings = {}

        def fake_guest_command(args, **_kwargs):
            if args[:2] == ["winepath", "-w"]:
                converted = "Z:\\owned\\" + Path(args[2]).name
                mappings[converted] = args[2]
                return converted
            if args[:2] == ["winepath", "-u"]:
                return mappings[args[2]]
            self.assertEqual(args[:4], ["wine", "cmd", "/c", "type"])
            self.assertEqual(mappings[args[4]], env["AGENT_BUS_CONFIG"])
            return "{}"

        guest = SimpleNamespace(
            directory=self.root,
            private_environment=lambda value: dict(value),
            command=fake_guest_command,
        )
        env = job.NativeWineProvider.guest_environment(guest, env)
        env.update(self.bindings())
        provider = FakeProvider()
        commands = [[str(self.tool), *command] for command in job.UNIT_COMMANDS]
        job.group(provider, commands, 900, env)
        captured = [event for event in provider.events if event[0] == "command"]
        self.assertEqual(len(captured), len(job.UNIT_COMMANDS))
        for event in captured:
            self.assertEqual(event[3]["SCCACHE_IDLE_TIMEOUT"], "1800")
            self.assertEqual(
                event[3]["AGENT_BUS_CONFIG"],
                "Z:\\owned\\agent-bus-config.json",
            )
        self.assertTrue(provider.retired)

    def test_changed_tool_bytes_rejected(self):
        self.tool.write_bytes(b"changed compiler")
        with self.assertRaisesRegex(RuntimeError, "executable differs"):
            self.bindings()

    def test_same_bytes_different_compiler_path_rejected(self):
        duplicate = self.root / "unadmitted-tool"
        duplicate.write_bytes(self.tool.read_bytes())
        self.source["RUSTC"] = self.source["CARGO_BUILD_RUSTC"] = str(duplicate)
        with self.assertRaisesRegex(RuntimeError, "bindings changed"):
            self.bindings()

    def test_cache_or_wrapper_change_rejected(self):
        for key, value in (
            ("SCCACHE_SERVER_PORT", "4226"),
            ("SCCACHE_CONF", "foreign-config"),
            ("RUSTC_WORKSPACE_WRAPPER", "foreign-wrapper"),
        ):
            with self.subTest(key=key):
                original = self.source[key]
                self.source[key] = value
                with self.assertRaises(RuntimeError):
                    self.bindings()
                self.source[key] = original

    def test_credential_bearing_cargo_home_refused(self):
        (self.cargo_home / "credentials.toml").write_text("synthetic", encoding="utf-8")
        with self.assertRaisesRegex(RuntimeError, "Credential-bearing"):
            self.bindings()

    def test_closed_environment_drops_auth_profiles_and_loader_overrides(self):
        source = {
            **self.source,
            "HOME": "foreign-home",
            "USERPROFILE": "foreign-profile",
            "APPDATA": "foreign-appdata",
            "WINEPREFIX": "foreign-prefix",
            "LD_PRELOAD": "foreign-loader",
            "AGENT_BUS_AUTH_TOKEN": "synthetic-auth",
            "GH_TOKEN": "synthetic-gh",
            "AWS_ACCESS_KEY_ID": "synthetic-cloud",
            "AGENT_BUS_SERVER_CANDIDATES": "foreign-route",
        }
        env = job.closed_environment(self.root, source)
        for key in (
            "HOME",
            "USERPROFILE",
            "APPDATA",
            "WINEPREFIX",
            "LD_PRELOAD",
            "GH_TOKEN",
            "AWS_ACCESS_KEY_ID",
            "AGENT_BUS_AUTH_TOKEN",
            "AGENT_BUS_SERVER_CANDIDATES",
        ):
            self.assertNotIn(key, env)
        self.assertEqual(env["AGENT_BUS_REDIS_URL"], "redis://localhost:1/0")
        self.assertEqual(
            env["AGENT_BUS_CONFIG"], str(self.root / "agent-bus-config.json")
        )

    def test_exact_unit_selection_is_preserved(self):
        self.assertEqual(
            job.UNIT_COMMANDS[0],
            (
                "test",
                "--target",
                job.TARGET,
                "--workspace",
                "--lib",
                "--bins",
                "--",
                "--test-threads=4",
            ),
        )
        self.assertEqual(
            job.UNIT_COMMANDS[1],
            (
                "test",
                "--target",
                job.TARGET,
                "-p",
                "agent-bus",
                "--test",
                "integration_isolation_test",
            ),
        )


if __name__ == "__main__":
    unittest.main()
