"""Check source pins, selected installed tools and official stable-release drift.

This command reads state only. Release drift never upgrades a shared runner,
rewrites a lockfile, restarts a daemon or silently selects a floating compiler.
"""

import argparse
import json
from pathlib import Path
import re
import subprocess
import urllib.request

ROOT = Path(__file__).resolve().parents[2]
POLICY = Path(__file__).with_name("toolchain-policy.json")
RELEASES = {
    "rust": "https://api.github.com/repos/rust-lang/rust/releases/latest",
    "sccache": "https://api.github.com/repos/mozilla/sccache/releases/latest",
    "powershell": "https://api.github.com/repos/PowerShell/PowerShell/releases/latest",
    "actions_runner": "https://api.github.com/repos/actions/runner/releases/latest",
    "docker": "https://api.github.com/repos/moby/moby/releases/latest",
    "lefthook": "https://api.github.com/repos/evilmartians/lefthook/releases/latest",
    "node": "https://nodejs.org/dist/index.json",
    "npm": "https://registry.npmjs.org/npm/latest",
    "buildx": "https://api.github.com/repos/docker/buildx/releases/latest",
    "buildkit": "https://api.github.com/repos/moby/buildkit/releases/latest",
    "rustup": "https://static.rust-lang.org/rustup/release-stable.toml",
}


def verify_sources(root, policy):
    """Reject inconsistent source pins and inadmissible runner routing."""
    issues = []

    def require(path, pattern, expected):
        source = (root / path).read_text(encoding="utf-8-sig")
        actual = re.findall(pattern, source, re.M)
        if not actual or any(value != expected for value in actual):
            issues.append(f"{path}: expected {expected}, observed {actual}")

    require("rust-toolchain.toml", r'^channel\s*=\s*"([^"]+)"', policy["rust"])
    require("docker/Dockerfile.linux", r"^FROM rust:([^-]+)-bookworm", policy["rust"])
    require(
        "scripts/ci/setup-rust.sh",
        r'^expected_sccache_version="([^"]+)"',
        policy["sccache"],
    )
    require(".github/workflows/ci.yml", r'^\s*node-version: "([^"]+)"', policy["node"])
    require(
        ".github/workflows/ci.yml",
        r'^\s*LEFTHOOK_VERSION: "([^"]+)"',
        policy["lefthook"],
    )
    require(
        ".github/workflows/ci.yml",
        r"^\s*LEFTHOOK_SHA256: ([a-f0-9]+)",
        policy["lefthook_linux_x64_sha256"],
    )
    windows = json.loads((root / "scripts/ci/windows-cache-policy.json").read_text())
    if windows["version"] != policy["sccache"]:
        issues.append("Windows cache version differs from toolchain policy")
    source = (root / ".github/workflows/ci.yml").read_text()
    release = (root / ".github/workflows/release.yml").read_text()
    if 'runner: \'["self-hosted","Linux","X64","vigil1"]\'' not in release:
        issues.append("Linux x64 release build is not routed to vigil1")
    # Every ordinary x64 Linux job is explicitly offloaded. Docker and Windows
    # cross jobs retain their existing capability labels and acceptance gates.
    if "runs-on: [self-hosted, Linux, X64]" in source:
        issues.append("generic Linux job can still contend with ASUS cross builds")
    for line in source.splitlines():
        if (
            "runs-on:" in line
            and "vigil1" in line
            and any(
                capability in line
                for capability in ("docker", "windows-cross", "Windows")
            )
        ):
            issues.append("vigil1 cannot receive unqualified Docker or Windows jobs")
    return issues


def release_version(tool, body):
    """Parse only the known official release schema; unknown data fails."""
    if tool == "rustup":
        match = re.search(r"(?m)^version\s*=\s*'([0-9]+\.[0-9]+\.[0-9]+)'$", body)
        if match is None:
            raise ValueError("official rustup release metadata has no stable version")
        return match[1]
    data = json.loads(body)
    if tool == "npm":
        version = data["version"]
    elif tool == "node":
        version = data[0]["version"]
    else:
        if data.get("draft") or data.get("prerelease"):
            raise ValueError("official latest endpoint returned an unstable release")
        version = data["tag_name"]
    version = version.removeprefix("docker-").removeprefix("v")
    if not re.fullmatch(r"[0-9]+\.[0-9]+\.[0-9]+", version):
        raise ValueError("unrecognized official stable release version")
    return version


def check_latest(policy, fetch):
    """Return all drift/access errors; never report a failed lookup as current."""
    issues = []
    for tool, endpoint in RELEASES.items():
        try:
            current = release_version(tool, fetch(endpoint))
            if current != policy[tool]:
                issues.append(
                    f"{tool}: pinned {policy[tool]}, official latest {current}"
                )
        except (OSError, ValueError, KeyError, IndexError, TypeError) as error:
            issues.append(
                f"{tool}: latest-release evidence unavailable ({type(error).__name__})"
            )
    return issues


def fetch_release(endpoint):
    request = urllib.request.Request(
        endpoint, headers={"User-Agent": "agent-hub-toolchain-policy"}
    )
    with urllib.request.urlopen(request, timeout=10) as response:
        body = response.read(2 * 1024 * 1024 + 1)
        if len(body) > 2 * 1024 * 1024:
            raise ValueError(
                "official release metadata exceeds the bounded response size"
            )
        return body.decode("utf-8")


def check_installed(policy, tools):
    commands = {
        "rust": (["rustc", "--version"], r"^rustc ([0-9.]+) "),
        "sccache": (["sccache", "--version"], r"^sccache ([0-9.]+)"),
        "node": (["node", "--version"], r"^v([0-9.]+)"),
        "npm": (["npm", "--version"], r"^([0-9.]+)"),
        "powershell": (
            [
                "pwsh",
                "-NoLogo",
                "-NoProfile",
                "-Command",
                "$PSVersionTable.PSVersion.ToString()",
            ],
            r"^([0-9.]+)",
        ),
        "rustup": (["rustup", "--version"], r"^rustup ([0-9.]+)"),
        "docker": (["docker", "--version"], r"^Docker version ([0-9.]+),"),
        "lefthook": (["lefthook", "version"], r"^([0-9.]+)"),
    }
    issues = []
    for tool in tools:
        try:
            command, pattern = commands[tool]
            output = subprocess.check_output(command, text=True, timeout=15)
            match = re.search(pattern, output.strip())
            if match is None or match[1] != policy[tool]:
                issues.append(
                    f"{tool}: installed version differs from pinned {policy[tool]}"
                )
        except (OSError, KeyError, subprocess.SubprocessError):
            issues.append(f"{tool}: installed version could not be established")
    return issues


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--verify-sources", action="store_true")
    parser.add_argument("--latest", action="store_true")
    parser.add_argument(
        "--installed",
        nargs="+",
        choices=(
            "rust",
            "rustup",
            "sccache",
            "node",
            "npm",
            "powershell",
            "docker",
            "lefthook",
        ),
    )
    args = parser.parse_args()
    if not (args.verify_sources or args.latest or args.installed):
        parser.error("select --verify-sources, --latest or --installed")
    policy = json.loads(POLICY.read_text())
    issues = []
    if args.verify_sources:
        issues.extend(verify_sources(ROOT, policy))
    if args.latest:
        issues.extend(check_latest(policy, fetch_release))
    if args.installed:
        issues.extend(check_installed(policy, args.installed))
    print(json.dumps({"ok": not issues, "issues": issues, "policy": policy}, indent=2))
    return int(bool(issues))


if __name__ == "__main__":
    raise SystemExit(main())
