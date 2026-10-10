"""Deterministic lifecycle controls: no Wine, daemon or backend access."""

import tempfile
import stat
import importlib.util
import os
import sys
import time
import json
import hashlib
import shutil
import subprocess

import wine_job
import unittest
from pathlib import Path
from types import SimpleNamespace
from unittest.mock import MagicMock, patch

from wine_lifecycle import (
    NativeWineProvider,
    WineGuest,
    process_rows,
    prove_runner_namespace,
    bounded_client,
    WineCommandFailure,
    UNSETTLED_CLIENTS,
    run_guest,
)

SPEC = importlib.util.spec_from_file_location(
    "wine_isolated_services", Path(__file__).with_name("isolated-services.py")
)
services = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(services)


class Provider:
    def __init__(self, tables):
        self.tables = iter(tables)
        self.last = {}
        self.waited = False
        self.retired = False
        self.foreign_prefix = {99: "foreign.exe"}

    def processes(self, timeout=10):
        self.last = next(self.tables, self.last)
        return self.last

    def assert_owner(self):
        pass

    def wait_prefix(self, timeout):
        self.waited = True

    def retire_prefix(self):
        self.retired = True


class Clock:
    def __init__(self):
        self.value = 0

    def now(self):
        return self.value

    def pause(self, seconds):
        self.value += seconds


