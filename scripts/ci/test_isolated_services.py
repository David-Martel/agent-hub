"""Isolation contract tests: no Docker daemon, sockets, or live agent-bus calls."""

import contextlib
import importlib.util
import io
import subprocess
import sys
import tempfile
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
                        stdout=f"password {owned.password}\ntest result: ok. 73 passed; 0 failed;\n",
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


if __name__ == "__main__":
    unittest.main()
