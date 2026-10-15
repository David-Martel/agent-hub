"""Qualify explicitly selected LLVM/UCRT tools; never substitute them for GCC."""

from __future__ import annotations

import argparse
import hashlib
import json
import re
import shutil
import socket
import struct
import subprocess
import time
from pathlib import Path

from native_cache_preflight import validate_identity

TARGET = "x86_64-pc-windows-gnullvm"
LLVM_VERSION = "23.1.3"
RUST_VERSION = "1.99.0"
SCCACHE_VERSION = "0.18.0"


def reject_legacy_runtime(imports):
    """Refuse legacy CRT, GCC unwinding and GNU C++ DLLs at every bundle edge."""
    if any(
        name == "msvcrt.dll" or name.startswith(("libgcc", "libstdc++"))
        for name in imports
    ):
        raise ValueError("GNU/MSVCRT runtime mixture refused")


def require_unused_port(port):
    """Refuse occupied ports before issuing cache RPCs or compiler requests."""
    if not 1 <= port <= 65535:
        raise ValueError("Invalid private cache port")
    try:
        connection = socket.create_connection(("localhost", port), timeout=0.3)
    except ConnectionRefusedError:
        return
    except OSError as error:
        raise ValueError("Cannot establish exclusive private cache port") from error
    connection.close()
    raise ValueError("Occupied cache port refused; no daemon was stopped")


def validate_private_cache(snapshot, directory):
    """Bind the serving version and exact local cache before requests or stop."""
    validate_identity(snapshot, SCCACHE_VERSION, directory)
    location = snapshot.get("cache_location", "")
    if not location.startswith("Local disk: "):
        raise ValueError("Only the dedicated local cache is admitted")
    actual = Path(json.loads(location.removeprefix("Local disk: "))).resolve()
    if actual != Path(directory).resolve():
        raise ValueError("Serving cache is outside the private owned directory")


def validate_language_cache(before, after, language):
    """Require each isolated language pair to demonstrate its own real reuse."""
    old, new = before["stats"], after["stats"]
    if new["requests_executed"] - old["requests_executed"] != 2:
        raise ValueError("Language pair did not execute two actual requests")
    hits = new["cache_hits"]["counts"]
    old_hits = old["cache_hits"]["counts"]
    if sum(hits.values()) - sum(old_hits.values()) != 1:
        raise ValueError("Language pair did not demonstrate one cache hit")
    rust_delta = hits.get("Rust", 0) - old_hits.get("Rust", 0)
    if rust_delta != (1 if language == "rs" else 0):
        raise ValueError(
            "Cache language classification differs from the compiled source"
        )
    for key in (
        "cache_timeouts",
        "cache_read_errors",
        "cache_write_errors",
        "compile_fails",
        "requests_unsupported_compiler",
        "requests_not_cacheable",
        "non_cacheable_compilations",
    ):
        if new[key] != old[key]:
            raise ValueError("Language pair increased " + key)
    if new["cache_errors"] != old["cache_errors"]:
        raise ValueError("Language pair increased backend errors")


def sha256(path):
    return hashlib.sha256(Path(path).read_bytes()).hexdigest()


def admitted_tool(prefix, name):
    """Reject every compiler, linker and archive implementation escaping its package."""
    prefix = Path(prefix).resolve(strict=True)
    path = prefix / "bin" / name
    resolved = path.resolve(strict=True)
    if not resolved.is_relative_to(prefix) or not resolved.is_file():
        raise ValueError("LLVM tool resolves outside admitted compiler package")
    return path


def validate_llvm_version(version):
    """Require the exact release token, including for compiler and linker drivers."""
    if not re.search(r"(?<![\w.])" + re.escape(LLVM_VERSION) + r"(?![\w.])", version):
        raise ValueError("Actual LLVM tool version differs from admitted release")


def validate_pe_amd64(data):
    """Bind the architecture of Wine providers even without an import directory."""
    if len(data) < 64 or data[:2] != b"MZ":
        raise ValueError("Expected AMD64 Wine PE provider")
    pe = struct.unpack_from("<I", data, 0x3C)[0]
    if pe + 26 > len(data) or data[pe : pe + 4] != b"PE\0\0":
        raise ValueError("Expected AMD64 Wine PE provider")
    if (
        struct.unpack_from("<H", data, pe + 4)[0] != 0x8664
        or struct.unpack_from("<H", data, pe + 24)[0] != 0x20B
    ):
        raise ValueError("Expected AMD64 Wine PE provider")


