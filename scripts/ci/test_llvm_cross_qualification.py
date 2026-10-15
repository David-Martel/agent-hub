"""Protects F4 admission; detects ABI/provider/cache errors using service-free controls.

Needs Python stdlib and adjacent compiler/native-cache helpers.
Breadcrumb: docs/fleet-reconciliation.md F4; llvm_cross_qualification.py.
"""

import copy
import json
import pathlib
import socket
import struct
import tempfile
import unittest
from unittest import mock

from llvm_cross_qualification import (
    TARGET,
    admitted_tool,
    capture_wine_os_manifest,
    collect_runtime_closure,
    compiler_bindings,
    pe_imports,
    read_api_set_aliases,
    require_unused_port,
    resolve_api_set,
    sha256,
    validate_language_cache,
    validate_llvm_version,
    validate_private_cache,
    validate_ucrt,
)


def image(dll):
    data = bytearray(1024)
    data[:2] = b"MZ"
    struct.pack_into("<I", data, 0x3C, 128)
    data[128:132] = b"PE\0\0"
    struct.pack_into("<HH", data, 132, 0x8664, 1)
    struct.pack_into("<H", data, 148, 240)
    struct.pack_into("<H", data, 152, 0x20B)
    struct.pack_into("<I", data, 212, 512)
    struct.pack_into("<I", data, 260, 16)
    struct.pack_into("<II", data, 272, 0x1000, 40)
    struct.pack_into("<IIII", data, 400, 512, 0x1000, 512, 512)
    struct.pack_into("<IIIII", data, 512, 0, 0, 0, 0x1080, 0)
    encoded = dll.encode() + b"\0"
    data[640 : 640 + len(encoded)] = encoded
    return data


def api_schema(host="ucrtbase.dll", contract_name="api-ms-win-crt-runtime-l1-1-0"):
    data = image("kernel32.dll")
    data[392:400] = b".apiset\0"
    namespace = bytearray(512)
    contract = contract_name.encode("utf-16-le")
    encoded_host = host.encode("utf-16-le")
    prefix = contract_name.rsplit("-", 1)[0]
    hashed = 0
    for character in prefix:
        hashed = (hashed * 31 + ord(character)) & 0xFFFFFFFF
    struct.pack_into("<IIIIIII", namespace, 0, 6, 512, 0, 1, 28, 480, 31)
    struct.pack_into(
        "<IIIIII", namespace, 28, 0, 64, len(contract), len(prefix) * 2, 160, 1
    )
    namespace[64 : 64 + len(contract)] = contract
    struct.pack_into("<IIIII", namespace, 160, 0, 0, 0, 200, len(encoded_host))
    namespace[200 : 200 + len(encoded_host)] = encoded_host
    struct.pack_into("<II", namespace, 480, hashed, 0)
    data[512:1024] = namespace
    return data


def importer_schema(importer="special.dll", provider="kernel32.dll"):
    data = api_schema("kernelbase.dll", "api-ms-win-core-synch-l1-2-1")
    selector = importer.encode("utf-16-le")
    host = provider.encode("utf-16-le")
    struct.pack_into("<I", data, 512 + 28 + 20, 2)
    struct.pack_into("<IIIII", data, 512 + 180, 0, 300, len(selector), 340, len(host))
    data[512 + 300 : 512 + 300 + len(selector)] = selector
    data[512 + 340 : 512 + 340 + len(host)] = host
    return data


