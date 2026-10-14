"""Synthetic launcher for first-error controls; never launches Wine or a backend."""

import argparse
import hashlib
import json
from pathlib import Path
import sys

parser = argparse.ArgumentParser()
parser.add_argument("operation")
parser.add_argument("--root", required=True)
parser.add_argument("--command")
parser.add_argument("--arguments-json")
args = parser.parse_args()
if args.operation == "prepare":
    print('{"prepared": true}')
elif args.operation == "finalize":
    if Path(__file__).with_name("fail-finalize").exists():
        sys.exit(9)
    print('{"settled": true}')
else:
    path = Path(args.command)
    root = Path(args.root)
    command_hash = hashlib.sha256(path.read_bytes()).hexdigest()
    receipt = root / ("mcp-launch-" + "a" * 32 + ".json")
    proof = {
        "profile": "configured-mcp-wine-launch-ready-v1",
        "root": str(root),
        "command": str(path),
        "command_sha256": command_hash,
    }
    receipt.write_text(json.dumps(proof))
    proof.update(
        receipt=receipt.name,
        sha256=hashlib.sha256(receipt.read_bytes()).hexdigest(),
    )
    print(json.dumps({"jsonrpc": "2.0", "id": 0, "result": proof}), flush=True)
    json.loads(sys.stdin.readline())
    print(
        json.dumps(
            {
                "jsonrpc": "2.0",
                "id": 1,
                "error": {"code": -32000, "message": "synthetic protocol failure"},
            }
        ),
        flush=True,
    )
    sys.exit(7)