def compiler_bindings(prefix, target):
    """Refuse GNU target reuse and tools escaping the selected LLVM package."""
    if target != TARGET:
        raise ValueError("LLVM/UCRT qualification requires explicit gnullvm target")
    prefix = Path(prefix).resolve(strict=True)
    names = {
        "CARGO_TARGET_X86_64_PC_WINDOWS_GNULLVM_LINKER": "x86_64-w64-mingw32-clang",
        "CC_x86_64_pc_windows_gnullvm": "x86_64-w64-mingw32-clang",
        "CXX_x86_64_pc_windows_gnullvm": "x86_64-w64-mingw32-clang++",
        "AR_x86_64_pc_windows_gnullvm": "llvm-ar",
    }
    result = {}
    for key, name in names.items():
        path = admitted_tool(prefix, name)
        result[key] = str(path)
    return result


def pe_imports(data):
    """Read actual PE64 DLL imports with bounded RVA and string validation."""

    def need(offset, count):
        if offset < 0 or offset + count > len(data):
            raise ValueError("Truncated PE structure")

    need(0, 64)
    if data[:2] != b"MZ":
        raise ValueError("Missing DOS header")
    pe = struct.unpack_from("<I", data, 0x3C)[0]
    need(pe, 24)
    if data[pe : pe + 4] != b"PE\0\0":
        raise ValueError("Missing PE signature")
    machine, sections = struct.unpack_from("<HH", data, pe + 4)
    size = struct.unpack_from("<H", data, pe + 20)[0]
    optional = pe + 24
    need(optional, size)
    if machine != 0x8664 or size < 128:
        raise ValueError("Expected AMD64 PE64")
    if struct.unpack_from("<H", data, optional)[0] != 0x20B:
        raise ValueError("Expected PE32+ optional header")
    directory_count = struct.unpack_from("<I", data, optional + 108)[0]
    if directory_count < 2:
        raise ValueError("Missing PE import directory")
    import_rva, import_size = struct.unpack_from("<II", data, optional + 120)
    headers_size = struct.unpack_from("<I", data, optional + 60)[0]
    section_table = optional + size
    need(section_table, sections * 40)

    def rva_offset(rva):
        if rva < headers_size:
            need(rva, 1)
            return rva
        for index in range(sections):
            section = section_table + index * 40
            virtual_size, virtual, raw_size, raw = struct.unpack_from(
                "<IIII", data, section + 8
            )
            if virtual <= rva < virtual + max(virtual_size, raw_size):
                relative = rva - virtual
                if relative >= raw_size:
                    raise ValueError("PE RVA refers to unbacked section data")
                offset = raw + relative
                need(offset, 1)
                return offset
        raise ValueError("PE RVA is outside section bounds")

    if not import_rva or import_size < 20:
        raise ValueError("Expected explicit runtime DLL imports")
    imports = []
    for index in range(min(import_size // 20, 256)):
        descriptor = rva_offset(import_rva + index * 20)
        need(descriptor, 20)
        fields = struct.unpack_from("<IIIII", data, descriptor)
        if not any(fields):
            return imports
        name_offset = rva_offset(fields[3])
        end = data.find(b"\0", name_offset, min(name_offset + 512, len(data)))
        if end == -1:
            raise ValueError("Unterminated DLL import name")
        name = data[name_offset:end].decode("ascii").lower()
        if not re.fullmatch(r"[a-z0-9_.+-]+\.(dll|drv)", name):
            raise ValueError("Invalid DLL import name")
        imports.append(name)
    raise ValueError("Unterminated PE import descriptor list")


def validate_ucrt(imports):
    """Reject legacy CRT and GCC runtimes rather than masking an ABI mixture."""
    reject_legacy_runtime(imports)
    if not any(
        name == "ucrtbase.dll" or name.startswith("api-ms-win-crt-") for name in imports
    ):
        raise ValueError("UCRT imports were not demonstrated")


def collect_runtime_closure(artifacts, prefix, directory, os_manifest):
    """Bind copied DLLs and explicit immutable Wine providers; refuse unknowns."""
    providers = os_manifest.get("providers", {})
    aliases = os_manifest.get("aliases", {})
    runtime_dlls = {}
    os_dlls = {}
    pending = [
        (dll, Path(artifact.get("path", key + ".exe")).name.lower())
        for key, artifact in artifacts.items()
        for dll in artifact["imports"]
    ]
    examined = set()
    while pending:
        name, importer = pending.pop()
        name = name.lower()
        reject_legacy_runtime([name])
        if (name, importer) in examined:
            continue
        examined.add((name, importer))
        if name in runtime_dlls:
            continue
        provider = Path(prefix) / "x86_64-w64-mingw32" / "bin" / name
        if provider.is_file():
            resolved = provider.resolve(strict=True)
            if not resolved.is_relative_to(Path(prefix).resolve(strict=True)):
                raise ValueError("Runtime DLL escaped admitted LLVM package")
            imports = pe_imports(provider.read_bytes())
            reject_legacy_runtime(imports)
            destination = Path(directory) / name
            shutil.copyfile(provider, destination)
            runtime_dlls[name] = {
                "sha256": sha256(destination),
                "provider": str(provider),
                "imports": imports,
            }
            pending.extend((dependency, name) for dependency in imports)
            continue
        selected = aliases.get(name, name)
        api_binding = None
        if selected != name and not name.startswith(("api-", "ext-")):
            raise ValueError("Non-API runtime alias refused")
        if name.startswith(("api-", "ext-")):
            proof = os_manifest.get("alias_proof", {})
            if not proof.get("path") or sha256(proof["path"]) != proof.get("sha256"):
                raise ValueError(
                    "API-set aliases require unchanged explicit source proof"
                )
            data = Path(proof["path"]).read_bytes()
            if (
                name in aliases
                and read_api_set_aliases(data).get(name) != aliases[name]
            ):
                raise ValueError("API-set alias differs from the actual Wine schema")
            api_binding = resolve_api_set(data, name, importer)
            selected = api_binding["provider"]
        reject_legacy_runtime([selected])
        entry = providers.get(selected)
        if entry is None:
            raise ValueError("Unresolved runtime DLL provider: " + name)
        os_provider = Path(entry["path"])
        if not os_provider.is_file() or sha256(os_provider) != entry["sha256"]:
            raise ValueError("Admitted Wine OS provider changed: " + name)
        validate_pe_amd64(os_provider.read_bytes())
        binding = {
            "provider": str(os_provider),
            "sha256": entry["sha256"],
            "resolved_name": selected,
            "importer": importer,
            "api_binding": api_binding,
        }
        if name not in os_dlls:
            os_dlls[name] = {**binding, "importers": {}}
        os_dlls[name]["importers"][importer] = binding
    return runtime_dlls, os_dlls


def api_set_namespace(data):
    """Extract the bounded version-six namespace from physical Wine PE bytes."""
    if len(data) < 64 or data[:2] != b"MZ":
        raise ValueError("Invalid API-set schema PE")
    pe = struct.unpack_from("<I", data, 0x3C)[0]
    if pe + 24 > len(data) or data[pe : pe + 4] != b"PE\0\0":
        raise ValueError("Invalid API-set schema PE header")
    count = struct.unpack_from("<H", data, pe + 6)[0]
    optional_size = struct.unpack_from("<H", data, pe + 20)[0]
    sections = pe + 24 + optional_size
    namespace = None
    for index in range(count):
        section = sections + index * 40
        if section + 40 > len(data):
            raise ValueError("Truncated API-set section table")
        if data[section : section + 8].rstrip(b"\0") == b".apiset":
            size, offset = struct.unpack_from("<II", data, section + 16)
            if offset + size > len(data):
                raise ValueError("Truncated API-set namespace section")
            namespace = data[offset : offset + size]
    if namespace is None or len(namespace) < 28:
        raise ValueError("Missing API-set namespace")
    version, size, _, count, entries, hashes, factor = struct.unpack_from(
        "<IIIIIII", namespace
    )
    if version != 6 or size > len(namespace) or count > 16384:
        raise ValueError("Unsupported or malformed API-set namespace")
    return namespace[:size], count, entries, hashes, factor


def read_api_set_aliases(data):
    """Retain only unambiguous canonical aliases for manifest tamper checks."""
    namespace, count, entries, _, _ = api_set_namespace(data)

    def read(offset, length):
        if offset < 0 or offset + length > len(namespace):
            raise ValueError("API-set namespace offset outside bounds")
        return namespace[offset : offset + length]

    aliases = {}
    for index in range(count):
        fields = struct.unpack("<IIIIII", read(entries + index * 24, 24))
        _, name, name_length, _, values, value_count = fields
        contract = read(name, name_length).decode("utf-16-le").lower() + ".dll"
        if value_count > 256:
            raise ValueError("Malformed API-set value count")
        defaults, all_hosts = set(), set()
        for value_index in range(value_count):
            _, _, named_length, host, host_length = struct.unpack(
                "<IIIII", read(values + value_index * 20, 20)
            )
            if not host_length:
                continue
            host_name = read(host, host_length).decode("utf-16-le").lower()
            if not re.fullmatch(r"[a-z0-9_.+-]+\.(dll|drv)", host_name):
                raise ValueError("Invalid API-set provider name")
            all_hosts.add(host_name)
            if not named_length:
                defaults.add(host_name)
        selected = defaults if defaults else all_hosts
        if len(selected) == 1:
            aliases[contract] = selected.pop()
    return aliases


def resolve_api_set(data, requested, importer):
    """Bind Wine 11 hash-prefix lookup and default/importer-specific provider."""
    # Tagged Wine loader.c get_apiset_entry/get_apiset_target: the hashed
    # prefix ends before the final hyphen; value zero is the default and
    # subsequent entries override it only for an exact importing basename.
    namespace, count, entries, hashes, factor = api_set_namespace(data)

    def read(offset, length):
        if offset < 0 or offset + length > len(namespace):
            raise ValueError("API-set namespace offset outside bounds")
        return namespace[offset : offset + length]

    def wide(offset, length):
        if length % 2:
            raise ValueError("Malformed API-set UTF-16 length")
        return read(offset, length).decode("utf-16-le").lower()

    if not re.fullmatch(r"(?:api|ext)-[a-z0-9-]+-[0-9]+\.dll", requested):
        raise ValueError("Unknown API-set contract format")
    if not re.fullmatch(r"[a-z0-9_.+-]+", importer):
        raise ValueError("Invalid API-set importing basename")
    prefix = requested.removesuffix(".dll").rsplit("-", 1)[0]
    if not factor:
        raise ValueError("Invalid API-set hash factor")
    hashed = 0
    for character in prefix:
        hashed = (hashed * factor + ord(character)) & 0xFFFFFFFF
    matches = []
    previous = -1
    for index in range(count):
        value, entry_index = struct.unpack("<II", read(hashes + index * 8, 8))
        if value < previous or entry_index >= count:
            raise ValueError("Malformed API-set hash table")
        previous = value
        if value == hashed:
            matches.append(entry_index)
    if not matches:
        raise ValueError("Unknown API-set contract: " + requested)
    if len(matches) != 1:
        raise ValueError("Ambiguous API-set hash lookup refused")
    _, name, name_length, hashed_length, values, value_count = struct.unpack(
        "<IIIIII", read(entries + matches[0] * 24, 24)
    )
    contract = wide(name, name_length)
    if hashed_length != len(prefix) * 2 or contract[: len(prefix)] != prefix:
        raise ValueError("Unknown API-set hashed prefix: " + requested)
    if not 1 <= value_count <= 256:
        raise ValueError("Missing or malformed API-set provider list")
    default = None
    overrides = []
    for index in range(value_count):
        _, named, named_length, host, host_length = struct.unpack(
            "<IIIII", read(values + index * 20, 20)
        )
        selector = wide(named, named_length)
        provider = wide(host, host_length) if host and host_length else None
        if selector and not re.fullmatch(r"[a-z0-9_.+-]+", selector):
            raise ValueError("Invalid API-set importer selector")
        if provider and not re.fullmatch(r"[a-z0-9_.+-]+\.(dll|drv)", provider):
            raise ValueError("Invalid API-set provider name")
        if index == 0:
            default = provider
        elif selector == importer:
            overrides.append(provider)
    if len(overrides) > 1:
        raise ValueError("Ambiguous API-set importer override refused")
    selected = overrides[0] if overrides else default
    if not selected:
        raise ValueError("Missing API-set selected provider")
    reject_legacy_runtime([selected])
    return {
        "requested_contract": requested,
        "schema_contract": contract + ".dll",
        "hashed_prefix": prefix,
        "hash": hashed,
        "importer": importer,
        "selector": importer if overrides else "default",
        "provider": selected,
    }


def capture_wine_os_manifest(system_directory, output):
    """Bind actual Wine OS DLL bytes and API-set aliases for later admission."""
    directory = Path(system_directory).resolve(strict=True)
    if not directory.is_dir():
        raise ValueError("Wine system DLL directory required")
    providers = {
        path.name.lower(): {"path": str(path), "sha256": sha256(path)}
        for pattern in ("*.dll", "*.drv")
        for path in directory.glob(pattern)
        if path.is_file()
    }
    if not {"kernel32.dll", "ntdll.dll", "ucrtbase.dll"}.issubset(providers):
        raise ValueError("Incomplete Wine AMD64 OS provider directory")
    for entry in providers.values():
        validate_pe_amd64(Path(entry["path"]).read_bytes())
    manifest = {
        "state": "WINE_OS_PROVIDER_SNAPSHOT_ONLY",
        "directory": str(directory),
        "providers": providers,
        "aliases": {},
    }
    schema = directory / "apisetschema.dll"
    if schema.is_file():
        manifest["aliases"] = read_api_set_aliases(schema.read_bytes())
        manifest["alias_proof"] = {"path": str(schema), "sha256": sha256(schema)}
    Path(output).write_text(json.dumps(manifest, indent=2) + "\n")
    return manifest


def preflight(
    prefix,
    directory,
    sccache,
    rustc,
    expected_memory,
    expected_quota,
    server_port,
    wine_os_manifest,
    expected_cpuset,
):
    directory = Path(directory)
    directory.mkdir(parents=True, exist_ok=False)
    bindings = compiler_bindings(prefix, TARGET)
    cg = Path("/sys/fs/cgroup")
    limits = {
        name: (cg / name).read_text().strip()
        for name in (
            "memory.max",
            "memory.swap.max",
            "cpu.max",
            "cpuset.cpus.effective",
        )
    }
    if (
        limits["memory.max"] != str(expected_memory)
        or limits["cpu.max"] != f"{expected_quota} 100000"
    ):
        raise ValueError("Actual container limits differ from admitted budget")
    if limits["memory.swap.max"] != "0":
        raise ValueError("Extra swap refused")
    if limits["cpuset.cpus.effective"] != expected_cpuset:
        raise ValueError("Actual CPU affinity differs from admitted budget")
    env = {
        "PATH": "/home/runner/.cargo/bin:/usr/local/bin:/usr/bin:/bin",
        "HOME": "/home/runner",
        "LANG": "C.UTF-8",
        "SCCACHE_DIR": str(directory / "cache"),
        "SCCACHE_SERVER_PORT": str(server_port),
        "RUSTUP_HOME": "/home/runner/.rustup",
        **bindings,
    }

    def run(args, timeout=60):
        result = subprocess.run(
            args, capture_output=True, text=True, env=env, timeout=timeout, check=False
        )
        with (directory / "commands.jsonl").open("a", encoding="utf-8") as log:
            log.write(
                json.dumps(
                    {
                        "argv": args,
                        "exit_code": result.returncode,
                        "stdout": result.stdout,
                        "stderr": result.stderr,
                    }
                )
                + "\n"
            )
        if result.returncode:
            raise RuntimeError(
                f"Command failed ({result.returncode}): {args}: {result.stderr}"
            )
        return result.stdout

    started = time.monotonic()
    tools = {
        key: {"path": value, "sha256": sha256(value)} for key, value in bindings.items()
    }
    for name, path, expected in (
        ("rustc", rustc, RUST_VERSION),
        ("sccache", sccache, SCCACHE_VERSION),
    ):
        resolved = Path(path).resolve(strict=True)
        version = run([str(resolved), "--version"])
        if (
            not version.startswith(name + " " + expected + " ")
            and version.strip() != name + " " + expected
        ):
            raise ValueError(
                "Actual " + name + " version differs from the admitted tool"
            )
        if name == "rustc" and resolved.name == "rustup":
            raise ValueError(
                "Actual rustc binary required; Rustup proxy is insufficient"
            )
        tools[name] = {
            "path": path,
            "resolved_path": str(resolved),
            "sha256": sha256(resolved),
            "version": version,
        }
    for key in ("CC_x86_64_pc_windows_gnullvm", "CXX_x86_64_pc_windows_gnullvm"):
        version = run([bindings[key], "--version"])
        if "clang version " not in version:
            raise ValueError("Actual Clang version differs from admitted release")
        validate_llvm_version(version)
        tools[key]["version"] = version
    for name in ("llvm-ar", "ld.lld"):
        tool_path = admitted_tool(prefix, name)
        version = run([str(tool_path), "--version"])
        validate_llvm_version(version)
        tools[name] = {
            "path": str(tool_path),
            "resolved_path": str(tool_path.resolve(strict=True)),
            "sha256": sha256(tool_path),
            "version": version,
        }
    clang_path = admitted_tool(prefix, "clang")
    clang_version = run([str(clang_path), "--version"])
    validate_llvm_version(clang_version)
    tools["clang"] = {
        "path": str(clang_path),
        "resolved_path": str(clang_path.resolve(strict=True)),
        "sha256": sha256(clang_path),
        "version": clang_version,
    }
    sources = {
        "c": '#include <stdio.h>\nint main(void) { puts("C_UCRT_PASS"); return 0; }\n',
        "cpp": (
            "#include <cstdio>\n#include <vector>\n#include <stdexcept>\n"
            "int main() { try { std::vector<int> v{1,2,3}; "
            'if(v.at(1)!=2) return 1; throw std::runtime_error("probe"); '
            "} catch(const std::runtime_error&) { "
            'std::puts("CXX_UCRT_PASS"); return 0; } }\n'
        ),
        "rs": (
            'fn main() { let value=std::panic::catch_unwind(|| panic!("probe")); '
            'assert!(value.is_err()); println!("RUST_UCRT_PASS"); }\n'
        ),
    }
    artifacts = {}
    require_unused_port(server_port)
    run([sccache, "--start-server"])
    before = json.loads(run([sccache, "--show-stats", "--stats-format=json"]))
    validate_private_cache(before, directory / "cache")
    for language, text in sources.items():
        source = directory / ("preflight." + language)
        source.write_text(text)
        if language == "rs":
            lib = directory / "cache.rs"
            lib.write_text("pub fn llvm_cache_proof() -> u64 { 42 }\n")
            output = directory / "libllvm_cache_proof.rlib"
            args = [
                rustc,
                "--target",
                TARGET,
                "--crate-name",
                "llvm_cache_proof",
                "--edition=2024",
                "--crate-type=rlib",
                "--emit=link",
                str(lib),
                "--out-dir",
                str(directory),
            ]
        else:
            output = directory / (language + ".o")
            compiler = bindings[
                "CC_x86_64_pc_windows_gnullvm"
                if language == "c"
                else "CXX_x86_64_pc_windows_gnullvm"
            ]
            args = [compiler, "-c", str(source), "-o", str(output)]
        group_before = json.loads(run([sccache, "--show-stats", "--stats-format=json"]))
        validate_private_cache(group_before, directory / "cache")
        run([sccache, *args])
        first = sha256(output)
        output.unlink()
        run([sccache, *args])
        if sha256(output) != first:
            raise ValueError("Cached object differs from original compile")
        group_after = json.loads(run([sccache, "--show-stats", "--stats-format=json"]))
        validate_private_cache(group_after, directory / "cache")
        validate_language_cache(group_before, group_after, language)
        executable = directory / (language + ".exe")
        if language == "rs":
            run(
                [
                    rustc,
                    "--target",
                    TARGET,
                    "--edition=2024",
                    str(source),
                    "-C",
                    "linker="
                    + bindings["CARGO_TARGET_X86_64_PC_WINDOWS_GNULLVM_LINKER"],
                    "-o",
                    str(executable),
                ]
            )
        else:
            run([compiler, str(output), "-o", str(executable)])
        imports = pe_imports(executable.read_bytes())
        validate_ucrt(imports)
        artifacts[language] = {
            "path": str(executable),
            "sha256": sha256(executable),
            "imports": imports,
            "cached_object_sha256": first,
            "cache_before": group_before,
            "cache_after": group_after,
        }
    after = json.loads(run([sccache, "--show-stats", "--stats-format=json"]))
    validate_private_cache(after, directory / "cache")
    hits = after["stats"]["cache_hits"]["counts"]
    old_hits = before["stats"]["cache_hits"]["counts"]
    if sum(hits.values()) - sum(old_hits.values()) < 3 or hits.get(
        "Rust", 0
    ) <= old_hits.get("Rust", 0):
        raise ValueError("Actual C/C++/Rust cache reuse not demonstrated")
    for key in (
        "cache_timeouts",
        "cache_read_errors",
        "cache_write_errors",
        "compile_fails",
        "requests_unsupported_compiler",
        "requests_not_cacheable",
    ):
        if after["stats"][key] != before["stats"][key]:
            raise ValueError("Cache preflight increased " + key)
    if after["stats"]["cache_errors"] != before["stats"]["cache_errors"]:
        raise ValueError("Cache preflight increased backend errors")
    # Only this invocation started the daemon after an unused-port check; prove
    # its private cache/version again before terminating it.
    validate_private_cache(
        json.loads(run([sccache, "--show-stats", "--stats-format=json"])),
        directory / "cache",
    )
    run([sccache, "--stop-server"])
    os_manifest = json.loads(Path(wine_os_manifest).read_text())
    runtime_dlls, os_dlls = collect_runtime_closure(
        artifacts, prefix, directory, os_manifest
    )
    env.update(
        WINEPREFIX=str(directory / "wine"),
        WINEDEBUG="-all",
        XDG_RUNTIME_DIR=str(directory / "xdg"),
    )
    Path(env["XDG_RUNTIME_DIR"]).mkdir(mode=0o700)
    for language, marker in (
        ("c", "C_UCRT_PASS"),
        ("cpp", "CXX_UCRT_PASS"),
        ("rs", "RUST_UCRT_PASS"),
    ):
        if run(["wine", artifacts[language]["path"]]).strip() != marker:
            raise ValueError("Actual Wine runtime marker differs")
    receipt = {
        "state": "LLVM_UCRT_PREFLIGHT_PASS_ONLY",
        "target": TARGET,
        "crt": "ucrt",
        "compiler": "LLVM-MinGW",
        "version": LLVM_VERSION,
        "bindings": bindings,
        "tools": tools,
        "limits": limits,
        "artifacts": artifacts,
        "before": before,
        "after": after,
        "runtime_dlls": runtime_dlls,
        "wine_os_providers": os_dlls,
        "wine_os_manifest_sha256": sha256(wine_os_manifest),
        "private_cache_server_port": server_port,
        "wall_seconds": time.monotonic() - started,
        "full_repo_qualification": "NOT_RUN",
    }
    (directory / "receipt.json").write_text(json.dumps(receipt, indent=2) + "\n")
    print(json.dumps(receipt))


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--llvm-prefix", required=True)
    parser.add_argument("--directory", required=True)
    parser.add_argument("--sccache", required=True)
    parser.add_argument("--rustc", required=True)
    parser.add_argument("--expected-memory", type=int, default=4294967296)
    parser.add_argument("--expected-quota", type=int, default=200000)
    parser.add_argument("--expected-cpuset", default="8-9")
    parser.add_argument("--server-port", type=int, required=True)
    parser.add_argument("--wine-os-manifest", required=True)
    args = parser.parse_args()
    preflight(
        args.llvm_prefix,
        args.directory,
        args.sccache,
        args.rustc,
        args.expected_memory,
        args.expected_quota,
        args.server_port,
        args.wine_os_manifest,
        args.expected_cpuset,
    )