class WineControls(unittest.TestCase):
    def guest(self, tables, launcher_exit=None):
        provider = Provider(tables)
        launcher = MagicMock(pid=321)
        launcher.poll.return_value = launcher_exit
        guest = WineGuest(provider, launcher, "agent-bus-http.exe", "a" * 32)
        return guest, provider, launcher

    def health(self, pid=654, nonce="a" * 32):
        return {
            "maintenance": {"pid": pid, "service_agent_id": "agent-bus-test-" + nonce}
        }

    def test_separate_unix_and_guest_ids_are_corroborated(self):
        guest, _, launcher = self.guest([{654: "agent-bus-http.exe"}])
        guest.corroborate(self.health())
        self.assertEqual(guest.guest_pid, 654)
        self.assertNotEqual(guest.guest_pid, launcher.pid)

    def test_wrong_image_wrong_nonce_and_boolean_pid_are_rejected(self):
        for table, health in (
            ({654: "foreign.exe"}, self.health()),
            ({654: "agent-bus-http.exe"}, self.health(nonce="b" * 32)),
            ({1: "agent-bus-http.exe"}, self.health(pid=True)),
        ):
            with self.subTest(table=table, health=health):
                guest, _, _ = self.guest([table])
                with self.assertRaisesRegex(RuntimeError, "identity"):
                    guest.corroborate(health)

    def test_guest_identity_cannot_change_between_readbacks(self):
        guest, _, _ = self.guest(
            [{654: "agent-bus-http.exe"}, {655: "agent-bus-http.exe"}]
        )
        guest.corroborate(self.health())
        with self.assertRaisesRegex(RuntimeError, "changed"):
            guest.corroborate(self.health(pid=655))

    def test_exited_launcher_retained_guest_fails_without_retiring_prefix(self):
        guest, provider, launcher = self.guest([{654: "agent-bus-http.exe"}], 0)
        guest.guest_pid = 654
        guest.ready = True
        clock = Clock()
        stop = MagicMock()
        with self.assertRaisesRegex(RuntimeError, "prefix retained"):
            guest.close(
                stop, lambda: True, timeout=0.2, clock=clock.now, pause=clock.pause
            )
        stop.assert_called_once()
        self.assertFalse(provider.retired)
        launcher.terminate.assert_not_called()
        self.assertEqual(provider.foreign_prefix, {99: "foreign.exe"})

    def test_closed_guest_but_retained_listener_fails(self):
        guest, provider, _ = self.guest([{}], 0)
        guest.guest_pid = 654
        guest.ready = True
        clock = Clock()
        with self.assertRaisesRegex(RuntimeError, "listener"):
            guest.close(
                lambda: None,
                lambda: False,
                timeout=0.2,
                clock=clock.now,
                pause=clock.pause,
            )
        self.assertFalse(provider.retired)

    def test_http_stop_requires_owned_launcher_stop_after_listener_closes(self):
        guest, provider, launcher = self.guest([{654: "agent-bus-http.exe"}, {}])
        guest.guest_pid = 654
        guest.ready = True
        launcher.terminate.side_effect = lambda: setattr(
            launcher.poll, "return_value", -15
        )
        stop = MagicMock()
        guest.close(stop, lambda: True)
        stop.assert_called_once()
        launcher.terminate.assert_called_once()
        launcher.kill.assert_not_called()
        self.assertTrue(provider.waited)
        self.assertTrue(provider.retired)

    def test_replaced_guest_is_refused_before_launcher_signal(self):
        guest, provider, launcher = self.guest([{654: "foreign.exe"}])
        guest.guest_pid = 654
        guest.ready = True
        with self.assertRaisesRegex(RuntimeError, "identity changed"):
            guest.close(lambda: None, lambda: True)
        launcher.terminate.assert_not_called()
        self.assertFalse(provider.retired)

    def test_open_listener_never_requests_managed_guest_termination(self):
        guest, _, launcher = self.guest([{654: "agent-bus-http.exe"}])
        guest.guest_pid = 654
        guest.ready = True
        clock = Clock()
        with self.assertRaisesRegex(RuntimeError, "did not settle"):
            guest.close(
                lambda: None,
                lambda: False,
                timeout=0.2,
                clock=clock.now,
                pause=clock.pause,
            )
        launcher.terminate.assert_not_called()

    def test_launcher_signal_relays_only_to_original_unreaped_child(self):
        with tempfile.TemporaryDirectory() as temporary:
            artifact = Path(temporary) / "agent-bus-http.exe"
            artifact.write_bytes(b"synthetic fixture, never executed")
            provider = MagicMock(unsafe=True)
            provider.private_environment.return_value = {}
            provider.guest_environment.return_value = {}
            child = MagicMock()
            child.poll.return_value = None
            handlers = []
            previous = object()

            def install(_number, handler):
                handlers.append(handler)
                return previous

            def wait():
                handlers[0](15, None)
                return -15

            child.wait.side_effect = wait
            with (
                patch(
                    "wine_lifecycle.NativeWineProvider.__new__", return_value=provider
                ),
                patch("wine_lifecycle.signal.signal", side_effect=install),
                patch("wine_lifecycle.subprocess.Popen", return_value=child) as launch,
            ):
                self.assertEqual(
                    run_guest(temporary, temporary, artifact, [], (1, 2, 3), (1, 4, 3)),
                    -15,
                )
            child.terminate.assert_called_once()
            child.kill.assert_not_called()
            self.assertIs(handlers[-1], previous)
            self.assertEqual(launch.call_count, 1)

    def test_success_requires_prefix_wait_and_preserves_unrelated_prefix(self):
        guest, provider, launcher = self.guest([{}], 0)
        guest.guest_pid = 654
        guest.ready = True
        guest.close(lambda: None, lambda: True)
        self.assertTrue(provider.waited)
        self.assertTrue(provider.retired)
        self.assertEqual(provider.foreign_prefix, {99: "foreign.exe"})
        launcher.kill.assert_not_called()
        launcher.terminate.assert_not_called()

    def test_prefix_wait_failure_never_retires_prefix(self):
        guest, provider, _ = self.guest([{}], 0)
        guest.ready = True
        provider.wait_prefix = MagicMock(
            side_effect=RuntimeError("prefix still occupied")
        )
        with self.assertRaisesRegex(RuntimeError, "occupied"):
            guest.close(lambda: None, lambda: True)
        self.assertFalse(provider.retired)

    def test_replaced_prefix_is_rejected_before_query_and_after_wait(self):
        provider = NativeWineProvider.__new__(NativeWineProvider)
        provider.prefix = MagicMock()
        provider.identity = (7, 8, 9)
        original = SimpleNamespace(
            st_dev=7, st_ino=8, st_uid=9, st_mode=stat.S_IFDIR | 0o700
        )
        replaced = SimpleNamespace(
            st_dev=7, st_ino=10, st_uid=9, st_mode=stat.S_IFDIR | 0o700
        )
        provider.env = {"WINEPREFIX": "owned-prefix"}
        provider.prefix.lstat.return_value = replaced
        with (
            patch("wine_lifecycle.os.getuid", return_value=9, create=True),
            patch("wine_lifecycle.bounded_client") as run,
            self.assertRaisesRegex(RuntimeError, "ownership changed"),
        ):
            provider.processes()
        run.assert_not_called()
        provider.assert_home = MagicMock()
        provider.prefix.lstat.side_effect = [original, replaced]
        with (
            patch("wine_lifecycle.os.getuid", return_value=9, create=True),
            patch("wine_lifecycle.bounded_client", return_value=""),
            self.assertRaisesRegex(RuntimeError, "ownership changed"),
        ):
            provider.wait_prefix(0.1)
        provider.prefix.lstat.side_effect = [original, replaced]
        with (
            patch("wine_lifecycle.os.getuid", return_value=9, create=True),
            patch(
                "wine_lifecycle.bounded_client",
                return_value='"agent-bus-http.exe","654","Console","1","3 K"',
            ),
            self.assertRaisesRegex(RuntimeError, "ownership changed"),
        ):
            provider.processes()
        # A symlink/recreated prefix must not inherit a successful old server wait.
        provider.prefix.lstat.side_effect = None
        provider.prefix.lstat.return_value = SimpleNamespace(
            st_dev=7, st_ino=8, st_uid=9, st_mode=stat.S_IFLNK | 0o700
        )
        with (
            patch("wine_lifecycle.os.getuid", return_value=9, create=True),
            self.assertRaisesRegex(RuntimeError, "ownership changed"),
        ):
            provider.assert_owner()

    def test_launcher_wrapper_embeds_original_prefix_identity(self):
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            artifact = directory / "agent-bus-http.exe"
            artifact.write_bytes(b"synthetic fixture, never executed")
            provider = NativeWineProvider.__new__(NativeWineProvider)
            provider.directory = directory
            provider.prefix = directory / "owned-prefix"
            provider.identity = (7, 8, 9)
            provider.home_identity = (7, 11, 9)
            wrapper = provider.wrapper(artifact)
            source = wrapper.read_text(encoding="utf-8")
            self.assertIn("(7, 8, 9)", source)
            compile(source, str(wrapper), "exec")

    def test_uncorroborated_guest_cannot_receive_authenticated_stop(self):
        guest, provider, launcher = self.guest([{}], 0)
        stop = MagicMock()
        guest.close(stop, lambda: True)
        stop.assert_not_called()
        self.assertTrue(provider.waited)
        launcher.kill.assert_not_called()

    def test_process_query_rejects_malformed_and_duplicate_rows(self):
        valid = '"agent-bus-http.exe","654","Console","1","3,000 K"\n'
        self.assertEqual(process_rows(valid), {654: "agent-bus-http.exe"})
        for bad in ("", "unsupported query", valid + valid):
            with self.assertRaises(RuntimeError):
                process_rows(bad)

    def test_namespace_requires_same_kernel_and_actual_network_not_just_running_id(
        self,
    ):
        cid = "a" * 64
        expected = {
            "/proc/sys/kernel/random/boot_id": "a" * 8
            + "-"
            + "a" * 4
            + "-"
            + "a" * 4
            + "-"
            + "a" * 4
            + "-"
            + "a" * 12,
            "/proc/self/ns/net": "net:[123]",
        }
        command = MagicMock(side_effect=lambda argv: expected[argv[-1]])
        prove_runner_namespace(cid, command, lambda path: expected[path])
        for values in (
            dict(expected, **{"/proc/self/ns/net": "net:[456]"}),
            dict(expected, **{"/proc/sys/kernel/random/boot_id": "b" * 36}),
        ):
            with self.assertRaisesRegex(RuntimeError, "not qualified"):
                prove_runner_namespace(
                    cid, lambda argv: values[argv[-1]], lambda path: expected[path]
                )
        with self.assertRaisesRegex(RuntimeError, "canonical"):
            prove_runner_namespace("a" * 12, command, lambda path: expected[path])

    def test_path_roundtrip_and_guest_configuration_readback_are_required(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            config = root / "configuration with spaces.json"
            config.write_text("{}", encoding="utf-8")
            provider = NativeWineProvider.__new__(NativeWineProvider)
            provider.directory = root.resolve()
            provider.prefix = root / "prefix"
            provider.home = root / "wine-home"
            env = {
                "AGENT_BUS_CONFIG": str(config),
                "TEMP": str(root),
                "TMP": str(root),
                "TMPDIR": str(root),
            }
            mapping = {}

            def command(argv, **kwargs):
                if argv[0] == "winepath" and argv[1] == "-w":
                    mapped = "Z:\\owned\\" + Path(argv[-1]).name
                    mapping[mapped] = argv[-1]
                    return mapped
                if argv[0] == "winepath":
                    return mapping[argv[-1]]
                return "{}\n"

            provider.command = command
            self.assertIn(
                "Z:\\owned", provider.guest_environment(env)["AGENT_BUS_CONFIG"]
            )
            self.assertNotIn("TMPDIR", provider.guest_environment(env))
            provider.command = lambda argv, **kwargs: (
                "Z:\\wrong" if argv[1] == "-w" else str(root)
            )
            with self.assertRaisesRegex(RuntimeError, "round-trip"):
                provider.guest_environment(env)
            provider.command = lambda argv, **kwargs: (
                command(argv, **kwargs)
                if argv[0] == "winepath"
                else "wrong configuration"
            )
            with self.assertRaisesRegex(RuntimeError, "private empty configuration"):
                provider.guest_environment(env)

    def test_custom_tmpdir_is_excluded_without_changing_owned_prefix_or_home(self):
        provider = NativeWineProvider.__new__(NativeWineProvider)
        provider.prefix = Path("owned-prefix")
        provider.directory = Path("owned-harness")
        provider.home = provider.directory / "wine-home"
        for key in ("TMPDIR", "tmpdir", "TmPdIr"):
            actual = provider.private_environment({key: "foreign-temp"})
            self.assertFalse(any(name.upper() == "TMPDIR" for name in actual))
            self.assertEqual(actual["HOME"], str(provider.home))
            self.assertEqual(actual["WINEPREFIX"], str(provider.prefix))

    def test_inherited_home_profile_credentials_and_loader_overrides_are_removed(self):
        provider = NativeWineProvider.__new__(NativeWineProvider)
        provider.prefix = Path("owned-prefix")
        provider.directory = Path("owned-harness")
        provider.home = provider.directory / "wine-home"
        inherited = {
            key: "foreign-value"
            for key in (
                "HOME",
                "USERPROFILE",
                "APPDATA",
                "LOCALAPPDATA",
                "WINEPREFIX",
                "WINEPATH",
                "WINEDLLOVERRIDES",
                "PYTHONPATH",
                "PYTHONHOME",
                "LD_PRELOAD",
                "LD_LIBRARY_PATH",
                "AWS_SECRET_ACCESS_KEY",
                "SSH_AUTH_SOCK",
                "XDG_CONFIG_HOME",
                "XDG_DATA_HOME",
                "XDG_CACHE_HOME",
            )
        }
        inherited.update(
            PATH="qualified-tools",
            AGENT_BUS_CONFIG="owned-config",
            AGENT_BUS_TEST_DATABASE_URL="closed-disposable-endpoint",
        )
        actual = provider.private_environment(inherited)
        self.assertEqual(actual["HOME"], str(provider.home))
        self.assertNotIn("TMPDIR", actual)
        self.assertEqual(actual["AGENT_BUS_CONFIG"], "owned-config")
        self.assertEqual(
            actual["AGENT_BUS_TEST_DATABASE_URL"], "closed-disposable-endpoint"
        )
        for key in inherited.keys() - {
            "PATH",
            "HOME",
            "WINEPREFIX",
            "AGENT_BUS_CONFIG",
            "AGENT_BUS_TEST_DATABASE_URL",
            "XDG_CONFIG_HOME",
            "XDG_CACHE_HOME",
            "XDG_DATA_HOME",
        }:
            self.assertNotIn(key, actual)
        for key in ("HOME", "XDG_CONFIG_HOME", "XDG_CACHE_HOME", "XDG_DATA_HOME"):
            self.assertTrue(Path(actual[key]).is_relative_to(provider.home))

    def test_private_home_replacement_is_rejected(self):
        provider = NativeWineProvider.__new__(NativeWineProvider)
        provider.directory = Path("owned-harness")
        provider.home = MagicMock()
        provider.home.parent = provider.directory
        provider.home_identity = (7, 8, 9)
        provider.home.lstat.return_value = SimpleNamespace(
            st_dev=7, st_ino=10, st_uid=9, st_mode=stat.S_IFDIR | 0o700
        )
        with (
            patch("wine_lifecycle.os.getuid", return_value=9, create=True),
            self.assertRaisesRegex(RuntimeError, "HOME ownership changed"),
        ):
            provider.assert_home()

    def test_finite_running_client_timeout_kills_only_original_and_settles(self):
        started = time.monotonic()
        with self.assertRaises(WineCommandFailure) as failure:
            bounded_client(
                [
                    sys.executable,
                    "-c",
                    "import time; print('fixture', flush=True); time.sleep(30)",
                ],
                dict(os.environ),
                0.15,
            )
        self.assertLess(time.monotonic() - started, 2.8)
        self.assertTrue(failure.exception.metadata["direct_child_killed"])
        self.assertTrue(failure.exception.metadata["pipes_and_child_settled"])
        self.assertIsNotNone(failure.exception.metadata["exit"])

    def test_finite_client_fault_retains_metadata_without_raw_output(self):
        with self.assertRaises(WineCommandFailure) as failure:
            bounded_client(
                [
                    sys.executable,
                    "-c",
                    "import sys; print('private-fixture-value', file=sys.stderr); "
                    "sys.exit(7)",
                ],
                dict(os.environ),
                2,
            )
        self.assertEqual(failure.exception.metadata["exit"], 7)
        self.assertFalse(failure.exception.metadata["direct_child_killed"])
        self.assertGreater(failure.exception.metadata["stderr_bytes"], 0)
        self.assertNotIn("private-fixture-value", str(failure.exception))
        self.assertNotIn(
            "private-fixture-value", json.dumps(failure.exception.metadata)
        )
        with (
            patch(
                "wine_lifecycle.subprocess.Popen", side_effect=OSError("fixture fault")
            ),
            self.assertRaises(WineCommandFailure) as launch,
        ):
            bounded_client(["never-launched"], {}, 0.1)
        self.assertEqual(launch.exception.metadata, {"reason": "launch-failed"})

    def test_fully_settled_late_success_still_fails_its_deadline(self):
        process = MagicMock(pid=321)
        process.communicate.return_value = (b"late-success", b"")
        process.poll.return_value = 0
        with (
            patch("wine_lifecycle.subprocess.Popen", return_value=process),
            patch("wine_lifecycle.time.monotonic", side_effect=[0, 2, 2]),
            self.assertRaises(WineCommandFailure) as failure,
        ):
            bounded_client(["fake-finite-tool"], {}, 1)
        self.assertEqual(failure.exception.metadata["reason"], "deadline")
        self.assertTrue(failure.exception.metadata["pipes_and_child_settled"])
        process.kill.assert_not_called()

    def test_exited_client_retained_pipes_fail_bounded_without_descendant_kill(
        self,
    ):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            ready, release, exited = (
                root / name for name in ("ready", "release", "exited")
            )
            descendant = (
                "import os,time; from pathlib import Path; "
                + "ready=Path("
                + repr(str(ready))
                + "); "
                + "release=Path("
                + repr(str(release))
                + "); "
                + "exited=Path("
                + repr(str(exited))
                + "); "
                + "ready.write_text(str(os.getpid())); deadline=time.monotonic()+12\n"
                + "while not release.exists() and time.monotonic()<deadline: "
                "time.sleep(.02)\n" + "exited.write_text('natural-exit')\n"
            )
            client = (
                "import os,subprocess,sys,time; from pathlib import Path; "
                + "subprocess.Popen([sys.executable,'-c',"
                + repr(descendant)
                + "]); "
                + "ready=Path("
                + repr(str(ready))
                + "); deadline=time.monotonic()+4\n"
                + "while not ready.exists() and time.monotonic()<deadline: "
                "time.sleep(.01)\n" + "os._exit(0 if ready.exists() else 8)\n"
            )
            retained_before = len(UNSETTLED_CLIENTS)
            started = time.monotonic()
            try:
                with self.assertRaises(WineCommandFailure) as failure:
                    bounded_client([sys.executable, "-c", client], dict(os.environ), 1)
                self.assertLess(time.monotonic() - started, 3.8)
                self.assertTrue(ready.exists())
                self.assertFalse(exited.exists())
                self.assertEqual(failure.exception.metadata["exit"], 0)
                self.assertFalse(failure.exception.metadata["direct_child_killed"])
                self.assertFalse(failure.exception.metadata["pipes_and_child_settled"])
            finally:
                # Only this synthetic fixture's private release marker. Descendant
                # exits naturally; production helper never signals it or its tree.
                release.write_text("owned-fixture-release")
                for handle in UNSETTLED_CLIENTS[retained_before:]:
                    _, fixture_stderr = handle.communicate(timeout=3)
                    if handle.returncode != 0:
                        # This subprocess executes only the synthetic script
                        # above: retain its actual command fault, no secret data.
                        print(
                            "Synthetic retained-pipe client fault: "
                            + fixture_stderr.decode()
                        )
                del UNSETTLED_CLIENTS[retained_before:]
            self.assertEqual(exited.read_text(), "natural-exit")

    def test_wine_readiness_requires_backend_and_authentication_readbacks(self):
        for failure in (
            "initial-backend",
            "authentication",
            "final-backend",
            "success",
        ):
            with (
                self.subTest(control=failure),
                tempfile.TemporaryDirectory() as temporary,
            ):
                owned = services.Services(temporary, "smoke", wine=True)
                owned.wine_provider = Provider([{654: "agent-bus-http.exe"}] * 2)
                owned.env.update(
                    AGENT_BUS_REDIS_URL="redis://localhost:49153/0",
                    AGENT_BUS_DATABASE_URL="postgresql://postgres:dummy@localhost:49154/owned",
                )
                health = self.health(nonce=owned.run_id)
                health.update(
                    ok=True,
                    database_ok=True,
                    storage_ready=True,
                    redis_url=owned.env["AGENT_BUS_REDIS_URL"],
                    database_url=owned.env["AGENT_BUS_DATABASE_URL"],
                )
                last_health = dict(health)
                if failure == "initial-backend":
                    health["redis_url"] = "redis://localhost:49155/0"
                if failure == "final-backend":
                    last_health["database_url"] = (
                        "postgresql://postgres:dummy@localhost:49154/foreign"
                    )
                reads = [
                    health,
                    RuntimeError("authentication failed")
                    if failure == "authentication"
                    else {},
                    last_health,
                ]
                launcher = MagicMock(pid=321)
                launcher.poll.return_value = None
                try:
                    with (
                        patch.object(services, "unused_port", return_value=49152),
                        patch.object(
                            services.subprocess, "Popen", return_value=launcher
                        ),
                        patch.object(services, "read_json", side_effect=reads) as read,
                    ):
                        if failure == "success":
                            owned.start_http(Path("synthetic-wrapper"))
                            self.assertTrue(owned.wine_guest.ready)
                            self.assertEqual(
                                read.call_args_list[1].args,
                                ("http://localhost:49152/admin/service", owned.token),
                            )
                        else:
                            with self.assertRaises(RuntimeError):
                                owned.start_http(Path("synthetic-wrapper"))
                            self.assertFalse(owned.wine_guest.ready)
                            if failure == "initial-backend":
                                self.assertEqual(read.call_count, 1)
                finally:
                    if owned.http_log:
                        owned.http_log.close()


class FailureRetention(unittest.TestCase):
    @unittest.skipUnless(sys.platform == "linux", "Linux prefix permission control")
    def test_failed_child_streams_survive_runner_temp_cleanup(self):
        """Protects: private failure evidence survives runner cleanup."""
        with tempfile.TemporaryDirectory() as root:
            root = Path(root)
            runner = root / "runner-temp"
            runner.mkdir(mode=0o700)
            prefix = root / "retained-prefix"
            prefix.mkdir(mode=0o700)
            home = runner / "wine-home"
            home.mkdir(mode=0o700)
            p = NativeWineProvider.__new__(NativeWineProvider)
            p.directory = runner
            p.home = home
            p.prefix = prefix
            p.env = {}
            p.identity = (prefix.stat().st_dev, prefix.stat().st_ino, os.getuid())
            p.home_identity = (home.stat().st_dev, home.stat().st_ino, os.getuid())
            payload = (
                b"test fixture::fails ... FAILED\r\nsecret=DO_NOT_PUBLISH\r\n"
                b"test result: FAILED. 0 passed; 1 failed; 0 ignored; "
                b"0 measured; 0 filtered out; finished in 0.01s\r\n"
            )
            try:
                p.command(
                    [
                        sys.executable,
                        "-I",
                        "-c",
                        "import os;os.write(1,"
                        + repr(payload)
                        + ");os.write(2,b'private-password');raise SystemExit(7)",
                    ],
                    env={},
                    timeout=5,
                )
            except WineCommandFailure as failure:
                assert failure.metadata["exit"] == 7
                summary = wine_job.failure_summary(failure)
                assert summary["failed_tests"] == ["fixture::fails"]
                assert len(summary["test_results"]) == 1
                assert "DO_NOT_PUBLISH" not in json.dumps(summary)
                assert "private-password" not in json.dumps(summary)
                shutil.rmtree(runner)
                metadata = json.loads(next(prefix.glob("*.json")).read_text())
                for name, expected in [
                    ("stdout", payload),
                    ("stderr", b"private-password"),
                ]:
                    leaf = Path(metadata[name + "_path"])
                    assert leaf.read_bytes() == expected
                    assert (
                        leaf.stat().st_uid == os.getuid()
                        and stat.S_IMODE(leaf.stat().st_mode) == 0o600
                    )
                    assert (
                        metadata[name + "_sha256"]
                        == hashlib.sha256(expected).hexdigest()
                    )

            else:
                raise AssertionError("Expected real failing child")


class RetentionFaults(unittest.TestCase):
    def test_partial_streams_retained_if_direct_child_kill_fails(self):
        """Protects: original kill failure retains already captured bytes."""
        process = MagicMock()
        process.poll.return_value = None
        process.communicate.side_effect = subprocess.TimeoutExpired(
            "synthetic", 1, output=b"partial stdout", stderr=b"partial stderr"
        )
        process.kill.side_effect = OSError("synthetic kill refusal")
        with patch("wine_lifecycle.subprocess.Popen", return_value=process):
            with self.assertRaises(WineCommandFailure) as caught:
                bounded_client(["synthetic"], {}, 1)
        self.assertEqual(caught.exception.stdout, b"partial stdout")
        self.assertEqual(caught.exception.stderr, b"partial stderr")
        self.assertEqual(
            caught.exception.metadata["reason"], "direct-child-kill-failed"
        )
        UNSETTLED_CLIENTS.remove(process)

    def test_retention_write_fault_preserves_original_child_failure(self):
        """Protects: diagnostic write faults do not mask the failed child."""
        provider = NativeWineProvider.__new__(NativeWineProvider)
        provider.assert_owner = MagicMock()
        provider.assert_home = MagicMock()
        provider.prefix = Path("synthetic-prefix")
        provider.env = {}
        failure = WineCommandFailure(
            {"reason": "nonzero-exit", "exit": 7}, b"private", b""
        )
        with patch("wine_lifecycle.bounded_client", side_effect=failure):
            with patch("wine_lifecycle.os.open", side_effect=OSError("SECRET")):
                with self.assertRaises(WineCommandFailure) as caught:
                    provider.command(["synthetic"])
        self.assertIs(caught.exception, failure)
        self.assertEqual(failure.metadata["retention_failure_type"], "OSError")
        self.assertNotIn("SECRET", json.dumps(wine_job.failure_summary(failure)))


if __name__ == "__main__":
    unittest.main()