class PeQualificationTests(unittest.TestCase):
    """Protects PE/UCRT identity; detects wrong ABI and truncation.

    Needs in-memory PE fixtures. Breadcrumb: pe_imports/validate_ucrt.
    """

    def test_actual_imports_establish_ucrt(self):
        imports = pe_imports(image("ucrtbase.dll"))
        self.assertEqual(imports, ["ucrtbase.dll"])
        validate_ucrt(imports)

    def test_legacy_crt_is_refused(self):
        with self.assertRaisesRegex(ValueError, "mixture"):
            validate_ucrt(pe_imports(image("msvcrt.dll")))

    def test_gcc_runtime_mixed_with_ucrt_is_refused(self):
        with self.assertRaisesRegex(ValueError, "mixture"):
            validate_ucrt(["ucrtbase.dll", "libgcc_s_seh-1.dll"])

    def test_gnu_cxx_runtime_mixed_with_ucrt_is_refused(self):
        with self.assertRaisesRegex(ValueError, "mixture"):
            validate_ucrt(["ucrtbase.dll", "libstdc++-6.dll"])

    def test_missing_ucrt_is_not_accepted(self):
        with self.assertRaisesRegex(ValueError, "not demonstrated"):
            validate_ucrt(["kernel32.dll"])

    def test_wrong_architecture_is_refused(self):
        data = image("ucrtbase.dll")
        struct.pack_into("<H", data, 132, 0x14C)
        with self.assertRaisesRegex(ValueError, "AMD64"):
            pe_imports(data)

    def test_out_of_bounds_import_name_is_refused(self):
        data = image("ucrtbase.dll")
        struct.pack_into("<I", data, 524, 0x1800)
        with self.assertRaisesRegex(ValueError, "outside section"):
            pe_imports(data)

    def test_truncated_optional_header_is_refused(self):
        with self.assertRaisesRegex(ValueError, "Truncated"):
            pe_imports(image("ucrtbase.dll")[:200])


class CompilerBindingTests(unittest.TestCase):
    """Protects explicit LLVM selection; detects target/escaped-tool substitution.

    Needs temporary public tool fixtures. Breadcrumb: compiler_bindings.
    """

    def test_gnu_target_cannot_silently_consume_llvm(self):
        with self.assertRaisesRegex(ValueError, "gnullvm"):
            compiler_bindings("unused", "x86_64-pc-windows-gnu")

    def test_explicit_paths_select_clang_and_llvm_archive(self):
        with tempfile.TemporaryDirectory() as directory:
            prefix = pathlib.Path(directory)
            (prefix / "bin").mkdir()
            for name in (
                "x86_64-w64-mingw32-clang",
                "x86_64-w64-mingw32-clang++",
                "llvm-ar",
            ):
                (prefix / "bin" / name).write_text("fixture")
            bindings = compiler_bindings(prefix, TARGET)
            self.assertTrue(bindings["CC_x86_64_pc_windows_gnullvm"].endswith("-clang"))
            self.assertTrue(
                bindings["AR_x86_64_pc_windows_gnullvm"].endswith("llvm-ar")
            )

    def test_resolved_tool_escape_is_refused(self):
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            prefix = root / "llvm"
            (prefix / "bin").mkdir(parents=True)
            foreign = root / "foreign-clang"
            foreign.write_text("fixture")
            original = pathlib.Path.resolve

            def resolve(path, *args, **kwargs):
                if path.name == "x86_64-w64-mingw32-clang":
                    return foreign
                return original(path, *args, **kwargs)

            with (
                mock.patch.object(pathlib.Path, "resolve", resolve),
                self.assertRaisesRegex(ValueError, "outside"),
            ):
                compiler_bindings(prefix, TARGET)

    def test_nearby_llvm_release_is_refused(self):
        for version in (
            "clang version 23.1.30",
            "LLD 123.1.3",
            "LLVM version 23.1.3git",
        ):
            with (
                self.subTest(version=version),
                self.assertRaisesRegex(ValueError, "version"),
            ):
                validate_llvm_version(version)
        validate_llvm_version("Ubuntu clang version 23.1.3 (release)")

    def test_linker_resolution_escape_is_refused(self):
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            prefix = root / "llvm"
            (prefix / "bin").mkdir(parents=True)
            foreign = root / "foreign-linker"
            foreign.write_text("fixture")
            original = pathlib.Path.resolve

            def resolve(path, *args, **kwargs):
                return (
                    foreign
                    if path.name == "ld.lld"
                    else original(path, *args, **kwargs)
                )

            with (
                mock.patch.object(pathlib.Path, "resolve", resolve),
                self.assertRaisesRegex(ValueError, "outside"),
            ):
                admitted_tool(prefix, "ld.lld")


