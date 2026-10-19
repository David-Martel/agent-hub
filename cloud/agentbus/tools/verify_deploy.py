#!/usr/bin/env python3
"""Read back an agentbus-cloud deploy: the newest deployment names the commit.

Used by .github/workflows/deploy-agentbus-cloud.yml after `wrangler deploy`. Two
checks, both required:

1. The Cloudflare deployments API lists the newest deployment at 100 percent of
   one version whose `workers/message` annotation names the expected commit
   (`Source<sha>`).
2. The public `/health` answers `ok: true` from the Workers runtime. The static
   build label in `/health` is not a revision (see README), so it is checked for
   liveness only.

The token is read from CLOUDFLARE_API_TOKEN and never printed. Stdlib only.
"""

from __future__ import annotations

import argparse
import json
import os
import sys
import urllib.error
import urllib.request

API = "https://api.cloudflare.com/client/v4"


class DeployMismatch(Exception):
    """The newest deployment does not name the expected commit."""


def newest_deployment_names_commit(payload: dict, commit: str) -> str:
    """Return the version id of the newest deployment, or raise DeployMismatch."""
    deployments = (payload.get("result") or {}).get("deployments") or []
    if not deployments:
        raise DeployMismatch("no deployments listed")
    newest = max(deployments, key=lambda d: d.get("created_on", ""))
    message = (newest.get("annotations") or {}).get("workers/message", "")
    if f"Source{commit}" not in message:
        raise DeployMismatch(
            f"newest deployment {newest.get('id')} message {message!r} "
            f"lacks Source{commit}"
        )
    versions = newest.get("versions") or []
    if len(versions) != 1 or versions[0].get("percentage") != 100:
        raise DeployMismatch(
            f"newest deployment {newest.get('id')} is not 100 percent on one version"
        )
    return versions[0]["version_id"]


def get_json(url: str, token: str | None = None) -> dict:
    request = urllib.request.Request(url, headers={"Accept": "application/json"})
    if token:
        request.add_header("Authorization", "Bearer " + token)
    with urllib.request.urlopen(request, timeout=30) as response:
        return json.load(response)


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument(
        "--commit", required=True, help="full 40-hex commit the deploy was made from"
    )
    parser.add_argument("--account-id", default=os.environ.get("CLOUDFLARE_ACCOUNT_ID"))
    parser.add_argument("--script", default="agentbus-cloud")
    parser.add_argument(
        "--health-url", default="https://agentbus.dtmventures.com/health"
    )
    args = parser.parse_args(argv)
    token = os.environ.get("CLOUDFLARE_API_TOKEN")
    if not token or not args.account_id:
        print("CLOUDFLARE_API_TOKEN and an account id are required", file=sys.stderr)
        return 2
    try:
        deployments = get_json(
            f"{API}/accounts/{args.account_id}/workers/scripts/"
            f"{args.script}/deployments",
            token,
        )
        version = newest_deployment_names_commit(deployments, args.commit)
        health = get_json(args.health_url)
    except (DeployMismatch, urllib.error.URLError, ValueError) as error:
        print(f"deploy readback FAILED: {error}", file=sys.stderr)
        return 1
    if health.get("ok") is not True or health.get("runtime") != "cloudflare-workers":
        print(
            "deploy readback FAILED: /health is not ok from the Workers runtime",
            file=sys.stderr,
        )
        return 1
    print(
        f"deploy readback OK: version {version} names Source{args.commit}; /health ok"
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
