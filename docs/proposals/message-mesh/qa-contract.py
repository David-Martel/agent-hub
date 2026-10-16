"""Service-free mesh QA evidence controller; cannot launch tools or backends.

Root owns container creation, maintained harness execution, process settlement,
and cleanup. This helper binds a frozen assembled source and verifies exact
compiled selections and uncompressed libtest output. Python 3.11+ only.
"""

from __future__ import annotations

import argparse
from collections import Counter
import hashlib
import json
from pathlib import Path, PurePosixPath
import re


class ContractError(ValueError):
    """An evidence gate failed; the supplied evidence must be preserved."""


def require(condition, message):
    if not condition:
        raise ContractError(message)


def sha(path):
    with Path(path).open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def read_json(path):
    return json.loads(Path(path).read_text(encoding="utf-8-sig"))


def source_guard(source, manifest_path, expected_sha):
    """Accept only a complete plain relative-path to SHA256 manifest."""
    source = Path(source).resolve(strict=True)
    require(re.fullmatch(r"[a-fA-F0-9]{64}", expected_sha), "Invalid manifest SHA256")
    require(sha(manifest_path) == expected_sha.lower(), "Source manifest changed")
    expected = read_json(manifest_path)
    require(isinstance(expected, dict) and bool(expected), "Empty source manifest")
    for relative, digest in expected.items():
        path = PurePosixPath(relative)
        require(
            isinstance(relative, str)
            and not path.is_absolute()
            and relative == path.as_posix()
            and ".." not in path.parts
            and "\\" not in relative
            and ":" not in relative
            and re.fullmatch(r"[a-fA-F0-9]{64}", digest),
            "Source manifest has unsafe path or digest",
        )
    actual = {}
    for path in source.rglob("*"):
        require(not path.is_symlink(), "Source symlinks are refused")
        if path.is_file():
            actual[path.relative_to(source).as_posix()] = sha(path)
    require(
        actual == {key: value.lower() for key, value in expected.items()},
        "Frozen source inventory changed",
    )
    require("Cargo.lock" in actual, "Source manifest does not bind Cargo.lock")
    return {
        "source_manifest_sha256": expected_sha.lower(),
        "lock_sha256": actual["Cargo.lock"],
        "source_files": len(actual),
    }


def expectations(contract):
    records = contract.get("tests", [])
    require(
        records and all(isinstance(item, dict) for item in records),
        "Missing required mesh test inventory",
    )
    names = [item["name"] for item in records]
    require(len(names) == len(set(names)), "Duplicate required mesh test name")
    for item in records:
        require(
            re.fullmatch(r"[A-Za-z0-9_]+(?:::[A-Za-z0-9_]+)+", item["name"])
            and isinstance(item["ignored"], bool),
            "Invalid mesh test identity",
        )
    return records


def compiled_tests(text):
    """Parse stable libtest --list lines; never infer tests from source counts."""
    return Counter(
        re.findall(r"(?m)^([A-Za-z0-9_]+(?:::[A-Za-z0-9_]+)*): test\s*$", text)
    )


def check_listing(contract, kind, text):
    listed = compiled_tests(text)
    require(bool(listed), "Compiled test listing is empty")
    required = [
        item["name"]
        for item in expectations(contract)
        if item["ignored"] == (kind == "backend")
    ]
    require(bool(required), "Required phase selection is empty")
    missing = [name for name in required if listed[name] != 1]
    require(
        not missing,
        "Required test missing or repeated in compiled listing: " + ", ".join(missing),
    )
    return {
        "state": "COMPILED_LIST_ONLY_TEST_EXECUTION_NOT_PROVEN",
        "kind": kind,
        "compiled_selected_tests": sum(listed.values()),
        "required_mesh_tests": len(required),
        "tests": dict(sorted(listed.items())),
    }