class RuntimeClosureTests(unittest.TestCase):
    """Protects complete DLL closure; detects transitive GNU CRT and missing providers.

    Needs temporary PE/provider fixtures. Breadcrumb: collect_runtime_closure.
    """

    def test_cpp_runtime_importing_msvcrt_is_refused(self):
        self.assert_transitive_refused("msvcrt.dll")

    def test_cpp_runtime_importing_gcc_is_refused(self):
        self.assert_transitive_refused("libgcc_s_seh-1.dll")

    def test_cpp_runtime_importing_gnu_cpp_is_refused(self):
        self.assert_transitive_refused("libstdc++-6.dll")

    def assert_transitive_refused(self, dependency):
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            dlls = root / "llvm/x86_64-w64-mingw32/bin"
            dlls.mkdir(parents=True)
            (dlls / "libc++.dll").write_bytes(image(dependency))
            artifacts = {"cpp": {"imports": ["ucrtbase.dll", "libc++.dll"]}}
            with self.assertRaisesRegex(ValueError, "mixture"):
                collect_runtime_closure(artifacts, root / "llvm", root, {})

    def test_unresolved_provider_is_not_assumed_system(self):
        with tempfile.TemporaryDirectory() as directory:
            artifacts = {"c": {"imports": ["unknown-provider.dll"]}}
            with self.assertRaisesRegex(ValueError, "Unresolved"):
                collect_runtime_closure(artifacts, directory, directory, {})

    def test_changed_os_provider_is_refused(self):
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            provider = root / "ucrtbase.dll"
            provider.write_text("original")
            manifest = {
                "providers": {
                    "ucrtbase.dll": {"path": str(provider), "sha256": sha256(provider)}
                }
            }
            provider.write_text("changed")
            with self.assertRaisesRegex(ValueError, "changed"):
                collect_runtime_closure(
                    {"c": {"imports": ["ucrtbase.dll"]}}, root / "llvm", root, manifest
                )

    def test_exact_os_provider_is_recorded(self):
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            provider = root / "ucrtbase.dll"
            provider.write_bytes(image("kernel32.dll"))
            manifest = {
                "providers": {
                    "ucrtbase.dll": {"path": str(provider), "sha256": sha256(provider)}
                }
            }
            bundled, providers = collect_runtime_closure(
                {"c": {"imports": ["ucrtbase.dll"]}}, root / "llvm", root, manifest
            )
            self.assertEqual(bundled, {})
            self.assertEqual(providers["ucrtbase.dll"]["sha256"], sha256(provider))

    def test_api_set_alias_comes_from_actual_schema(self):
        self.assertEqual(
            read_api_set_aliases(api_schema()),
            {"api-ms-win-crt-runtime-l1-1-0.dll": "ucrtbase.dll"},
        )

    def test_minor_contract_uses_physical_schema_hashed_prefix(self):
        binding = resolve_api_set(
            api_schema("kernelbase.dll", "api-ms-win-core-synch-l1-2-1"),
            "api-ms-win-core-synch-l1-2-0.dll",
            "rs.exe",
        )
        self.assertEqual(binding["schema_contract"], "api-ms-win-core-synch-l1-2-1.dll")
        self.assertEqual(binding["provider"], "kernelbase.dll")
        self.assertEqual(binding["selector"], "default")

    def test_importer_override_requires_exact_basename(self):
        name = "api-ms-win-core-synch-l1-2-0.dll"
        schema = importer_schema()
        self.assertEqual(
            resolve_api_set(schema, name, "special.dll")["provider"], "kernel32.dll"
        )
        self.assertEqual(
            resolve_api_set(schema, name, "special.dll.exe")["provider"],
            "kernelbase.dll",
        )

    def test_other_contract_major_prefix_is_refused(self):
        with self.assertRaisesRegex(ValueError, "Unknown API-set"):
            resolve_api_set(
                importer_schema(), "api-ms-win-core-synch-l1-3-0.dll", "rs.exe"
            )

    def test_ambiguous_hash_lookup_is_refused(self):
        data = api_schema("kernelbase.dll", "api-ms-win-core-synch-l1-2-1")
        hashed = struct.unpack_from("<I", data, 512 + 480)[0]
        struct.pack_into("<I", data, 512 + 12, 2)
        struct.pack_into("<II", data, 512 + 488, hashed, 1)
        with self.assertRaisesRegex(ValueError, "Ambiguous"):
            resolve_api_set(data, "api-ms-win-core-synch-l1-2-0.dll", "rs.exe")

    def test_selected_empty_default_does_not_use_unrelated_override(self):
        data = importer_schema()
        struct.pack_into("<I", data, 512 + 160 + 12, 0)
        with self.assertRaisesRegex(ValueError, "Missing API-set selected"):
            resolve_api_set(data, "api-ms-win-core-synch-l1-2-0.dll", "rs.exe")

    def test_duplicate_importer_overrides_are_refused(self):
        data = importer_schema()
        default = "kernelbase.dll".encode("utf-16-le")
        data[512 + 240 : 512 + 240 + len(default)] = default
        struct.pack_into("<I", data, 512 + 160 + 12, 240)
        data[512 + 200 : 512 + 220] = data[512 + 180 : 512 + 200]
        struct.pack_into("<I", data, 512 + 28 + 20, 3)
        with self.assertRaisesRegex(ValueError, "Ambiguous API-set importer"):
            resolve_api_set(data, "api-ms-win-core-synch-l1-2-0.dll", "special.dll")

    def test_unsafe_importer_selector_is_refused(self):
        with self.assertRaisesRegex(ValueError, "selector"):
            resolve_api_set(
                importer_schema("../special.dll"),
                "api-ms-win-core-synch-l1-2-0.dll",
                "rs.exe",
            )

    def test_importer_selected_legacy_crt_is_refused(self):
        with self.assertRaisesRegex(ValueError, "mixture"):
            resolve_api_set(
                importer_schema(provider="msvcrt.dll"),
                "api-ms-win-core-synch-l1-2-0.dll",
                "special.dll",
            )

    def test_each_importer_provider_binding_is_retained(self):
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            schema = root / "apisetschema.dll"
            schema.write_bytes(importer_schema("special.exe"))
            providers = {}
            for name in ("kernel32.dll", "kernelbase.dll"):
                provider = root / name
                provider.write_bytes(image("ntdll.dll"))
                providers[name] = {"path": str(provider), "sha256": sha256(provider)}
            manifest = {
                "providers": providers,
                "aliases": {},
                "alias_proof": {"path": str(schema), "sha256": sha256(schema)},
            }
            name = "api-ms-win-core-synch-l1-2-0.dll"
            artifacts = {key: {"imports": [name]} for key in ("ordinary", "special")}
            _, closure = collect_runtime_closure(
                artifacts, root / "llvm", root, manifest
            )
            self.assertEqual(
                closure[name]["importers"]["special.exe"]["resolved_name"],
                "kernel32.dll",
            )
            self.assertEqual(
                closure[name]["importers"]["ordinary.exe"]["resolved_name"],
                "kernelbase.dll",
            )

    def test_wine_printer_drv_alias_is_decoded(self):
        self.assertEqual(
            read_api_set_aliases(
                api_schema("winspool.drv", "ext-ms-win-printer-winspool-core-l1-1-0")
            ),
            {"ext-ms-win-printer-winspool-core-l1-1-0.dll": "winspool.drv"},
        )

    def test_actual_drv_provider_is_snapshotted_and_hash_bound(self):
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            for name in ("kernel32.dll", "ntdll.dll", "ucrtbase.dll", "winspool.drv"):
                (root / name).write_bytes(image("kernel32.dll"))
            schema = root / "apisetschema.dll"
            schema.write_bytes(
                api_schema("winspool.drv", "ext-ms-win-printer-winspool-core-l1-1-0")
            )
            manifest = capture_wine_os_manifest(root, root / "providers.json")
            name = "ext-ms-win-printer-winspool-core-l1-1-0.dll"
            _, providers = collect_runtime_closure(
                {"c": {"imports": [name]}}, root / "llvm", root, manifest
            )
            self.assertEqual(providers[name]["resolved_name"], "winspool.drv")
            self.assertEqual(providers[name]["sha256"], sha256(root / "winspool.drv"))
            (root / "winspool.drv").write_bytes(b"changed")
            with self.assertRaisesRegex(ValueError, "changed"):
                collect_runtime_closure(
                    {"c": {"imports": [name]}}, root / "llvm", root, manifest
                )

    def test_drv_alias_path_escape_is_refused(self):
        with self.assertRaisesRegex(ValueError, "Invalid API-set provider"):
            read_api_set_aliases(api_schema("../winspool.drv"))

    def test_api_set_out_of_bounds_value_is_refused(self):
        data = api_schema()
        struct.pack_into("<I", data, 512 + 28 + 16, 500)
        with self.assertRaisesRegex(ValueError, "outside bounds"):
            read_api_set_aliases(data)

    def test_false_alias_does_not_match_admitted_schema(self):
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            schema = root / "apisetschema.dll"
            schema.write_bytes(api_schema("kernel32.dll"))
            provider = root / "ucrtbase.dll"
            provider.write_bytes(image("kernel32.dll"))
            name = "api-ms-win-crt-runtime-l1-1-0.dll"
            manifest = {
                "providers": {
                    "ucrtbase.dll": {"path": str(provider), "sha256": sha256(provider)}
                },
                "aliases": {name: "ucrtbase.dll"},
                "alias_proof": {"path": str(schema), "sha256": sha256(schema)},
            }
            with self.assertRaisesRegex(ValueError, "actual Wine schema"):
                collect_runtime_closure(
                    {"c": {"imports": [name]}}, root / "llvm", root, manifest
                )

    def test_hash_bound_wrong_architecture_os_provider_is_refused(self):
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            provider = root / "ucrtbase.dll"
            data = image("kernel32.dll")
            struct.pack_into("<H", data, 132, 0x14C)
            provider.write_bytes(data)
            manifest = {
                "providers": {
                    "ucrtbase.dll": {"path": str(provider), "sha256": sha256(provider)}
                }
            }
            with self.assertRaisesRegex(ValueError, "AMD64"):
                collect_runtime_closure(
                    {"c": {"imports": ["ucrtbase.dll"]}}, root / "llvm", root, manifest
                )


