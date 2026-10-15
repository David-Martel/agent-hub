"""Ensure an old/shared daemon and false cache hit cannot qualify native CI."""

import copy
import importlib.util
import json
from pathlib import Path
import unittest

SPEC = importlib.util.spec_from_file_location(
    "native_proof", Path(__file__).with_name("native_cache_preflight.py")
)
PROOF = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(PROOF)


class NativeCacheProofTests(unittest.TestCase):
    def setUp(self):
        self.directory = str(Path("private-native-cache").resolve())
        self.before = {
            "stats": {
                "cache_hits": {"counts": {"Rust": 2}},
                "cache_errors": {"counts": {}},
                "cache_timeouts": 0,
                "cache_read_errors": 0,
                "cache_write_errors": 0,
                "compile_fails": 0,
                "requests_unsupported_compiler": 0,
                "requests_not_cacheable": 0,
            }
        }
        self.after = copy.deepcopy(self.before)
        self.after.update(
            version="0.18.0", cache_location="Local disk: " + json.dumps(self.directory)
        )
        self.after["stats"]["cache_hits"]["counts"]["Rust"] += 1

    def verify(self):
        PROOF.validate_stats(self.before, self.after, "0.18.0", self.directory)

    def test_real_rust_hit_and_bound_daemon_pass(self):
        self.verify()

    def test_old_serving_daemon_is_refused(self):
        self.after["version"] = "0.16.0"
        with self.assertRaisesRegex(ValueError, "serving daemon version"):
            self.verify()

    def test_other_cache_is_refused(self):
        self.after["cache_location"] = 'Local disk: "/other-owner/cache"'
        with self.assertRaisesRegex(ValueError, "cache directory"):
            self.verify()

    def test_directory_prefix_cannot_imply_identity(self):
        self.after["cache_location"] = "Local disk: " + json.dumps(
            self.directory + "-other-owner"
        )
        with self.assertRaisesRegex(ValueError, "cache directory"):
            self.verify()

    def test_no_cache_hit_is_refused(self):
        self.after["stats"]["cache_hits"] = copy.deepcopy(
            self.before["stats"]["cache_hits"]
        )
        with self.assertRaisesRegex(ValueError, "real Rust cache hit"):
            self.verify()

    def multilevel(self):
        # Exact fields/location rendering measured from native sccache0.18.
        self.after["cache_location"] = "Multi-level (2 levels)"
        self.after["stats"]["multi_level"] = [
            {
                "name": "L0 (disk)",
                "location": "Local disk: " + json.dumps(self.directory),
                "write_failures": 0,
            },
            {
                "name": "L1 (redis)",
                "location": "redis, name: redis://cache.invalid:6381, "
                "prefix: /owned-prefix:/",
                "write_failures": 0,
            },
        ]
        self.before.update(version="0.18.0", cache_location="Multi-level (2 levels)")
        self.before["stats"]["multi_level"] = copy.deepcopy(
            self.after["stats"]["multi_level"]
        )
        return self.after["stats"]["multi_level"]

    def verify_multilevel(
        self, endpoint="redis://cache.invalid:6381", prefix="owned-prefix:"
    ):
        PROOF.validate_stats(
            self.before,
            self.after,
            "0.18.0",
            self.directory,
            "disk,redis",
            endpoint,
            prefix,
        )

    def test_actual_multilevel_shape_binds_l0_disk_and_l1_peer(self):
        """Protects ARM cache admission; detects top-level-summary path lookup."""
        self.multilevel()
        self.verify_multilevel()

    def test_multilevel_wrong_l0_path_and_prefix_cannot_qualify(self):
        for directory in (self.directory + "-foreign", str(Path("foreign").resolve())):
            levels = self.multilevel()
            levels[0]["location"] = "Local disk: " + json.dumps(directory)
            with self.assertRaisesRegex(ValueError, "cache directory"):
                self.verify_multilevel()

    def test_multilevel_missing_duplicate_reordered_or_wrong_type_refused(self):
        for mutate in (
            lambda levels: levels.pop(),
            lambda levels: levels.append(copy.deepcopy(levels[0])),
            lambda levels: levels.reverse(),
            lambda levels: levels[0].update(name="L0 (redis)"),
            lambda levels: levels[1].update(name="L0 (disk)"),
            lambda levels: levels.__setitem__(0, "not-an-object"),
        ):
            levels = self.multilevel()
            mutate(levels)
            with self.assertRaisesRegex(ValueError, "level inventory"):
                self.verify_multilevel()
        self.multilevel()
        self.after["stats"]["multi_level"] = {"L0": "not-an-array"}
        with self.assertRaisesRegex(ValueError, "level inventory"):
            self.verify_multilevel()

    def test_multilevel_wrong_peer_prefix_or_hidden_levels_refused(self):
        for location in (
            "redis, name: redis://foreign.invalid:6381, prefix: /owned-prefix:/",
            "redis, name: redis://cache.invalid:6381, prefix: /foreign-prefix:/",
        ):
            levels = self.multilevel()
            levels[1]["location"] = location
            with self.assertRaisesRegex(ValueError, "Redis peer or key prefix"):
                self.verify_multilevel()
        self.multilevel()
        self.after["cache_location"] = "Multi-level (3 levels)"
        with self.assertRaisesRegex(ValueError, "level inventory"):
            self.verify_multilevel()

    def test_multilevel_refusals_never_echo_endpoint_credentials(self):
        secret = "closed-fixture-private-token"
        levels = self.multilevel()
        levels[1]["location"] = (
            f"redis, name: redis://owner:{secret}@cache.invalid:6381"
        )
        with self.assertRaises(ValueError) as error:
            self.verify_multilevel()
        self.assertNotIn(secret, str(error.exception))
        for endpoint in (
            f"redis://owner:{secret}@cache.invalid:6381",
            "redis://[invalid",
        ):
            with self.assertRaisesRegex(ValueError, "identity is invalid") as error:
                self.verify_multilevel(endpoint=endpoint)
            self.assertNotIn(secret, str(error.exception))

    def test_multilevel_does_not_relax_existing_hit_or_version_guards(self):
        self.multilevel()
        self.after["version"] = "0.16.0"
        with self.assertRaisesRegex(ValueError, "serving daemon version"):
            self.verify_multilevel()
        self.after["version"] = "0.18.0"
        self.after["stats"]["cache_hits"] = copy.deepcopy(
            self.before["stats"]["cache_hits"]
        )
        with self.assertRaisesRegex(ValueError, "real Rust cache hit"):
            self.verify_multilevel()

    def test_receipt_redacts_redis_credentials_query_and_prefix(self):
        secret = "closed-fixture-private-token"
        levels = self.multilevel()
        levels[1]["location"] = (
            f"redis, name: redis://owner:{secret}@cache.invalid:6381/0?token={secret}, "
            f"prefix: /{secret}/"
        )
        rendered = json.dumps(PROOF.sanitized_snapshot(self.after))
        self.assertNotIn(secret, rendered)
        self.assertNotIn("owner:", rendered)
        self.assertIn("redis://cache.invalid:6381/0", rendered)
        self.assertEqual(
            PROOF.sanitized_snapshot(self.after)["stats"]["cache_hits"],
            self.after["stats"]["cache_hits"],
        )

    def test_multilevel_background_write_errors_cannot_hide_behind_l0_success(self):
        """Protects Redis error admission; detects L1 failures with top errors0."""
        levels = self.multilevel()
        unchanged_top = copy.deepcopy(
            {
                key: value
                for key, value in self.after["stats"].items()
                if key != "multi_level"
            }
        )
        levels[1]["write_failures"] = 1
        self.assertEqual(
            unchanged_top,
            {
                key: value
                for key, value in self.after["stats"].items()
                if key != "multi_level"
            },
        )
        with self.assertRaisesRegex(ValueError, "changed cache-level write_failures"):
            self.verify_multilevel()

    def test_multilevel_write_error_counters_are_nonnegative_integers(self):
        for side in ("before", "after"):
            for index in (0, 1):
                for invalid in (None, True, -1, 0.5, "0"):
                    self.multilevel()
                    getattr(self, side)["stats"]["multi_level"][index][
                        "write_failures"
                    ] = invalid
                    with self.subTest(side=side, level=index, value=invalid):
                        with self.assertRaisesRegex(ValueError, "invalid cache-level"):
                            self.verify_multilevel()

    def test_new_error_counter_is_refused(self):
        for key in (
            "cache_timeouts",
            "cache_read_errors",
            "cache_write_errors",
            "compile_fails",
            "requests_unsupported_compiler",
            "requests_not_cacheable",
        ):
            with self.subTest(counter=key):
                self.after["stats"][key] = 1
                with self.assertRaisesRegex(ValueError, key):
                    self.verify()
                self.after["stats"][key] = 0


if __name__ == "__main__":
    unittest.main()
