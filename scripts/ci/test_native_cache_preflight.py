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
