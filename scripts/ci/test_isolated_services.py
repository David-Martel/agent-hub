"""Isolation contract tests: no Docker daemon, sockets, or live agent-bus calls."""

import contextlib
import importlib.util
import io
import math
import subprocess
import sys
import tempfile
import traceback
import unittest
from pathlib import Path
from types import SimpleNamespace
from unittest.mock import MagicMock, patch

SPEC = importlib.util.spec_from_file_location(
    "isolated_services", Path(__file__).with_name("isolated-services.py")
)
services = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(services)


class IsolationTests(unittest.TestCase):
    def test_native_smoke_cannot_resolve_the_operator_journal(self):
        with tempfile.TemporaryDirectory() as directory:
            operator = Path(directory) / "operator"
            operator.mkdir()
            journal = operator / "agent-bus-outbox" / "journal.jsonl"
            journal.parent.mkdir()
            journal.write_bytes(b"operator custody sentinel")
            owned = Path(directory) / "fixture"
            owned.mkdir()
            with patch.dict(
                services.os.environ,
                {"HOME": str(operator), "LOCALAPPDATA": str(operator)},
            ):
                fixture = services.Services(owned, "smoke")
            for variable in ("HOME", "LOCALAPPDATA"):
                root = Path(fixture.env[variable])
                self.assertTrue(root.is_relative_to(owned))
                self.assertTrue(root.is_dir())
                self.assertFalse((root / "agent-bus-outbox" / "journal.jsonl").exists())
            self.assertEqual(journal.read_bytes(), b"operator custody sentinel")

    def test_native_backend_tests_have_a_private_windows_journal_root(self):
        with tempfile.TemporaryDirectory() as directory:
            with patch.dict(services.os.environ, {"LOCALAPPDATA": "operator"}):
                fixture = services.Services(directory, "integration")
            self.assertEqual(
                Path(fixture.env["LOCALAPPDATA"]), Path(directory) / "owned-home"
            )

    def test_main_preserves_first_failure_when_owned_cleanup_also_fails(self):
        owned = MagicMock()
        owned.start.side_effect = RuntimeError("first readiness failure")
        owned.close.side_effect = RuntimeError("owned fixture cleanup failure")
        output = io.StringIO()
        with (
            patch.object(sys, "argv", ["isolated-services.py", "history"]),
            patch.object(services.shutil, "which", return_value="cargo"),
            patch.object(services, "require_local_docker"),
            patch.object(services, "Services", return_value=owned),
            contextlib.redirect_stdout(output),
            self.assertRaisesRegex(RuntimeError, "first readiness failure"),
        ):
            services.main()
        owned.close.assert_called_once()
        self.assertIn(
            "Cleanup also failed: owned fixture cleanup failure", output.getvalue()
        )

    def test_ambient_agent_settings_are_removed(self):
        env = services.clean_environment(
            {
                "PATH": "tools",
                "AGENT_BUS_SERVER_URL": "http://shared:8400",
                "AGENT_BUS_DATABASE_URL": "private",
                "AGENT_BUS_CONFIG": "user.json",
                "AGENT_BUS_TEST_RUN_ID": "old",
                "AGENT_BUS_BUILD_REVISION": "abc",
                "https_proxy": "http://unexpected-proxy",
            }
        )
        self.assertEqual(
            env,
            {
                "PATH": "tools",
                "AGENT_BUS_BUILD_REVISION": "abc",
                "NO_PROXY": "localhost,127.0.0.1,::1",
            },
        )

    def test_no_docker_is_a_failure_not_a_skip(self):
        with (
            patch.object(services.shutil, "which", return_value=None),
            self.assertRaisesRegex(RuntimeError, "Docker is required"),
        ):
            services.require_local_docker()

    def test_remote_docker_is_rejected_before_any_command(self):
        with (
            patch.dict(
                services.os.environ, {"DOCKER_HOST": "ssh://another-host"}, clear=True
            ),
            patch.object(services.shutil, "which", return_value="docker"),
            patch.object(services, "run") as run,
        ):
            with self.assertRaisesRegex(RuntimeError, "local Docker"):
                services.require_local_docker()
            run.assert_not_called()

    def test_cleanup_only_captured_ids_even_after_partial_create(self):
        with tempfile.TemporaryDirectory() as directory:
            owned = services.Services(directory, "integration")
            owned.containers = ["a" * 64]
            cidfile = Path(directory) / "created.cid"
            cidfile.write_text("b" * 64)
            invalid = Path(directory) / "invalid.cid"
            invalid.write_text("shared-bus")
            unrelated = Path(directory) / "unregistered.cid"
            unrelated.write_text("c" * 64)
            owned.cid_files = [cidfile, invalid]
            with patch.object(services, "run") as run:
                owned.close()
            self.assertEqual(
                [call.args[0] for call in run.call_args_list],
                [
                    ["docker", "rm", "--force", "--volumes", "a" * 64],
                    ["docker", "rm", "--force", "--volumes", "b" * 64],
                ],
            )

    def test_cleanup_continues_after_http_or_container_failure(self):
        with tempfile.TemporaryDirectory() as directory:
            owned = services.Services(directory, "integration")
            owned.http = MagicMock()
            owned.http.poll.return_value = None
            owned.http.terminate.side_effect = OSError("termination failure")
            owned.containers = ["a" * 64, "b" * 64]
            with (
                patch.object(
                    services,
                    "run",
                    side_effect=[
                        subprocess.CalledProcessError(1, "docker"),
                        None,
                    ],
                ) as run,
                self.assertRaisesRegex(RuntimeError, "clean owned resources"),
            ):
                owned.close()
            self.assertEqual(run.call_count, 2)

    def test_all_workspace_backend_targets_run_serial_and_enforce_floor(self):
        owned = SimpleNamespace(
            env={},
            start_http=MagicMock(),
            password="private-password",  # pragma: allowlist secret
            token="private-token",  # pragma: allowlist secret
        )
        with (
            patch.object(
                services,
                "run",
                return_value=SimpleNamespace(
                    stdout="test result: ok. 73 passed; 0 failed;\n", stderr=""
                ),
            ) as run,
            patch.object(services, "binary_path", return_value=Path("owned-http")),
        ):
            services.execute("integration", owned)
        command = run.call_args.args[0]
        self.assertEqual(
            command,
            [
                "cargo",
                "test",
                "--workspace",
                "--tests",
                "--no-fail-fast",
                "--",
                "--ignored",
                "--test-threads=1",
            ],
        )
        owned.start_http.assert_called_once_with(Path("owned-http"))

    def test_backend_test_floor_rejects_empty_or_partial_success(self):
        owned = SimpleNamespace(
            env={},
            start_http=MagicMock(),
            password="private-password",  # pragma: allowlist secret
            token="private-token",  # pragma: allowlist secret
        )
        for output in ["", "test result: ok. 72 passed; 0 failed;\n"]:
            with (
                patch.object(
                    services,
                    "run",
                    return_value=SimpleNamespace(stdout=output, stderr=""),
                ),
                patch.object(services, "binary_path", return_value=Path("owned-http")),
                contextlib.redirect_stdout(io.StringIO()),
                self.assertRaisesRegex(RuntimeError, "required floor is 73"),
            ):
                services.execute("integration", owned)

    def test_successful_backend_suite_redacts_both_output_streams(self):
        with tempfile.TemporaryDirectory() as directory:
            owned = services.Services(directory, "integration")
            owned.start_http = MagicMock()
            output = io.StringIO()
            with (
                patch.object(
                    services,
                    "run",
                    return_value=SimpleNamespace(
                        stdout=(
                            f"password {owned.password}\n"
                            "test result: ok. 73 passed; 0 failed;\n"
                        ),
                        stderr=f"token {owned.token}\n",
                    ),
                ),
                patch.object(services, "binary_path", return_value=Path("owned-http")),
                contextlib.redirect_stdout(output),
            ):
                services.execute("integration", owned)
            self.assertIn("password <redacted>", output.getvalue())
            self.assertIn("token <redacted>", output.getvalue())
            self.assertIn("test result: ok. 73 passed;", output.getvalue())
            self.assertNotIn(owned.password, output.getvalue())
            self.assertNotIn(owned.token, output.getvalue())

    def test_failed_backend_suite_preserves_first_failure_without_credentials(self):
        with tempfile.TemporaryDirectory() as directory:
            owned = services.Services(directory, "integration")
            owned.start_http = MagicMock()
            failure = subprocess.CalledProcessError(
                101,
                "cargo",
                output=f"failed {owned.password}",
                stderr=f"reason {owned.token}",
            )
            output = io.StringIO()
            with (
                patch.object(services, "run", side_effect=[None, failure]),
                patch.object(services, "binary_path", return_value=Path("owned-http")),
                contextlib.redirect_stdout(output),
                self.assertRaises(subprocess.CalledProcessError),
            ):
                services.execute("integration", owned)
            self.assertIn("failed <redacted>", output.getvalue())
            self.assertIn("reason <redacted>", output.getvalue())
            self.assertNotIn(owned.password, output.getvalue())
            self.assertNotIn(owned.token, output.getvalue())

    def test_runner_namespace_fixtures_have_no_host_publish_and_only_owned_cleanup(
        self,
    ):
        with tempfile.TemporaryDirectory() as directory:
            owned = services.Services(
                directory, "integration", network_container="a" * 64
            )
            calls = []

            def run(command, **kwargs):
                calls.append(command)
                if command[1] == "create":
                    Path(command[command.index("--cidfile") + 1]).write_text("b" * 64)
                return SimpleNamespace(stdout="", stderr="")

            with (
                patch.object(services, "run", side_effect=run),
                patch.object(services, "unused_port", return_value=49153),
            ):
                cid, port = owned.create("redis:7-alpine", 6379, [])
                owned.close()
            self.assertEqual((cid, port), ("b" * 64, 49153))
            self.assertIn("container:" + "a" * 64, calls[0])
            self.assertNotIn("--publish", calls[0])
            self.assertEqual(
                calls[0][-5:],
                ["redis-server", "--port", "49153", "--bind", "127.0.0.1"],
            )
            self.assertEqual(
                calls[-1], ["docker", "rm", "--force", "--volumes", "b" * 64]
            )

    def test_history_fresh_database_readiness_uses_runner_namespace_port(self):
        with tempfile.TemporaryDirectory() as directory:
            owned = services.Services(directory, "history", network_container="a" * 64)
            with (
                patch.object(owned, "create", return_value=("b" * 64, 49154)),
                patch.object(owned, "wait_container") as wait,
            ):
                owned.start()
            self.assertEqual(
                wait.call_args.args[1],
                [
                    "pg_isready",
                    "-h",
                    "localhost",
                    "-p",
                    "49154",
                    "-U",
                    "postgres",
                    "-d",
                    "agent_bus_history_" + owned.run_id,
                ],
            )
            self.assertIn(
                "localhost:49154/agent_bus_history_",
                owned.env["AGENT_BUS_TEST_DATABASE_URL"],
            )
            self.assertNotIn("AGENT_BUS_TEST_REDIS_URL", owned.env)

    def test_missing_history_target_is_not_swallowed(self):
        owned = SimpleNamespace(env={"AGENT_BUS_TEST_DATABASE_URL": "owned"})
        with (
            patch.object(
                services, "run", side_effect=subprocess.CalledProcessError(101, "cargo")
            ),
            self.assertRaises(subprocess.CalledProcessError),
        ):
            services.execute("history", owned)

    def test_smoke_uses_owned_server_and_propagates_timeout(self):
        with tempfile.TemporaryDirectory() as directory:
            owned = services.Services(directory, "smoke")
            owned.http = MagicMock(pid=321)
            owned.http.poll.return_value = None
            owned.env.update(
                {
                    "AGENT_BUS_TEST_SERVER_URL": "http://localhost:49152",
                    "AGENT_BUS_SERVICE_AGENT_ID": "agent-bus-test-owned",
                }
            )
            with (
                patch.object(owned, "start_http") as start,
                patch.object(
                    services,
                    "run",
                    side_effect=[
                        None,
                        subprocess.TimeoutExpired("pwsh", 600),
                    ],
                ) as run,
            ):
                try:
                    with self.assertRaises(subprocess.TimeoutExpired):
                        services.execute("smoke", owned, Path("cli"), Path("http"))
                    self.assertIn("-SkipHttp", run.call_args_list[0].args[0])
                    self.assertIn("-UseExistingServer", run.call_args_list[1].args[0])
                    start.assert_called_once_with(Path("http"))
                finally:
                    owned.close()
            owned.http.terminate.assert_called_once()
            owned.http.wait.assert_called_once_with(timeout=10)

    def test_http_identity_mismatch_refuses_admin_request(self):
        with tempfile.TemporaryDirectory() as directory:
            owned = services.Services(directory, "integration")
            owned.env["AGENT_BUS_SERVICE_AGENT_ID"] = "expected"
            process = MagicMock(pid=321)
            process.poll.return_value = None
            with (
                patch.object(services, "unused_port", return_value=49152),
                patch.object(services.subprocess, "Popen", return_value=process),
                patch.object(
                    services,
                    "read_json",
                    return_value={
                        "maintenance": {"pid": 654, "service_agent_id": "shared"},
                    },
                ) as read,
            ):
                try:
                    with self.assertRaisesRegex(RuntimeError, "does not belong"):
                        owned.start_http(Path("owned-http"))
                    self.assertEqual(read.call_count, 1)
                finally:
                    owned.close()

    def test_http_ready_requires_matching_backends_then_authenticated_read(self):
        with tempfile.TemporaryDirectory() as directory:
            owned = services.Services(directory, "integration")
            owned.env.update(
                {
                    "AGENT_BUS_SERVICE_AGENT_ID": "expected",
                    "AGENT_BUS_REDIS_URL": "redis://localhost:49153/0",
                    "AGENT_BUS_DATABASE_URL": "postgresql://postgres:secret@localhost:49154/owned",
                }
            )
            process = MagicMock(pid=321)
            process.poll.return_value = None
            health = {
                "ok": True,
                "database_ok": True,
                "storage_ready": True,
                "maintenance": {"pid": 321, "service_agent_id": "expected"},
                "redis_url": "redis://localhost:49153/0",
                "database_url": "postgresql://postgres:REDACTED@localhost:49154/owned",
            }
            with (
                patch.object(services, "unused_port", return_value=49152),
                patch.object(services.subprocess, "Popen", return_value=process),
                patch.object(services, "read_json", side_effect=[health, {}]) as read,
            ):
                try:
                    owned.start_http(Path("owned-http"))
                    self.assertEqual(read.call_count, 2)
                    self.assertEqual(
                        read.call_args.args,
                        ("http://localhost:49152/admin/service", owned.token),
                    )
                finally:
                    owned.close()


