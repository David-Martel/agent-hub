"""Unit tests for verify_deploy.py and deploy-agentbus-cloud.yml invariants.

Protects: the post-deploy readback fails when the newest deployment is not the
  merged commit, and the deploy workflow isolates npm setup and runner state.
Detects:  a readback that passes on a stale deployment (the mismatch tests are
  the positive control), a deploy job routed to persistent PR runners, or
  setup-npm.sh combined into the same step as npm ci.
Needs:    Python stdlib only (runs under both unittest and pytest), no network.
  Run (from cloud/agentbus):
    python3 -B -m unittest discover -s tools -p "test_verify_deploy.py"
Breadcrumb: .github/workflows/deploy-agentbus-cloud.yml
"""

from __future__ import annotations

import importlib.util
import io
import json
import os
import re
import sys
import unittest
from email.message import Message
from pathlib import Path
from unittest.mock import patch
from urllib.error import HTTPError, URLError

TOOLS_DIR = Path(__file__).resolve().parent
REPO_ROOT = TOOLS_DIR.parents[2]
if str(TOOLS_DIR) not in sys.path:
    sys.path.insert(0, str(TOOLS_DIR))

SPEC = importlib.util.spec_from_file_location(
    "verify_deploy", TOOLS_DIR / "verify_deploy.py"
)
assert SPEC is not None and SPEC.loader is not None
vd = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(vd)

SHA = "a" * 40


def deployment(created: str, message: str, versions=None):
    return {
        "id": "d-" + created,
        "created_on": created,
        "annotations": {"workers/message": message},
        "versions": versions
        if versions is not None
        else [{"version_id": "v-" + created, "percentage": 100}],
    }


def payload(*deployments):
    return {"result": {"deployments": list(deployments)}}