def check_result(contract, kind, listing, output, exit_code):
    selection = check_listing(contract, kind, listing)
    require(exit_code == 0, "Test process failed; preserve first failure")
    summaries = re.findall(
        r"(?m)^test result: ok\. (\d+) passed; (\d+) failed; (\d+) ignored;", output
    )
    require(bool(summaries), "Test result summaries are missing")
    require(
        all(int(failed) == 0 for _, failed, _ in summaries),
        "A summary reports failures",
    )
    outcomes = re.findall(
        r"(?m)^test ([A-Za-z0-9_]+(?:::[A-Za-z0-9_]+)*) \.\.\. (ok|FAILED|ignored)\b",
        output,
    )
    passed = Counter(name for name, result in outcomes if result == "ok")
    failed = [name for name, result in outcomes if result == "FAILED"]
    require(not failed, "Libtest output reports a failed test")
    required = [
        item["name"]
        for item in expectations(contract)
        if item["ignored"] == (kind == "backend")
    ]
    require(
        all(passed[name] == 1 for name in required),
        "A required mesh test did not pass exactly once",
    )
    total_passed = sum(int(count) for count, _, _ in summaries)
    if kind == "backend":
        # Every ignored test in the complete workspace selection must run.
        # Aggregate success alone cannot mask an omitted/renamed mesh test.
        require(
            all(int(ignored) == 0 for _, _, ignored in summaries),
            "Backend selection contains skipped tests",
        )
        require(
            passed == compiled_tests(listing),
            "Backend pass set differs from compiled selection",
        )
        require(
            total_passed == selection["compiled_selected_tests"],
            "Backend summary count differs from compiled selection",
        )
    return {
        **selection,
        "state": "TEST_OUTPUT_PASS_CLEANUP_STILL_REQUIRED",
        "passed": total_passed,
        "ignored": sum(int(count) for _, _, count in summaries),
        "exit_code": exit_code,
    }


def write_new(path, payload):
    with Path(path).open("x", encoding="utf-8") as stream:
        json.dump(payload, stream, indent=2)
        stream.write("\n")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "action", choices=("source-check", "list-check", "result-check")
    )
    parser.add_argument("--source", type=Path, required=True)
    parser.add_argument("--source-manifest", type=Path, required=True)
    parser.add_argument("--manifest-sha256", required=True)
    parser.add_argument("--contract", type=Path, required=True)
    parser.add_argument("--contract-sha256", required=True)
    parser.add_argument("--kind", choices=("unit", "backend"))
    parser.add_argument("--listing", type=Path)
    parser.add_argument("--output-log", type=Path)
    parser.add_argument("--exit-code", type=int)
    parser.add_argument("--receipt", type=Path, required=True)
    args = parser.parse_args()
    require(
        sha(args.contract) == args.contract_sha256.lower(),
        "Required-test contract changed",
    )
    contract = read_json(args.contract)
    records = expectations(contract)
    source = source_guard(args.source, args.source_manifest, args.manifest_sha256)
    result = {
        "state": "SOURCE_BINDING_ONLY_NOT_COMPILED",
        "required_tests": len(records),
    }
    if args.action != "source-check":
        require(
            args.kind is not None and args.listing is not None,
            "Selection arguments missing",
        )
        listing = args.listing.read_text(encoding="utf-8")
        result = check_listing(contract, args.kind, listing)
        result["listing_sha256"] = sha(args.listing)
        if args.action == "result-check":
            require(
                args.output_log is not None and args.exit_code is not None,
                "Actual output and exit code are required",
            )
            result = {
                **result,
                **check_result(
                    contract,
                    args.kind,
                    listing,
                    args.output_log.read_text(encoding="utf-8"),
                    args.exit_code,
                ),
                "output_sha256": sha(args.output_log),
            }
    write_new(
        args.receipt,
        {
            **source,
            **result,
            "contract_sha256": sha(args.contract),
            "limits": (
                "Source/list/output evidence only; "
                "Root must prove owned cleanup separately"
            ),
        },
    )


if __name__ == "__main__":
    try:
        main()
    except (ContractError, OSError, ValueError, KeyError, TypeError) as error:
        # Do not echo arbitrary file contents, command arguments or backend URLs.
        # ContractError text is authored here and includes only validated test names.
        message = (
            str(error) if isinstance(error, ContractError) else type(error).__name__
        )
        raise SystemExit("Mesh QA evidence rejected: " + message) from None