def snapshot():
    return {
        "version": "0.18.0",
        "stats": {
            "requests_executed": 0,
            "cache_hits": {"counts": {}},
            "cache_errors": {"counts": {}},
            **{
                key: 0
                for key in (
                    "cache_timeouts",
                    "cache_read_errors",
                    "cache_write_errors",
                    "compile_fails",
                    "requests_unsupported_compiler",
                    "requests_not_cacheable",
                    "non_cacheable_compilations",
                )
            },
        },
    }


class CacheOwnershipTests(unittest.TestCase):
    """Protects owned daemons and real reuse; detects occupancy, mismatch and failures.

    Needs temporary paths and an ephemeral localhost socket, no bus/services.
    Breadcrumb: require_unused_port/validate_private_cache/validate_language_cache.
    """

    def test_occupied_port_is_refused_without_daemon_mutation(self):
        with (
            socket.create_server(("localhost", 0)) as listener,
            self.assertRaisesRegex(ValueError, "Occupied"),
        ):
            require_unused_port(listener.getsockname()[1])

    def test_foreign_cache_directory_is_refused(self):
        state = snapshot()
        state["cache_location"] = "Local disk: " + json.dumps(
            str(pathlib.Path("foreign").resolve())
        )
        with self.assertRaisesRegex(ValueError, "directory"):
            validate_private_cache(state, "owned")

    def test_wrong_cache_version_is_refused(self):
        state = snapshot()
        state["version"] = "0.14.0"
        with self.assertRaisesRegex(ValueError, "version"):
            validate_private_cache(state, "owned")

    def test_each_language_without_a_cache_hit_is_refused(self):
        for language in ("c", "cpp", "rs"):
            with self.subTest(language=language):
                before = snapshot()
                after = copy.deepcopy(before)
                after["stats"]["requests_executed"] = 2
                with self.assertRaisesRegex(ValueError, "cache hit"):
                    validate_language_cache(before, after, language)

    def test_compiler_failure_cannot_become_cache_pass(self):
        before = snapshot()
        after = copy.deepcopy(before)
        after["stats"]["requests_executed"] = 2
        after["stats"]["cache_hits"]["counts"]["Rust"] = 1
        after["stats"]["compile_fails"] = 1
        with self.assertRaisesRegex(ValueError, "compile_fails"):
            validate_language_cache(before, after, "rs")

    def test_unexpected_language_hit_is_refused(self):
        before = snapshot()
        after = copy.deepcopy(before)
        after["stats"]["requests_executed"] = 2
        after["stats"]["cache_hits"]["counts"]["Rust"] = 1
        with self.assertRaisesRegex(ValueError, "classification"):
            validate_language_cache(before, after, "cpp")

    def test_actual_rust_cache_pair_is_accepted(self):
        before = snapshot()
        after = copy.deepcopy(before)
        after["stats"]["requests_executed"] = 2
        after["stats"]["cache_hits"]["counts"]["Rust"] = 1
        validate_language_cache(before, after, "rs")


if __name__ == "__main__":
    unittest.main()
