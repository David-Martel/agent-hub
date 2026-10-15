"""Detect stale pins, source/image drift and unavailable latest-release evidence."""

import copy
import importlib.util
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys
import tempfile
import unittest

SPEC = importlib.util.spec_from_file_location(
    "policy", Path(__file__).with_name("toolchain_policy.py")
)
POLICY_MODULE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(POLICY_MODULE)


class ToolchainPolicyTests(unittest.TestCase):
    def setUp(self):
        self.policy = json.loads(POLICY_MODULE.POLICY.read_text())
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        for name in (
            "rust-toolchain.toml",
            "docker/Dockerfile.linux",
            "scripts/ci/setup-rust.sh",
            ".github/workflows/ci.yml",
            ".github/workflows/release.yml",
            "scripts/ci/windows-cache-policy.json",
        ):
            dest = self.root / name
            dest.parent.mkdir(parents=True, exist_ok=True)
            dest.write_bytes((POLICY_MODULE.ROOT / name).read_bytes())

    def test_conserved_sources_pass(self):
        self.assertEqual(POLICY_MODULE.verify_sources(self.root, self.policy), [])

    def test_one_docker_stage_on_old_compiler_fails(self):
        path = self.root / "docker/Dockerfile.linux"
        path.write_text(path.read_text().replace("rust:1.99.0", "rust:1.98.1", 1))
        self.assertTrue(POLICY_MODULE.verify_sources(self.root, self.policy))

    def test_floating_node_pin_fails(self):
        path = self.root / ".github/workflows/ci.yml"
        path.write_text(
            path.read_text().replace('node-version: "26.12.0"', 'node-version: "26"')
        )
        self.assertTrue(POLICY_MODULE.verify_sources(self.root, self.policy))

    def test_unqualified_vigil1_docker_or_windows_routing_fails(self):
        path = self.root / ".github/workflows/ci.yml"
        source = path.read_text()
        for labels in (
            "Windows, X64, agent-hub-native-smoke]",
            "Linux, X64, docker]",
        ):
            with self.subTest(labels=labels):
                self.assertIn(
                    labels, source, "routing mutation must change the fixture"
                )
                path.write_text(source.replace(labels, labels[:-1] + ", vigil1]", 1))
                self.assertIn(
                    "vigil1 cannot receive unqualified Docker or Windows jobs",
                    POLICY_MODULE.verify_sources(self.root, self.policy),
                )

    def test_release_build_cannot_drift_back_to_generic_linux(self):
        path = self.root / ".github/workflows/release.yml"
        path.write_text(
            path.read_text().replace(
                '["self-hosted","Linux","X64","vigil1"]',
                '["self-hosted","Linux","X64"]',
            )
        )
        self.assertIn(
            "Linux x64 release build is not routed to vigil1",
            POLICY_MODULE.verify_sources(self.root, self.policy),
        )

    def release_fixture(self, endpoint):
        tool = next(
            key for key, value in POLICY_MODULE.RELEASES.items() if value == endpoint
        )
        version = self.policy[tool]
        if tool == "rustup":
            return f"version = '{version}'\n"
        if tool == "node":
            return json.dumps([{"version": "v" + version}])
        if tool == "npm":
            return json.dumps({"version": version})
        return json.dumps(
            {"tag_name": "v" + version, "draft": False, "prerelease": False}
        )

    def test_official_release_snapshot_passes(self):
        self.assertEqual(
            POLICY_MODULE.check_latest(self.policy, self.release_fixture), []
        )

    def test_new_release_fails_without_upgrading(self):
        original = copy.deepcopy(self.policy)

        def newer(endpoint):
            if endpoint == POLICY_MODULE.RELEASES["sccache"]:
                return json.dumps({"tag_name": "v0.19.0"})
            return self.release_fixture(endpoint)

        self.assertIn(
            "sccache: pinned 0.18.0, official latest 0.19.0",
            POLICY_MODULE.check_latest(self.policy, newer),
        )
        self.assertEqual(self.policy, original)

    def test_failed_lookups_never_report_current(self):
        def unavailable(endpoint):
            raise OSError("closed service")

        issues = POLICY_MODULE.check_latest(self.policy, unavailable)
        self.assertEqual(len(issues), len(POLICY_MODULE.RELEASES))
        self.assertTrue(all("unavailable" in issue for issue in issues))

    def test_unstable_release_is_refused(self):
        with self.assertRaises(ValueError):
            POLICY_MODULE.release_version(
                "sccache", '{"tag_name":"v0.19.0","prerelease":true}'
            )

    def test_native_setup_never_stops_or_replaces_shared_cache(self):
        source = (self.root / "scripts/ci/setup-rust.sh").read_text()
        self.assertNotIn("--stop-server", source)
        self.assertIn('"$sccache_tools/sccache"', source)
        self.assertIn("export CARGO_INCREMENTAL=0", source)

    @unittest.skipUnless(
        sys.platform == "linux" and shutil.which("bash"),
        "Linux Bash PATH publication control",
    )
    def test_next_step_path_selects_private_cache_over_inherited_clients(self):
        """
        Protects: The serving-cache-qualified executable survives step boundaries.
        Detects: GITHUB_PATH publication makes an inherited old client shadow it.
        Needs: Linux Bash and owned fixture executables; no Rust/cache daemon.
        Breadcrumb: Runner 2.338 AddPathFileCommand and AddPrependPathToEnvironment.
        """
        source = (POLICY_MODULE.ROOT / "scripts/ci/setup-rust.sh").read_text()
        block = re.search(
            r'if \[\[ -n "\$\{GITHUB_PATH:-\}" \]\]; then\n.*?\nfi', source, re.S
        ).group()
        home = self.root / "home"
        private = self.root / "qualified-tools"
        inherited = [home / ".cargo/bin", home / ".local/bin"]
        original = {}
        for directory, version in [
            *((path, "0.16.0") for path in inherited),
            (private, "0.18.0"),
        ]:
            directory.mkdir(parents=True)
            client = directory / "sccache"
            client.write_text(f"#!/bin/bash\necho 'sccache {version}'\n")
            client.chmod(0o755)
            original[client] = client.read_bytes()
        published = self.root / "github-path"
        env = {
            "PATH": "/usr/bin:/bin",
            "HOME": str(home),
            "GITHUB_PATH": str(published),
        }
        subprocess.run(
            [
                shutil.which("bash"),
                "-euc",
                'sccache_tools="$1"\n' + block,
                "fixture",
                str(private),
            ],
            env=env,
            check=True,
            timeout=5,
        )
        # Runner 2.338 collects each line, then iterates PrependPath in reverse.
        next_path = os.pathsep.join(
            [
                *reversed(published.read_text().splitlines()),
                *map(str, inherited),
                "/usr/bin",
                "/bin",
            ]
        )
        selected = shutil.which("sccache", path=next_path)
        self.assertEqual(Path(selected), private / "sccache")
        result = subprocess.run(
            [selected, "--version"],
            env={**env, "PATH": next_path},
            text=True,
            capture_output=True,
            check=True,
            timeout=5,
        )
        self.assertEqual(result.stdout.strip(), "sccache 0.18.0")
        self.assertEqual({client: client.read_bytes() for client in original}, original)


if __name__ == "__main__":
    unittest.main()
