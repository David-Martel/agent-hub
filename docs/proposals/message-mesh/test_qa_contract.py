"""Evidence-controller regressions; no subprocesses, sockets or backend access."""

import importlib.util
import json
from pathlib import Path
import tempfile
import unittest

SPEC = importlib.util.spec_from_file_location(
    "qa_contract", Path(__file__).with_name("qa-contract.py")
)
qa = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(qa)

CONTRACT = {
    "tests": [
        {"name": "mesh_store::tests::durable", "ignored": True},
        {"name": "mesh_capture::tests::capture", "ignored": True},
        {"name": "peer_sync::tests::three_peers", "ignored": False},
    ]
}
LISTING = (
    "mesh_store::tests::durable: test\n"
    "mesh_capture::tests::capture: test\nlegacy::backend: test\n"
)
OUTPUT = (
    "test mesh_store::tests::durable ... ok\n"
    "test mesh_capture::tests::capture ... ok\n"
    "test legacy::backend ... ok\n"
    "test result: ok. 3 passed; 0 failed; 0 ignored; 0 measured; 5 filtered out;\n"
)


class EvidenceContracts(unittest.TestCase):
    def test_all_backend_names_and_summary_must_agree(self):
        receipt = qa.check_result(CONTRACT, "backend", LISTING, OUTPUT, 0)
        self.assertEqual(receipt["passed"], 3)
        self.assertEqual(receipt["required_mesh_tests"], 2)
        self.assertEqual(receipt["state"], "TEST_OUTPUT_PASS_CLEANUP_STILL_REQUIRED")

    def test_aggregate_floor_cannot_hide_an_omitted_mesh_test(self):
        forged = OUTPUT.replace("test mesh_capture::tests::capture ... ok\n", "")
        with self.assertRaisesRegex(qa.ContractError, "exactly once"):
            qa.check_result(CONTRACT, "backend", LISTING, forged, 0)

    def test_missing_module_wiring_is_rejected_before_execution(self):
        for listing in [
            "",
            "legacy::backend: test\n",
            LISTING.replace("mesh_store::tests::durable: test\n", ""),
        ]:
            with self.assertRaises(qa.ContractError):
                qa.check_listing(CONTRACT, "backend", listing)

    def test_duplicate_test_or_output_cannot_substitute_for_coverage(self):
        with self.assertRaisesRegex(qa.ContractError, "repeated"):
            qa.check_listing(
                CONTRACT, "backend", LISTING + "mesh_store::tests::durable: test\n"
            )
        with self.assertRaisesRegex(qa.ContractError, "exactly once"):
            qa.check_result(
                CONTRACT,
                "backend",
                LISTING,
                OUTPUT + "test mesh_store::tests::durable ... ok\n",
                0,
            )

    def test_every_compiled_backend_including_legacy_must_pass(self):
        output = OUTPUT.replace("test legacy::backend ... ok\n", "")
        with self.assertRaisesRegex(qa.ContractError, "pass set"):
            qa.check_result(CONTRACT, "backend", LISTING, output, 0)

    def test_failure_exit_skipped_backend_or_missing_summary_is_rejected(self):
        for output, code in [
            (OUTPUT, 101),
            (OUTPUT.replace("0 ignored", "1 ignored"), 0),
            (OUTPUT.split("test result:")[0], 0),
            (OUTPUT.replace("3 passed", "4 passed"), 0),
            (OUTPUT.replace("legacy::backend ... ok", "legacy::backend ... FAILED"), 0),
        ]:
            with self.assertRaises(qa.ContractError):
                qa.check_result(CONTRACT, "backend", LISTING, output, code)

    def test_unit_selection_requires_real_three_node_test_and_allows_ignored_backends(
        self,
    ):
        output = (
            "test peer_sync::tests::three_peers ... ok\n"
            "test mesh_store::tests::durable ... ignored\n"
            "test result: ok. 1 passed; 0 failed; 1 ignored;\n"
        )
        receipt = qa.check_result(
            CONTRACT, "unit", "peer_sync::tests::three_peers: test\n", output, 0
        )
        self.assertEqual(receipt["passed"], 1)
        with self.assertRaisesRegex(qa.ContractError, "missing"):
            qa.check_result(CONTRACT, "unit", "some_other::test: test\n", output, 0)

    def test_manifest_binds_all_source_bytes_lock_and_inventory(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            source = root / "source"
            source.mkdir()
            (source / "Cargo.lock").write_text("owned lock", encoding="utf-8")
            (source / "main.rs").write_text("fn main() {}", encoding="utf-8")
            manifest = root / "manifest.json"
            manifest.write_text(
                json.dumps(
                    {name: qa.sha(source / name) for name in ["Cargo.lock", "main.rs"]}
                ),
                encoding="utf-8",
            )
            bound = qa.sha(manifest)
            self.assertEqual(
                qa.source_guard(source, manifest, bound)["source_files"], 2
            )
            (source / "extra.rs").write_text("extra", encoding="utf-8")
            with self.assertRaisesRegex(qa.ContractError, "inventory"):
                qa.source_guard(source, manifest, bound)
            (source / "extra.rs").unlink()
            (source / "main.rs").write_text("changed", encoding="utf-8")
            with self.assertRaisesRegex(qa.ContractError, "inventory"):
                qa.source_guard(source, manifest, bound)

    def test_source_manifest_refuses_path_escape_and_changed_manifest(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            manifest = root / "manifest.json"
            manifest.write_text(json.dumps({"../escape": "a" * 64}), encoding="utf-8")
            with self.assertRaisesRegex(qa.ContractError, "unsafe"):
                qa.source_guard(root, manifest, qa.sha(manifest))
            with self.assertRaisesRegex(qa.ContractError, "manifest changed"):
                qa.source_guard(root, manifest, "0" * 64)

    def test_existing_receipt_is_preserved(self):
        with tempfile.TemporaryDirectory() as directory:
            receipt = Path(directory) / "receipt.json"
            qa.write_new(receipt, {"state": "FIRST_FAILURE"})
            with self.assertRaises(FileExistsError):
                qa.write_new(receipt, {"state": "PASS"})
            self.assertEqual(json.loads(receipt.read_text())["state"], "FIRST_FAILURE")


if __name__ == "__main__":
    unittest.main()
