#!/usr/bin/env python3
"""Forward cloud-token operations to the canonical Rust CLI.

No token generation, file writes, recovery gate or HTTP policy is duplicated
here. Use host-supported tooling to unlock Bitwarden before recovery actions.
"""

from __future__ import annotations

import argparse
import subprocess
from pathlib import Path


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "action",
        choices=[
            "init-manifest",
            "mint",
            "rotate",
            "activate",
            "write-client",
            "bw-upsert",
            "wrangler-put",
            "revoke",
            "wrangler-hint",
            "status",
            "smoke",
        ],
    )
    parser.add_argument("--manifest")
    parser.add_argument("--repo-root", default=str(Path(__file__).resolve().parents[1]))
    parser.add_argument("--agent-bus", default="agent-bus")
    parser.add_argument("--dry-run", action="store_true")
    args = parser.parse_args()
    command = [
        args.agent_bus,
        "cloud-tokens",
        args.action,
        "--repo-root",
        args.repo_root,
    ]
    if args.manifest:
        command.extend(["--manifest", args.manifest])
    if args.dry_run:
        command.append("--dry-run")
    try:
        return subprocess.run(command, check=False).returncode
    except OSError:
        parser.exit(2, "agent-bus executable could not be started\n")


if __name__ == "__main__":
    raise SystemExit(main())