class VerifyDeployTests(unittest.TestCase):
    def test_transport_identifies_verifier_and_keeps_health_unauthenticated(self):
        """Protects: client identification and API-only bearer credentials.
        Detects: urllib's default UA or bearer reuse across endpoint phases.
        Needs: captured stdlib requests, no network or credentials.
        Breadcrumb: verify_deploy.py get_json request headers.
        """
        with patch.object(vd.urllib.request, "urlopen") as transport:
            transport.side_effect = [io.BytesIO(b"{}"), io.BytesIO(b"{}")]
            vd.get_json("https://api.example.invalid", "synthetic-token")
            vd.get_json("https://health.example.invalid")
        api, health = [call.args[0] for call in transport.call_args_list]
        for request in (api, health):
            self.assertEqual(
                request.get_header("User-agent"), "agent-hub-deploy-readback/1.0"
            )
        self.assertEqual(api.get_header("Authorization"), "Bearer synthetic-token")
        self.assertIsNone(health.get_header("Authorization"))
        self.assertTrue(
            all(call.kwargs == {"timeout": 30} for call in transport.call_args_list)
        )

    def test_http_failures_name_phase_status_and_codes_without_response_data(self):
        """Protects: HTTP diagnostics without credential or body disclosure.
        Detects: API/health ambiguity, raw error echoes, or retrying a denied request.
        Needs: synthetic HTTPError bodies and captured stderr; no live endpoints.
        Breadcrumb: verify_deploy.py get_json HTTPError handling and main phases.
        """
        secret = "sensitive-body-url-and-token"
        for phase, health_failure in (("deployments-api", False), ("health", True)):
            with self.subTest(phase=phase):
                failure = HTTPError(
                    "https://example.invalid/?token=" + secret,
                    403,
                    secret,
                    Message(),
                    io.BytesIO(
                        json.dumps(
                            {
                                "errors": [
                                    {"code": 10000, "message": secret},
                                    {"code": secret},
                                    {"code": True},
                                    {"code": -1},
                                    {"code": 10000},
                                ],
                                "extra": secret,
                            }
                        ).encode()
                    ),
                )
                replies: list[HTTPError | io.BytesIO] = [failure]
                if health_failure:
                    replies.insert(
                        0,
                        io.BytesIO(
                            json.dumps(
                                payload(deployment("new", "Source" + SHA))
                            ).encode()
                        ),
                    )
                stderr = io.StringIO()
                with (
                    patch.dict(os.environ, {"CLOUDFLARE_API_TOKEN": secret}),
                    patch.object(
                        vd.urllib.request, "urlopen", side_effect=replies
                    ) as transport,
                    patch("sys.stderr", stderr),
                ):
                    self.assertEqual(
                        vd.main(["--commit", SHA, "--account-id", "acct"]), 1
                    )
                diagnostic = stderr.getvalue()
                self.assertEqual(
                    diagnostic,
                    f"deploy readback FAILED: phase={phase} "
                    "http_status=403 cloudflare_codes=10000\n",
                )
                self.assertNotIn(secret, diagnostic)
                self.assertNotIn("https://", diagnostic)
                self.assertEqual(transport.call_count, 2 if health_failure else 1)

    def test_http_error_body_read_is_bounded(self):
        """Protects: bounded diagnostic response processing and fail-closed denial.
        Detects: unbounded error-body reads while extracting optional API codes.
        Needs: synthetic oversized body with an instrumented read method.
        Breadcrumb: verify_deploy.py HTTP error diagnostic byte limit.
        """

        class Body(io.BytesIO):
            def __init__(self, value: bytes) -> None:
                super().__init__(value)
                self.sizes: list[int | None] = []

            def read(self, size: int | None = -1) -> bytes:
                self.sizes.append(size)
                return super().read(size)

        body = Body(b" " * 20000 + b"sensitive-tail")
        failure = HTTPError(
            "https://example.invalid", 403, "secret-reason", Message(), body
        )
        stderr = io.StringIO()
        with (
            patch.dict(os.environ, {"CLOUDFLARE_API_TOKEN": "fake-token"}),
            patch.object(vd.urllib.request, "urlopen", side_effect=failure),
            patch("sys.stderr", stderr),
        ):
            self.assertEqual(vd.main(["--commit", SHA, "--account-id", "acct"]), 1)
        self.assertEqual(body.sizes, [4096])
        self.assertNotIn("sensitive-tail", stderr.getvalue())
        self.assertNotIn("secret-reason", stderr.getvalue())

    def test_http_code_count_and_malformed_bodies_are_safe(self):
        """Protects: a finite code-only diagnostic even for untrusted error responses.
        Detects: unlimited codes, malformed-body echoes, or string codes.
        Needs: synthetic bounded JSON/HTML/error bodies; no credentials or network.
        Breadcrumb: verify_deploy.py HTTP error code extraction.
        """
        for body, suffix in (
            (
                json.dumps({"errors": [{"code": i} for i in range(12)]}).encode(),
                " cloudflare_codes=0,1,2,3,4,5,6,7",
            ),
            (b"<html>sensitive-body</html>", ""),
            (b'{"errors": [sensitive-body', ""),
            (b'{"errors": "sensitive-body"}', ""),
            (
                (
                    b'{"errors": [{"code": "sensitive-body"}, '
                    b'{"code": 999999999999999999}]}'
                ),
                "",
            ),
        ):
            with self.subTest(body=body[:12]):
                failure = HTTPError(
                    "https://example.invalid",
                    403,
                    "sensitive-reason",
                    Message(),
                    io.BytesIO(body),
                )
                stderr = io.StringIO()
                with (
                    patch.dict(os.environ, {"CLOUDFLARE_API_TOKEN": "fake-token"}),
                    patch.object(vd.urllib.request, "urlopen", side_effect=failure),
                    patch("sys.stderr", stderr),
                ):
                    self.assertEqual(
                        vd.main(["--commit", SHA, "--account-id", "acct"]), 1
                    )
                self.assertEqual(
                    stderr.getvalue(),
                    "deploy readback FAILED: phase=deployments-api "
                    f"http_status=403{suffix}\n",
                )

    def test_transport_and_json_failures_are_phase_safe_and_fail_closed(self):
        """Protects: phase-specific failures without exception or response text.
        Detects: token-bearing URLs leaking through transport/JSON exception messages.
        Needs: synthetic URL errors, invalid JSON and non-object JSON; no network.
        Breadcrumb: verify_deploy.py get_json error mapping.
        """
        secret = "sensitive-token-in-reason"
        for reply, marker in (
            (
                URLError("https://example.invalid/?token=" + secret),
                "transport_error=URLError",
            ),
            (OSError("sensitive-os-error"), "transport_error=OSError"),
            (io.BytesIO(("<html>" + secret).encode()), "invalid_json"),
            (io.BytesIO(b"[]"), "invalid_json"),
        ):
            with self.subTest(marker=marker, reply=type(reply).__name__):
                stderr = io.StringIO()
                with (
                    patch.dict(os.environ, {"CLOUDFLARE_API_TOKEN": secret}),
                    patch.object(
                        vd.urllib.request, "urlopen", side_effect=[reply]
                    ) as transport,
                    patch("sys.stderr", stderr),
                ):
                    self.assertEqual(
                        vd.main(["--commit", SHA, "--account-id", "acct"]), 1
                    )
                self.assertIn("phase=deployments-api", stderr.getvalue())
                self.assertIn(marker, stderr.getvalue())
                self.assertNotIn(secret, stderr.getvalue())
                self.assertNotIn("sensitive-os-error", stderr.getvalue())
                self.assertNotIn("https://", stderr.getvalue())
                self.assertEqual(transport.call_count, 1)

    def test_main_stale_deployment_still_fails_without_echoing_annotation(self):
        """Protects: stale-deploy rejection after a successful authenticated read.
        Detects: echoed annotations or a health query after mismatch.
        Needs: one synthetic stale deployment and request count control.
        Breadcrumb: verify_deploy.py newest_deployment_names_commit and main.
        """
        secret = "sensitive-annotation"
        reply = io.BytesIO(json.dumps(payload(deployment("new", secret))).encode())
        stderr = io.StringIO()
        with (
            patch.dict(os.environ, {"CLOUDFLARE_API_TOKEN": "fake-token"}),
            patch.object(vd.urllib.request, "urlopen", return_value=reply) as transport,
            patch("sys.stderr", stderr),
        ):
            self.assertEqual(vd.main(["--commit", SHA, "--account-id", "acct"]), 1)
        self.assertEqual(transport.call_count, 1)
        self.assertIn("phase=deployments-api", stderr.getvalue())
        self.assertNotIn(secret, stderr.getvalue())

    def test_main_success_requires_api_revision_and_worker_health(self):
        """Protects: both existing acceptance conditions after diagnostic changes.
        Detects: liveness alone as revision proof or missing health validation.
        Needs: two synthetic transport replies and a failing health positive control.
        Breadcrumb: verify_deploy.py main deployment and health acceptance.
        """
        for healthy in (True, False):
            with self.subTest(healthy=healthy):
                replies = [
                    io.BytesIO(
                        json.dumps(payload(deployment("new", "Source" + SHA))).encode()
                    ),
                    io.BytesIO(
                        json.dumps(
                            {"ok": healthy, "runtime": "cloudflare-workers"}
                        ).encode()
                    ),
                ]
                with (
                    patch.dict(os.environ, {"CLOUDFLARE_API_TOKEN": "fake-token"}),
                    patch.object(
                        vd.urllib.request, "urlopen", side_effect=replies
                    ) as transport,
                    patch("sys.stderr", io.StringIO()),
                    patch("sys.stdout", io.StringIO()),
                ):
                    self.assertEqual(
                        vd.main(["--commit", SHA, "--account-id", "acct"]),
                        0 if healthy else 1,
                    )
                self.assertEqual(transport.call_count, 2)

    def test_newest_deployment_naming_the_commit_passes(self):
        old = deployment("2026-10-15T00:00:00Z", "Source" + "b" * 40)
        new = deployment("2026-10-19T00:00:00Z", "Source" + SHA)
        self.assertEqual(
            vd.newest_deployment_names_commit(payload(old, new), SHA),
            "v-2026-10-19T00:00:00Z",
        )

    def test_stale_newest_deployment_fails_even_if_an_older_one_matches(self):
        older_match = deployment("2026-10-15T00:00:00Z", "Source" + SHA)
        newer_other = deployment("2026-10-19T00:00:00Z", "Source" + "b" * 40)
        with self.assertRaises(vd.DeployMismatch):
            vd.newest_deployment_names_commit(payload(older_match, newer_other), SHA)

    def test_split_rollout_fails(self):
        split = deployment(
            "2026-10-19T00:00:00Z",
            "Source" + SHA,
            [
                {"version_id": "x", "percentage": 50},
                {"version_id": "y", "percentage": 50},
            ],
        )
        with self.assertRaises(vd.DeployMismatch):
            vd.newest_deployment_names_commit(payload(split), SHA)

    def test_no_deployments_fails(self):
        with self.assertRaises(vd.DeployMismatch):
            vd.newest_deployment_names_commit(payload(), SHA)

    def test_missing_token_is_a_usage_error(self):
        env = {k: v for k, v in os.environ.items() if k != "CLOUDFLARE_API_TOKEN"}
        with patch.dict(os.environ, env, clear=True):
            self.assertEqual(vd.main(["--commit", SHA, "--account-id", "acct"]), 2)

    def test_workflow_isolates_runner_and_npm_bootstrap_step(self):
        """Protects: isolated deploy runners and a preceding npm bootstrap step.
        Detects: persistent runners, combined setup/install, or missing readback tests.
        Needs: checked-in workflow text only; no runner, network or credentials.
        Breadcrumb: .github/workflows/deploy-agentbus-cloud.yml deploy steps.
        """
        workflow = (
            REPO_ROOT / ".github/workflows/deploy-agentbus-cloud.yml"
        ).read_text(encoding="utf-8")
        active = "\n".join(
            line for line in workflow.splitlines() if not line.lstrip().startswith("#")
        )
        self.assertIn("runs-on: ubuntu-latest", active)
        self.assertNotIn("vigil1", active)
        self.assertNotIn("self-hosted", active)
        steps = re.split(r"(?m)^      - ", active)[1:]
        setup_idx = next(
            (i for i, step in enumerate(steps) if "setup-npm.sh" in step), None
        )
        install_idx = next(
            (i for i, step in enumerate(steps) if "npm ci" in step), None
        )
        self.assertIsNotNone(setup_idx, "setup-npm.sh step must be present")
        self.assertIsNotNone(install_idx, "npm ci step must be present")
        assert setup_idx is not None
        assert install_idx is not None
        self.assertLess(
            setup_idx,
            install_idx,
            "setup-npm.sh must run in a preceding step before npm ci",
        )
        self.assertNotIn("npm ci", steps[setup_idx])
        self.assertIn(
            'python3 -B -m unittest discover -s tools -p "test_verify_deploy.py"',
            steps[install_idx],
        )


if __name__ == "__main__":
    unittest.main()
