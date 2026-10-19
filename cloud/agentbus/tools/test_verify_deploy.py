"""Unit tests for verify_deploy.py.

Protects: the post-deploy readback fails when the newest deployment is not the
  merged commit.
Detects:  a readback that passes on a stale deployment (the mismatch tests are
  the positive control).
Needs:    stdlib and pytest, no network.
  Run: uv run --no-project --with pytest pytest test_verify_deploy.py
Breadcrumb: .github/workflows/deploy-agentbus-cloud.yml
"""

from __future__ import annotations

import pytest

import verify_deploy as vd

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


def test_newest_deployment_naming_the_commit_passes():
    old = deployment("2026-10-15T00:00:00Z", "Source" + "b" * 40)
    new = deployment("2026-10-19T00:00:00Z", "Source" + SHA)
    assert (
        vd.newest_deployment_names_commit(payload(old, new), SHA)
        == "v-2026-10-19T00:00:00Z"
    )


def test_stale_newest_deployment_fails_even_if_an_older_one_matches():
    older_match = deployment("2026-10-15T00:00:00Z", "Source" + SHA)
    newer_other = deployment("2026-10-19T00:00:00Z", "Source" + "b" * 40)
    with pytest.raises(vd.DeployMismatch):
        vd.newest_deployment_names_commit(payload(older_match, newer_other), SHA)


def test_split_rollout_fails():
    split = deployment(
        "2026-10-19T00:00:00Z",
        "Source" + SHA,
        [{"version_id": "x", "percentage": 50}, {"version_id": "y", "percentage": 50}],
    )
    with pytest.raises(vd.DeployMismatch):
        vd.newest_deployment_names_commit(payload(split), SHA)


def test_no_deployments_fails():
    with pytest.raises(vd.DeployMismatch):
        vd.newest_deployment_names_commit(payload(), SHA)


def test_missing_token_is_a_usage_error(monkeypatch):
    monkeypatch.delenv("CLOUDFLARE_API_TOKEN", raising=False)
    assert vd.main(["--commit", SHA, "--account-id", "acct"]) == 2