class FakeClock:
    """A monotonic clock that only moves when a faked child or sleep spends time."""

    def __init__(self):
        self.now = 0.0
        self.sleeps = []

    def monotonic(self):
        return self.now

    def sleep(self, seconds):
        self.sleeps.append(seconds)
        self.now += seconds


class DockerPrerequisiteBudgetTests(unittest.TestCase):
    """The Docker prerequisite waits for a slow daemon within a bounded budget.

    agent-hub #242's smoke failed twice on dtm-p1gen9 because `docker info`
    took longer than the fixed 15 s limit; Docker Desktop measured 11.8 s cold
    while idle. These tests fake the child process and the clock, so they spend
    no real time and need no Docker.
    """

    def _probe(self, behaviour, environ=None):
        clock = FakeClock()
        calls = []

        def fake_run(args, *, capture, timeout):
            calls.append(timeout)
            return behaviour(args, timeout, clock, len(calls))

        with (
            patch.dict(services.os.environ, environ or {}, clear=True),
            patch.object(services, "run", side_effect=fake_run),
            patch.object(services.time, "monotonic", side_effect=clock.monotonic),
            patch.object(services.time, "sleep", side_effect=clock.sleep),
        ):
            try:
                result = services._docker_prerequisite("linux-container-info")
            except RuntimeError as caught:
                return clock, calls, caught
        return clock, calls, result

    @staticmethod
    def _answers_after(seconds):
        def behaviour(args, timeout, clock, _attempt):
            if timeout < seconds:
                clock.now += timeout
                raise subprocess.TimeoutExpired(args, timeout)
            clock.now += seconds
            return subprocess.CompletedProcess(args, 0, "linux\n", "")

        return behaviour

    def test_a_docker_answering_after_20s_is_waited_for(self):
        """Protects: a slow but healthy Docker passes the prerequisite.

        Detects: a per-call limit below the daemon's answer time, which kills
        the probe before it answers (the old fixed 15 s failed this case).
        Needs: faked run() and clock only.
        Breadcrumb: scripts/ci/isolated-services.py _docker_prerequisite.
        """
        clock, calls, result = self._probe(self._answers_after(20))
        self.assertIsInstance(result, subprocess.CompletedProcess, result)
        self.assertEqual(result.stdout.strip(), "linux")
        self.assertEqual(len(calls), 1)
        self.assertGreaterEqual(calls[0], 20)
        self.assertEqual(clock.now, 20)

    def test_a_docker_that_never_answers_fails_within_the_budget(self):
        """Protects: a hung Docker still fails, and says so plainly, in bounded time.

        Detects: retrying after a timeout (which would overrun the budget) or
        an unbounded wait. Needs: faked run() and clock only.
        Breadcrumb: scripts/ci/isolated-services.py _docker_prerequisite.
        """
        clock, calls, caught = self._probe(self._answers_after(math.inf))
        self.assertIsInstance(caught, RuntimeError)
        self.assertLessEqual(clock.now, services.DOCKER_PREREQUISITE_TIMEOUT)
        self.assertEqual(len(calls), 1)
        self.assertEqual(
            str(caught),
            "Docker prerequisite linux-container-info failed (timed out after "
            "60.0s) after 1 attempt in 60.0s of a 60s budget (raise it with "
            "ISOLATED_SERVICES_DOCKER_TIMEOUT); no fixtures created",
        )

    def test_a_refusing_daemon_is_retried_with_backoff_until_it_answers(self):
        """Protects: Docker Desktop still starting (a quick refusal) is retried.

        Detects: failing on the first non-zero exit, or retrying without
        backing off. Needs: faked run() and clock only.
        """

        def behaviour(args, timeout, clock, attempt):
            clock.now += 0.5
            if attempt < 3:
                raise subprocess.CalledProcessError(1, args)
            return subprocess.CompletedProcess(args, 0, "linux\n", "")

        clock, calls, result = self._probe(behaviour)
        self.assertIsInstance(result, subprocess.CompletedProcess, result)
        self.assertEqual(clock.sleeps, [1.0, 2.0])
        self.assertEqual(len(calls), 3)
        self.assertEqual(calls[1], 60 - 0.5 - 1.0)

    def test_a_daemon_that_keeps_refusing_stops_inside_the_budget(self):
        """Protects: retries end, with the attempt count, before the budget runs out.

        Detects: an unbounded retry loop or a sleep that overruns the budget.
        Needs: faked run() and clock only.
        """

        def behaviour(args, timeout, clock, _attempt):
            clock.now += 0.5
            raise subprocess.CalledProcessError(1, args)

        clock, calls, caught = self._probe(behaviour)
        self.assertIsInstance(caught, RuntimeError)
        self.assertLessEqual(clock.now, services.DOCKER_PREREQUISITE_TIMEOUT)
        self.assertEqual(clock.sleeps[:4], [1.0, 2.0, 4.0, 8.0])
        self.assertTrue(all(delay == 8.0 for delay in clock.sleeps[4:]))
        self.assertIn(f"(native exit 1) after {len(calls)} attempts", str(caught))

    def test_the_budget_is_overridable_and_a_bad_override_is_an_error(self):
        """Protects: a runner can raise or lower the budget, and a typo is refused.

        Detects: an override that is ignored (here a 5 s budget must fail a
        Docker that answers after 20 s), or a bad value silently replaced by a
        default. Needs: faked run() and clock only.
        """
        clock, calls, caught = self._probe(
            self._answers_after(20), {"ISOLATED_SERVICES_DOCKER_TIMEOUT": "5"}
        )
        self.assertIsInstance(caught, RuntimeError)
        self.assertEqual(calls, [5.0])
        self.assertIn("of a 5s budget", str(caught))
        clock, calls, result = self._probe(
            self._answers_after(90), {"ISOLATED_SERVICES_DOCKER_TIMEOUT": "120"}
        )
        self.assertIsInstance(result, subprocess.CompletedProcess, result)
        for value in ("", "  "):
            self.assertEqual(
                services.docker_prerequisite_budget(
                    {"ISOLATED_SERVICES_DOCKER_TIMEOUT": value}
                ),
                60.0,
            )
        for value in ("abc", "0", "-1", "nan", "inf", "601"):
            with self.subTest(value=value):
                with self.assertRaisesRegex(
                    RuntimeError, "ISOLATED_SERVICES_DOCKER_TIMEOUT must be seconds"
                ):
                    services.docker_prerequisite_budget(
                        {"ISOLATED_SERVICES_DOCKER_TIMEOUT": value}
                    )


