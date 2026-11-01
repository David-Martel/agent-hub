"""Detect stale pins, source/image drift and unavailable latest-release evidence."""

import copy
from datetime import datetime, timezone
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

    # Protects: CI uses the exact Node version declared by the source policy.
    # Detects: A floating Node pin or a stale mutation that changes no fixture bytes.
    # Needs: Temporary tracked-source fixtures; no network, services or hardware.
    # Breadcrumb: scripts/ci/toolchain_policy.py verify_sources Node workflow check.
    def test_floating_node_pin_fails(self):
        path = self.root / ".github/workflows/ci.yml"
        needle = f'node-version: "{self.policy["node"]}"'
        self.assertIn(needle, path.read_text(), "mutation must change the fixture")
        path.write_text(path.read_text().replace(needle, 'node-version: "26"'))
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

    def held_policy(self, **hold):
        policy = copy.deepcopy(self.policy)
        record = {
            "version": "1.99.1",
            "reason": "rebuild and qualify immutable cross image (fixture)",
            "reference": "PR #184",
            "reviewer": "fixture reviewer",
            "recorded_utc": "2026-11-01T00:00:00Z",
            "expires_utc": "2026-11-15T00:00:00Z",
        }
        record.update(hold)
        policy["rust"] = "1.99.0"
        policy["holds"] = {"rust": record}
        return policy

    def rust_release(self, version):
        def fetch(endpoint):
            if endpoint == POLICY_MODULE.RELEASES["rust"]:
                return json.dumps(
                    {"tag_name": version, "draft": False, "prerelease": False}
                )
            return self.release_fixture(endpoint)

        return fetch

    def rust_issues(self, policy, version, now):
        issues = POLICY_MODULE.check_latest(policy, self.rust_release(version), now=now)
        return [issue for issue in issues if issue.startswith("rust:")]

    # Protects: a deliberate, reviewed deferral keeps the scheduled drift job useful.
    # Detects: a recorded hold that fails to excuse its exact version before expiry,
    #   or that keeps excusing it at or after expiry.
    # Needs: in-memory release fixtures; no network, services or hardware.
    # Breadcrumb: scripts/ci/toolchain_policy.py hold_problem and check_latest.
    def test_held_drift_passes_before_expiry_and_fails_after(self):
        policy = self.held_policy()
        held = []
        before = datetime(2026, 11, 14, 23, 59, tzinfo=timezone.utc)
        self.assertEqual(
            POLICY_MODULE.check_latest(
                policy, self.rust_release("1.99.1"), now=before, held=held
            ),
            [],
        )
        self.assertEqual(len(held), 1)
        self.assertIn("held until 2026-11-15T00:00:00Z", held[0])
        at_expiry = datetime(2026, 11, 15, tzinfo=timezone.utc)
        issues = self.rust_issues(policy, "1.99.1", at_expiry)
        self.assertEqual(len(issues), 1)
        self.assertIn("expired", issues[0])

    # Protects: drift without a recorded hold stays a failure.
    # Detects: a hold on one tool excusing drift in another.
    # Needs: in-memory release fixtures; no network, services or hardware.
    # Breadcrumb: scripts/ci/toolchain_policy.py check_latest.
    def test_unheld_drift_still_fails_beside_a_hold(self):
        policy = self.held_policy()

        def fetch(endpoint):
            if endpoint == POLICY_MODULE.RELEASES["sccache"]:
                return json.dumps({"tag_name": "v9.9.9"})
            return self.rust_release("1.99.1")(endpoint)

        now = datetime(2026, 11, 2, tzinfo=timezone.utc)
        issues = POLICY_MODULE.check_latest(policy, fetch, now=now)
        self.assertEqual(
            issues, [f"sccache: pinned {policy['sccache']}, official latest 9.9.9"]
        )

    # Protects: a hold excuses only the exact upstream version that was reviewed.
    # Detects: a newer upstream release hiding behind an older hold.
    # Needs: in-memory release fixtures; no network, services or hardware.
    # Breadcrumb: scripts/ci/toolchain_policy.py hold_problem version check.
    def test_hold_does_not_cover_a_newer_release(self):
        now = datetime(2026, 11, 2, tzinfo=timezone.utc)
        issues = self.rust_issues(self.held_policy(), "1.100.0", now)
        self.assertEqual(len(issues), 1)
        self.assertIn("hold covers 1.99.1, official latest is 1.100.0", issues[0])

    # Protects: every hold carries its reason, decision, reviewer and expiry, and
    #   cannot outlive HOLD_MAX_DAYS.
    # Detects: an unreviewed, unexplained or effectively permanent hold.
    # Needs: in-memory release fixtures; no network, services or hardware.
    # Breadcrumb: scripts/ci/toolchain_policy.py hold_record_problem field, span checks.
    def test_malformed_or_long_holds_fail(self):
        now = datetime(2026, 11, 2, tzinfo=timezone.utc)
        for field, value, expected in (
            ("reviewer", "", "malformed"),
            ("reason", "  ", "malformed"),
            ("expires_utc", "2026-12-15T00:00:00Z", "within 31 days"),
            ("expires_utc", "2026-10-31T00:00:00Z", "within 31 days"),
            ("recorded_utc", "not-a-date", "ISO-8601"),
        ):
            with self.subTest(field=field, value=value):
                issues = self.rust_issues(
                    self.held_policy(**{field: value}), "1.99.1", now
                )
                self.assertEqual(len(issues), 1)
                self.assertIn(expected, issues[0])

    # Protects: a hold's 31-day window starts no later than the check that reads it.
    # Detects: a recorded_utc set in the future to stretch a hold past HOLD_MAX_DAYS.
    # Needs: in-memory release fixtures; no network, services or hardware.
    # Breadcrumb: scripts/ci/toolchain_policy.py hold_record_problem recorded_utc check.
    def test_hold_recorded_in_the_future_fails(self):
        now = datetime(2026, 11, 2, tzinfo=timezone.utc)
        policy = self.held_policy(
            recorded_utc="2026-11-20T00:00:00Z", expires_utc="2026-12-20T00:00:00Z"
        )
        issues = self.rust_issues(policy, "1.99.1", now)
        self.assertEqual(len(issues), 1)
        self.assertIn("recorded in the future", issues[0])

    # Protects: the fail-closed hold contract holds once a pin has caught up.
    # Detects: a malformed, expired, future-recorded or over-long hold that passes
    #   silently because the tool currently shows no drift.
    # Needs: in-memory release fixtures; no network, services or hardware.
    # Breadcrumb: scripts/ci/toolchain_policy.py check_latest hold validation.
    def test_invalid_hold_fails_without_drift(self):
        now = datetime(2026, 11, 2, tzinfo=timezone.utc)
        for overrides, expected in (
            ({"reviewer": ""}, "malformed"),
            ({"expires_utc": "2026-11-01T12:00:00Z"}, "expired"),
            (
                {
                    "recorded_utc": "2026-11-20T00:00:00Z",
                    "expires_utc": "2026-12-01T00:00:00Z",
                },
                "recorded in the future",
            ),
            ({"expires_utc": "2026-12-15T00:00:00Z"}, "within 31 days"),
        ):
            with self.subTest(expected=expected):
                policy = self.held_policy(**overrides)
                policy["rust"] = "1.99.1"
                issues = self.rust_issues(policy, "1.99.1", now)
                self.assertEqual(len(issues), 1)
                self.assertIn(expected, issues[0])

    # Protects: a valid hold whose pin has caught up is not itself a failure.
    # Detects: the no-drift validation rejecting a well-formed, unexpired hold.
    # Needs: in-memory release fixtures; no network, services or hardware.
    # Breadcrumb: scripts/ci/toolchain_policy.py check_latest hold validation.
    def test_valid_hold_without_drift_passes(self):
        now = datetime(2026, 11, 2, tzinfo=timezone.utc)
        policy = self.held_policy()
        policy["rust"] = "1.99.1"
        self.assertEqual(self.rust_issues(policy, "1.99.1", now), [])

    # Protects: holds name only tools the drift check knows.
    # Detects: a misspelled hold that silently excuses nothing and rots.
    # Needs: in-memory release fixtures; no network, services or hardware.
    # Breadcrumb: scripts/ci/toolchain_policy.py check_latest hold-key check.
    def test_hold_for_unknown_tool_fails(self):
        policy = self.held_policy()
        policy["holds"]["rustc"] = policy["holds"].pop("rust")
        now = datetime(2026, 11, 2, tzinfo=timezone.utc)
        issues = POLICY_MODULE.check_latest(
            policy, self.rust_release("1.99.0"), now=now
        )
        self.assertEqual(issues, ["holds: unknown tool rustc"])

    # Protects: the seeded hold is the one PR #184 recorded, and stays short.
    # Detects: a hold for a release no reviewer decided (actions runner 2.338.2 was
    #   published 2026-10-26, after #184 merged), or one without its citation.
    # Needs: the tracked toolchain-policy.json; no network, services or hardware.
    # Breadcrumb: scripts/ci/toolchain-policy.json holds; PR #184 image admission.
    def test_seeded_holds_are_the_reviewed_image_admission_deferrals(self):
        holds = self.policy.get("holds", {})
        self.assertEqual(set(holds), {"rust"})
        for tool, hold in holds.items():
            with self.subTest(tool=tool):
                self.assertIn("immutable cross image", hold["reason"])
                self.assertIn("#184", hold["reference"])
                self.assertIn("review of PR #184", hold["reviewer"])
                self.assertNotEqual(hold["version"], self.policy[tool])
                self.assertIsNone(
                    POLICY_MODULE.hold_problem(
                        tool,
                        hold,
                        hold["version"],
                        POLICY_MODULE.parse_utc(hold["recorded_utc"]),
                    )
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