class DockerPrerequisiteTests(unittest.TestCase):
    def test_prerequisite_success_uses_exact_fixed_commands_and_finite_timeout(self):
        for operation, args in (
            ("context-inspect", ["docker", "context", "inspect"]),
            ("linux-container-info", ["docker", "info", "--format", "{{.OSType}}"]),
        ):
            with self.subTest(operation=operation):
                completed = subprocess.CompletedProcess(args, 0, "inert", "")
                with (
                    patch.dict(services.os.environ, {}, clear=True),
                    patch.object(services.time, "monotonic", return_value=0.0),
                    patch.object(services.shutil, "which", return_value="owned-docker"),
                    patch.object(
                        services.subprocess, "run", return_value=completed
                    ) as native,
                ):
                    self.assertIs(services._docker_prerequisite(operation), completed)
                native.assert_called_once_with(
                    ["owned-docker", *args[1:]],
                    cwd=services.ROOT,
                    env=None,
                    check=True,
                    text=True,
                    stdout=subprocess.PIPE,
                    stderr=subprocess.PIPE,
                    timeout=60.0,
                    creationflags=(
                        subprocess.CREATE_NO_WINDOW if services.os.name == "nt" else 0
                    ),
                )

    def test_a_slow_readiness_probe_is_retried_not_fatal(self):
        """Protects: one slow `docker exec` readiness probe does not abort the run.

        Detects: wait_container retrying only CalledProcessError, so a single
        probe over its 5 s limit raised TimeoutExpired and failed the whole
        fixture start (agent-hub main 15392c9, 32 s into the step).
        Needs: no Docker; run() and sleep are faked.
        Breadcrumb: scripts/ci/isolated-services.py Services.wait_container.
        """
        with tempfile.TemporaryDirectory() as directory:
            owned = services.Services(directory, "smoke")
            probes = [
                subprocess.TimeoutExpired(["docker", "exec"], 5),
                subprocess.CalledProcessError(1, ["docker", "exec"]),
                None,
            ]
            with (
                patch.object(services, "run", side_effect=probes) as probe,
                patch.object(services.time, "sleep"),
            ):
                owned.wait_container("c" * 64, ["pg_isready"])
            self.assertEqual(probe.call_count, 3)

    def test_failed_fixture_docker_command_is_named_in_the_failure(self):
        """Protects: a red smoke run names its cause (agent-hub main, 2026-10-29).

        Detects: a fixture ``docker`` failure printing only ``CalledProcessError``,
        indistinguishable from a failing test script, and the disposable password
        or token leaking through argv or the output tail.
        Needs: no Docker; ``start`` is faked.
        Breadcrumb: scripts/ci/isolated-services.py.
        """
        with tempfile.TemporaryDirectory() as directory:
            owned = services.Services(directory, "smoke")
            failure = subprocess.CalledProcessError(
                125,
                [
                    "C:\\Docker\\docker.exe",
                    "create",
                    "--env",
                    f"POSTGRES_PASSWORD={owned.password}",
                ],
                output="",
                stderr=f"pull access denied\ntoken {owned.token}\ntoomanyrequests",
            )
            owned.start = MagicMock(side_effect=failure)
            cli = Path(directory) / "agent-bus.exe"
            http = Path(directory) / "agent-bus-http.exe"
            cli.write_text("x")
            http.write_text("x")
            output = io.StringIO()
            with (
                patch.object(
                    sys,
                    "argv",
                    [
                        "isolated-services.py",
                        "smoke",
                        "--cli",
                        str(cli),
                        "--http",
                        str(http),
                    ],
                ),
                patch.object(services.shutil, "which", return_value="pwsh"),
                patch.object(services, "require_local_docker"),
                patch.object(services, "Services", return_value=owned),
                contextlib.redirect_stdout(output),
                self.assertRaises(subprocess.CalledProcessError),
            ):
                services.main()
            text = output.getvalue()
            self.assertIn("Child command failed: docker create (exit 125)", text)
            self.assertIn("pull access denied", text)
            self.assertIn("toomanyrequests", text)
            self.assertNotIn(owned.password, text)
            self.assertNotIn(owned.token, text)

    def test_native_failure_and_timeout_report_operation_without_native_details(self):
        marker = "inert-sensitive-fixture-marker"
        for operation in ("context-inspect", "linux-container-info"):
            for failure, detail in (
                (
                    subprocess.CalledProcessError(
                        7, [marker], output=marker, stderr=marker
                    ),
                    "native exit 7",
                ),
                (
                    subprocess.TimeoutExpired(
                        [marker], 60.0, output=marker, stderr=marker
                    ),
                    "timed out after 60.0s",
                ),
                (OSError(marker), "OSError"),
                (RuntimeError(marker), "RuntimeError"),
            ):
                clock = FakeClock()
                with (
                    self.subTest(operation=operation, detail=detail),
                    patch.dict(services.os.environ, {}, clear=True),
                    patch.object(
                        services.time, "monotonic", side_effect=clock.monotonic
                    ),
                    patch.object(services.time, "sleep", side_effect=clock.sleep),
                    patch.object(services.shutil, "which", return_value="owned-docker"),
                    patch.object(services.subprocess, "run", side_effect=failure),
                ):
                    try:
                        services._docker_prerequisite(operation)
                    except RuntimeError as caught:
                        # A refusal is retried until the next backoff no longer
                        # fits; the fake clock moves only by those sleeps.
                        attempts = len(clock.sleeps) + 1
                        self.assertEqual(
                            str(caught),
                            f"Docker prerequisite {operation} failed ({detail}) "
                            f"after {attempts} attempt{'s' if attempts != 1 else ''} "
                            f"in {clock.now:.1f}s of a 60s budget (raise it with "
                            "ISOLATED_SERVICES_DOCKER_TIMEOUT); no fixtures created",
                        )
                        self.assertIs(caught.__context__, failure)
                        self.assertTrue(caught.__suppress_context__)
                        self.assertNotIn(
                            marker, "".join(traceback.format_exception(caught))
                        )
                    else:
                        self.fail("Expected safe prerequisite rejection absent")

    def test_context_precedence_and_local_endpoint_requirements_remain_exact(self):
        local_endpoints = (
            "unix:///var/run/docker.sock",
            "npipe:////./pipe/docker_engine",
            "tcp://localhost:2375",
            "tcp://127.0.0.1:2375",
            "tcp://[::1]:2375",
        )
        for endpoint in local_endpoints:
            with self.subTest(endpoint=endpoint):
                context = json_context(endpoint)
                with (
                    patch.dict(
                        services.os.environ,
                        {
                            "DOCKER_CONTEXT": "explicit-inert-context",
                            "DOCKER_HOST": "ssh://inert-remote",
                        },
                        clear=True,
                    ),
                    patch.object(services.time, "monotonic", return_value=0.0),
                    patch.object(services.shutil, "which", return_value="owned-docker"),
                    patch.object(
                        services.subprocess,
                        "run",
                        side_effect=[
                            subprocess.CompletedProcess([], 0, context, ""),
                            subprocess.CompletedProcess([], 0, "linux\n", ""),
                        ],
                    ) as native,
                ):
                    services.require_local_docker()
                self.assertEqual(
                    [call.args[0] for call in native.call_args_list],
                    [
                        ["owned-docker", "context", "inspect"],
                        ["owned-docker", "info", "--format", "{{.OSType}}"],
                    ],
                )
                self.assertEqual(
                    [call.kwargs["timeout"] for call in native.call_args_list],
                    [60.0, 60.0],
                )

    def test_explicit_local_host_skips_context_and_requires_linux(self):
        for os_type in ("linux\n", "windows\n"):
            with self.subTest(os_type=os_type):
                with (
                    patch.dict(
                        services.os.environ,
                        {"DOCKER_HOST": "unix:///inert/docker.sock"},
                        clear=True,
                    ),
                    patch.object(services.shutil, "which", return_value="owned-docker"),
                    patch.object(
                        services.subprocess,
                        "run",
                        return_value=subprocess.CompletedProcess([], 0, os_type, ""),
                    ) as native,
                ):
                    if os_type == "linux\n":
                        services.require_local_docker()
                    else:
                        with self.assertRaisesRegex(RuntimeError, "Linux containers"):
                            services.require_local_docker()
                self.assertEqual(
                    native.call_args.args[0],
                    ["owned-docker", "info", "--format", "{{.OSType}}"],
                )
                self.assertEqual(native.call_count, 1)

    def test_explicit_remote_context_rejects_even_when_host_override_is_local(self):
        with (
            patch.dict(
                services.os.environ,
                {"DOCKER_CONTEXT": "remote", "DOCKER_HOST": "unix:///inert.sock"},
                clear=True,
            ),
            patch.object(services.shutil, "which", return_value="owned-docker"),
            patch.object(
                services.subprocess,
                "run",
                return_value=subprocess.CompletedProcess(
                    [], 0, json_context("ssh://inert-remote"), ""
                ),
            ) as native,
            self.assertRaisesRegex(RuntimeError, "local Docker"),
        ):
            services.require_local_docker()
        self.assertEqual(native.call_count, 1)
        self.assertEqual(
            native.call_args.args[0], ["owned-docker", "context", "inspect"]
        )

    def test_main_prerequisite_failure_creates_no_fixture_directory_or_services(self):
        for operation in ("context-inspect", "linux-container-info"):
            for timeout in (False, True):
                with self.subTest(operation=operation, timeout=timeout):
                    failure = (
                        subprocess.TimeoutExpired(["inert-marker"], 15)
                        if timeout
                        else subprocess.CalledProcessError(7, ["inert-marker"])
                    )
                    context = subprocess.CompletedProcess(
                        [], 0, json_context("unix:///inert.sock"), ""
                    )
                    first = [] if operation == "context-inspect" else [context]

                    def replies(*_args, _first=first, _failure=failure, **_kwargs):
                        if _first:
                            return _first.pop()
                        raise _failure

                    clock = FakeClock()
                    with (
                        patch.object(
                            services.time, "monotonic", side_effect=clock.monotonic
                        ),
                        patch.object(services.time, "sleep", side_effect=clock.sleep),
                        patch.dict(services.os.environ, {}, clear=True),
                        patch.object(sys, "argv", ["isolated-services.py", "history"]),
                        patch.object(
                            services.shutil, "which", return_value="owned-tool"
                        ),
                        patch.object(services.subprocess, "run", side_effect=replies),
                        patch.object(
                            services.tempfile, "TemporaryDirectory"
                        ) as directory,
                        patch.object(services, "Services") as fixtures,
                        self.assertRaisesRegex(
                            RuntimeError, f"Docker prerequisite {operation} failed"
                        ),
                    ):
                        services.main()
                    directory.assert_not_called()
                    fixtures.assert_not_called()

    def _inert_child(self, code, *, timeout, expected_exit=None, failure_type=None):
        originals = []
        original_popen = subprocess.Popen

        def create_original(*args, **kwargs):
            child = original_popen(*args, **kwargs)
            originals.append(child)
            return child

        # sys.base_prefix identifies the base installation. A Windows PATH copy
        # of sys.executable can require ambient PATH to locate its runtime DLLs.
        interpreter = (
            Path(sys.base_prefix) / "python.exe"
            if services.os.name == "nt"
            else Path(sys.executable)
        )
        self.assertTrue(interpreter.is_file(), "Base runtime interpreter is absent")
        command = [str(interpreter), "-I", "-S", "-B", "-W", "error", "-c", code]
        public_environment = {
            key: services.os.environ[key]
            for key in ("SystemRoot", "WINDIR", "SystemDrive")
            if key in services.os.environ
        }
        with patch.object(services.subprocess, "Popen", side_effect=create_original):
            expectation = (
                self.assertRaises(failure_type)
                if failure_type is not None
                else contextlib.nullcontext()
            )
            with expectation as caught:
                result = services.run(
                    command, env=public_environment, capture=True, timeout=timeout
                )
        self.assertEqual(len(originals), 1)
        child = originals[0]
        self.assertIsNotNone(child.returncode)
        self.assertTrue(child.stdout.closed)
        self.assertTrue(child.stderr.closed)
        if expected_exit is not None:
            self.assertEqual(child.returncode, expected_exit)
        else:
            self.assertNotEqual(child.returncode, 0)
        return caught.exception if failure_type is not None else result

    def test_real_run_boundary_success_reaps_original_and_closes_captures(self):
        result = self._inert_child(
            "import sys; print('inert stdout'); print('inert stderr', file=sys.stderr)",
            timeout=5,
            expected_exit=0,
        )
        self.assertEqual(
            (result.stdout, result.stderr), ("inert stdout\n", "inert stderr\n")
        )

    def test_real_run_boundary_native7_reaps_original_and_preserves_exit(self):
        failure = self._inert_child(
            "import sys; print('inert stdout'); "
            "print('inert stderr', file=sys.stderr); sys.exit(7)",
            timeout=5,
            expected_exit=7,
            failure_type=subprocess.CalledProcessError,
        )
        self.assertEqual(failure.returncode, 7)
        self.assertEqual(
            (failure.stdout, failure.stderr), ("inert stdout\n", "inert stderr\n")
        )

    def test_real_run_boundary_timeout_reaps_original_and_closes_captures(self):
        failure = self._inert_child(
            "import time; time.sleep(10)",
            timeout=0.1,
            failure_type=subprocess.TimeoutExpired,
        )
        self.assertEqual(failure.timeout, 0.1)


def json_context(endpoint):
    return services.json.dumps([{"Endpoints": {"docker": {"Host": endpoint}}}])


if __name__ == "__main__":
    unittest.main()
