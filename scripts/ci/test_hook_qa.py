"""Service-free hostile-input and lifecycle-boundary regressions; no Cargo/Docker."""

import copy
import hashlib
import importlib.util
import io
import json
import os
import subprocess
import tarfile
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

SPEC = importlib.util.spec_from_file_location(
    "hook_qa", Path(__file__).with_name("run-hook-qa.py")
)
QA = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(QA)


def archive(entries):
    output = io.BytesIO()
    with tarfile.open(fileobj=output, mode="w") as stream:
        for name, data, kind in entries:
            entry = tarfile.TarInfo(name)
            entry.type = kind
            if kind == tarfile.REGTYPE:
                entry.size = len(data)
            else:
                entry.linkname = "../../outside"
            stream.addfile(entry, io.BytesIO(data) if kind == tarfile.REGTYPE else None)
    return output.getvalue()


def blob(data):
    return hashlib.sha1(b"blob " + str(len(data)).encode() + b"\0" + data).hexdigest()


def valid_envelope(mounts, image, label):
    return {
        "Image": image,
        "Config": {
            "Image": image,
            "User": "1000:1000",
            "Labels": {"agent-bus.hook-run": label},
        },
        "HostConfig": {
            "ReadonlyRootfs": True,
            "Memory": 4294967296,
            "MemorySwap": 4294967296,
            "NanoCpus": 4000000000,
            "PidsLimit": 512,
            "NetworkMode": "none",
            "Privileged": False,
            "CapAdd": None,
            "CapDrop": ["ALL"],
            "Devices": [],
            "DeviceRequests": None,
            "SecurityOpt": ["no-new-privileges"],
            "Tmpfs": {
                "/tmp": "rw,exec,nosuid,nodev,size=268435456,uid=1000,gid=1000,mode=700"
            },
            "Mounts": [
                {
                    "Type": "bind",
                    "Source": str(source),
                    "Target": target,
                    "ReadOnly": readonly,
                }
                for source, target, readonly in mounts
            ],
        },
        "Mounts": [
            {
                "Type": "bind",
                "Source": str(source),
                "Destination": target,
                "RW": not readonly,
            }
            for source, target, readonly in mounts
        ],
        "State": {"Running": False, "Pid": 0, "ExitCode": 0, "OOMKilled": False},
    }


class HookGuards(unittest.TestCase):
    def test_handoff_requires_existing_regular_file_and_preserves_identity(self):
        with tempfile.TemporaryDirectory() as directory:
            target = Path(directory) / "handoff"
            with self.assertRaises(FileNotFoundError):
                QA.existing_handoff(target)
            self.assertFalse(target.exists())
            target.write_bytes(b"old bytes")
            before = target.stat()
            with QA.existing_handoff(target) as stream:
                stream.write(b"new")
            after = target.stat()
            self.assertEqual(
                (before.st_dev, before.st_ino), (after.st_dev, after.st_ino)
            )
            self.assertEqual(target.read_bytes(), b"new")
            with self.assertRaises(RuntimeError):
                QA.existing_handoff(Path(directory))

    def test_handoff_names_cover_exact_actor_commands(self):
        for phase, count in (("pre-commit", 6), ("pre-push", 3), ("audit", 4)):
            names = QA.handoff_names(phase)
            self.assertEqual(len(names), count * 2 + 1)
            self.assertEqual(len(names), len(set(names)))

    def test_all_actual_outgoing_tips_and_deletion(self):
        a, b, zero = "a" * 40, "b" * 40, "0" * 40
        text = (
            f"refs/heads/a {a} refs/heads/a {zero}\n"
            f"refs/heads/b {b} refs/heads/b {a}\n"
            f"HEAD {a} refs/heads/c {zero}\n"
            f"(delete) {zero} refs/heads/old {a}\n"
        )
        self.assertEqual(QA.outgoing_commits(text), [a, b])

    def test_missing_or_malformed_refs_do_not_fall_back_to_head(self):
        for text in ("", "\n", "HEAD abc refs/heads/x abc", "a " + "a" * 40):
            with self.subTest(text=text), self.assertRaises(RuntimeError):
                QA.outgoing_commits(text)

    def test_snapshot_extracts_exact_blob(self):
        data = b"line\r\nexact\x00bytes"
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            QA.extract_snapshot(
                archive([("dir/file", data, tarfile.REGTYPE)]),
                root,
                {"dir/file": blob(data)},
            )
            self.assertEqual((root / "dir/file").read_bytes(), data)

    def test_archive_cannot_escape_snapshot(self):
        for name in ("../escaped", "/absolute", "C:/drive", "dir\\escape"):
            with tempfile.TemporaryDirectory() as directory:
                root = Path(directory) / "source"
                root.mkdir()
                with self.assertRaises(RuntimeError):
                    QA.extract_snapshot(
                        archive([(name, b"bad", tarfile.REGTYPE)]),
                        root,
                        {name: blob(b"bad")},
                    )
                self.assertFalse((root.parent / "escaped").exists())

    def test_links_duplicates_substitution_and_omission_fail_closed(self):
        cases = [
            ([("a", b"", tarfile.SYMTYPE)], {"a": blob(b"")}),
            ([("a", b"x", tarfile.REGTYPE)] * 2, {"a": blob(b"x")}),
            ([("a", b"changed", tarfile.REGTYPE)], {"a": blob(b"original")}),
            ([], {"export-ignored": blob(b"x")}),
        ]
        for entries, expected in cases:
            with (
                tempfile.TemporaryDirectory() as directory,
                self.assertRaises(RuntimeError),
            ):
                QA.extract_snapshot(archive(entries), Path(directory), expected)

    def test_host_credentials_and_live_bus_environment_are_not_forwarded(self):
        env = QA.clean_host_environment(
            {
                "PATH": "tools",
                "SystemRoot": "windows",
                "AGENT_BUS_AUTH_TOKEN": "secret",
                "HOME": "live",
                "HTTP_PROXY": "secret",
                "DOCKER_HOST": "tcp://foreign",
                "GIT_CONFIG_COUNT": "1",
            }
        )
        self.assertEqual(
            env,
            {"PATH": "tools", "SystemRoot": "windows", "GIT_NO_REPLACE_OBJECTS": "1"},
        )

    def test_config_refuses_mutable_tools_credentials_and_remote_daemon(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            config = {
                "image": "sha256:" + "b" * 64,
                "endpoint": "unix:///owned/docker.sock",
                "timeout_seconds": 120,
                "jobs": 2,
                "tool_files_sha256": {},
            }
            for key in ("cargo_home", "target_dir", "tools_dir", "evidence_root"):
                path = root / key
                path.mkdir()
                config[key] = str(path)
            QA.validate_config(config)
            (root / "tools_dir" / "cargo-clippy").write_bytes(b"changed")
            with self.assertRaises(RuntimeError):
                QA.validate_config(config)
            (root / "tools_dir" / "cargo-clippy").unlink()
            (root / "cargo_home" / "credentials.toml").write_text("secret")
            with self.assertRaises(RuntimeError):
                QA.validate_config(config)
            (root / "cargo_home" / "credentials.toml").unlink()
            config["endpoint"] = "tcp://remote:2375"
            with self.assertRaises(RuntimeError):
                QA.validate_config(config)

    def run_fake_container(
        self, *, exit_code=0, deadline=False, settle=True, phase="pre-commit"
    ):
        """Exercise the actual controller with a deterministic owned-engine fixture."""
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            source = root / "source"
            source.mkdir()
            run = root / "run"
            run.mkdir()
            script = root / "controller.py"
            script.write_text("# fixture")
            config = {
                "endpoint": "unix:///owned.sock",
                "image": "sha256:" + "d" * 64,
                "timeout_seconds": 30,
                "jobs": 2,
                "tools_dir": str(root / "tools"),
                "cargo_home": str(root / "cargo"),
                "target_dir": str(root / "target"),
            }
            for key in ("tools_dir", "cargo_home", "target_dir"):
                Path(config[key]).mkdir()
            cid, calls, label = "a" * 64, [], None
            metadata = None
            process = type("Process", (), {"pid": 101, "returncode": None})()
            process.poll = lambda: process.returncode
            communications = []

            def communicate(timeout):
                communications.append(timeout)
                if deadline and len(communications) == 1:
                    raise subprocess.TimeoutExpired("owned-cli", timeout)
                if not settle:
                    raise subprocess.TimeoutExpired("owned-cli", timeout)
                process.returncode = exit_code
                (run / phase / "actor-receipt.json").write_text(
                    json.dumps({"state": "PASS"})
                )
                return b"stdout EOF", b"stderr EOF"

            process.communicate = communicate

            def control(command, **kwargs):
                nonlocal label, metadata
                # Strip the explicit executable/endpoint/config prefix.
                args = command[5:]
                calls.append(args)
                if args[0] == "info":
                    return b"linux\n"
                if args[:2] == ["image", "inspect"]:
                    return json.dumps([{"Id": config["image"]}]).encode()
                if args[0] == "create":
                    Path(args[args.index("--cidfile") + 1]).write_text(cid)
                    label = args[args.index("--label") + 1].split("=", 1)[1]
                    mounts = []
                    for position, part in enumerate(args):
                        if part == "--mount":
                            fields = args[position + 1].split(",")
                            source = next(f[4:] for f in fields if f.startswith("src="))
                            target = next(f[4:] for f in fields if f.startswith("dst="))
                            mounts.append((source, target, "readonly" in fields))
                    metadata = valid_envelope(mounts, config["image"], label)
                    metadata["HostConfig"]["NetworkMode"] = (
                        "bridge" if phase == "audit" else "none"
                    )
                    metadata["State"]["ExitCode"] = exit_code
                    return cid.encode()
                if args[0] == "inspect":
                    return json.dumps([metadata]).encode()
                return b""

            with (
                patch.object(QA.shutil, "which", return_value="owned-docker"),
                patch.object(QA.subprocess, "check_output", side_effect=control),
                patch.object(QA.subprocess, "Popen", return_value=process),
            ):
                result = QA.execute_container(config, source, run, phase, script)
            receipt = json.loads((run / phase / "controller-receipt.json").read_text())
            return result, receipt, calls

    def test_real_controller_removes_only_captured_id_after_child_eof(self):
        result, receipt, calls = self.run_fake_container()
        self.assertEqual(result, 0)
        self.assertTrue(receipt["original_cli_both_eof"])
        self.assertEqual(receipt["original_cli_exit"], 0)
        self.assertIn(["rm", "a" * 64], calls)
        self.assertFalse(any("--force" in call or "prune" in call for call in calls))

    def test_real_controller_preserves_phase_failure_and_deadline(self):
        for options, failure in (
            ({"exit_code": 101}, "Actual QA failed"),
            ({"deadline": True}, "QA deadline exceeded"),
        ):
            with self.subTest(options=options):
                result, receipt, calls = self.run_fake_container(**options)
                self.assertEqual(result, 1)
                self.assertEqual(receipt["first_failure"], failure)
                self.assertIn(["rm", "a" * 64], calls)

    def test_unsettled_original_handle_blocks_container_removal(self):
        result, receipt, calls = self.run_fake_container(settle=False)
        self.assertEqual(result, 1)
        self.assertTrue(receipt["cleanup_errors"])
        self.assertNotIn(["rm", "a" * 64], calls)

    def audit_actor_result(
        self,
        tool_version_code,
        audit_code,
        sysroot="/usr/local/rustup/toolchains/1.99.0-aarch64-unknown-linux-gnu",
    ):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            source, output = root / "source", root / "work"
            source.mkdir()
            output.mkdir()
            for name in QA.handoff_names("audit"):
                (output / name).touch(exist_ok=False)
            (source / "rust-toolchain.toml").write_text(
                '[toolchain]\nchannel="1.99.0"\n'
            )
            real_path = Path

            def paths(value):
                return {"/source": source, "/work": output}.get(value, real_path(value))

            def check_output(command, **kwargs):
                if command == ["rustc", "--print", "sysroot"]:
                    return (sysroot + "\n").encode()
                if command[:3] == ["git", "-C", "/work/advisory-db"]:
                    return ("b" * 40 + "\n").encode()
                raise AssertionError(f"Unexpected check_output: {command}")

            def tool(command, **kwargs):
                if command[:2] == ["cargo", "audit"]:
                    code = tool_version_code if "--version" in command else audit_code
                else:
                    code = 0
                    kwargs["stdout"].write(
                        (command[0] + " 1.99.0 (fixture)\n").encode()
                    )
                return subprocess.CompletedProcess(command, code)

            with (
                patch.object(QA, "Path", side_effect=paths),
                patch.object(QA.subprocess, "check_output", side_effect=check_output),
                patch.object(QA.subprocess, "run", side_effect=tool),
            ):
                result = QA.actor({"phase": "audit", "jobs": 2})
            return result, json.loads((output / "actor-receipt.json").read_text())

    def test_actor_derives_arm64_and_x86_64_sysroot_from_admitted_compiler(self):
        for target in ("aarch64-unknown-linux-gnu", "x86_64-unknown-linux-gnu"):
            sysroot = f"/usr/local/rustup/toolchains/1.99.0-{target}"
            with patch.object(
                QA.subprocess, "check_output", return_value=(sysroot + "\n").encode()
            ):
                self.assertEqual(QA.resolve_sysroot("1.99.0", {}, Path(".")), sysroot)
        for bad in (
            "/usr/local/rustup/toolchains/1.98.0-aarch64-unknown-linux-gnu",
            "/usr/local/rustup/toolchains/1.99.0-",
            "/usr/local/rustup/toolchains/1.99.0-aarch64/extra",
            "/custom/sysroot",
        ):
            with (
                self.subTest(bad=bad),
                patch.object(
                    QA.subprocess, "check_output", return_value=(bad + "\n").encode()
                ),
                self.assertRaises(RuntimeError),
            ):
                QA.resolve_sysroot("1.99.0", {}, Path("."))

    def test_actor_and_envelope_map_invoking_posix_user_and_refuse_root(self):
        image, label = "sha256:" + "b" * 64, "owned"
        mounts = [("/owned/source", "/source", True), ("/owned/work", "/work", False)]
        envelope = valid_envelope(mounts, image, label)
        envelope["Config"]["User"] = "2002:2003"
        envelope["HostConfig"]["Tmpfs"] = {
            "/tmp": "rw,exec,nosuid,nodev,size=268435456,uid=2002,gid=2003,mode=700"
        }
        QA.validate_envelope(envelope, image, label, "pre-commit", mounts, 2002, 2003)
        with self.assertRaises(RuntimeError):
            QA.validate_envelope(
                envelope, image, label, "pre-commit", mounts, 1000, 1000
            )
        with self.assertRaises(RuntimeError):
            QA.validate_envelope(envelope, image, label, "pre-commit", mounts, 0, 2003)
        with (
            patch.object(QA.os, "getuid", create=True, return_value=2002),
            patch.object(QA.os, "getgid", create=True, return_value=2003),
        ):
            self.assertEqual(QA.actor_identity(), (2002, 2003))
            with self.assertRaises(RuntimeError):
                QA.actor({"phase": "pre-commit", "jobs": 2, "uid": 1000, "gid": 1000})
        with (
            patch.object(QA.os, "getuid", create=True, return_value=0),
            patch.object(QA.os, "getgid", create=True, return_value=0),
            self.assertRaises(RuntimeError),
        ):
            QA.actor_identity()

    def test_missing_container_audit_tool_fails_before_audit_gate(self):
        result, receipt = self.audit_actor_result(127, 0)
        self.assertEqual(result, 127)
        self.assertEqual(receipt["state"], "FAIL")
        self.assertEqual(receipt["commands"], [])

    def test_actual_container_audit_failure_code_is_preserved(self):
        result, receipt = self.audit_actor_result(0, 23)
        self.assertEqual(result, 23)
        self.assertEqual(receipt["state"], "FAIL")
        self.assertEqual(receipt["commands"][0]["exit_code"], 23)

    def test_actual_envelope_tampering_is_refused(self):
        image, label = "sha256:" + "b" * 64, "owned"
        mounts = [("/owned/source", "/source", True), ("/owned/work", "/work", False)]
        good = valid_envelope(mounts, image, label)
        QA.validate_envelope(good, image, label, "pre-commit", mounts)
        changes = [
            lambda m: m["HostConfig"].update(Memory=0),
            lambda m: m["HostConfig"].update(MemorySwap=-1),
            lambda m: m["HostConfig"].update(NanoCpus=0),
            lambda m: m["HostConfig"].update(PidsLimit=0),
            lambda m: m["HostConfig"].update(NetworkMode="host"),
            lambda m: m["HostConfig"].update(ReadonlyRootfs=False),
            lambda m: m["HostConfig"].update(Privileged=True),
            lambda m: m["HostConfig"].update(CapAdd=["SYS_ADMIN"]),
            lambda m: m["HostConfig"].update(DeviceRequests=[{"Count": -1}]),
            lambda m: m["HostConfig"].update(Tmpfs={}),
            lambda m: m["Config"].update(User="0:0"),
            lambda m: m.update(Image="sha256:" + "a" * 64),
            lambda m: m["Mounts"][0].update(RW=True),
            lambda m: m["Mounts"][0].update(Source="/foreign/source"),
            lambda m: m["HostConfig"]["Mounts"].append(
                {"Type": "bind", "Source": "/host/socket", "Target": "/socket"}
            ),
        ]
        for number, change in enumerate(changes):
            bad = copy.deepcopy(good)
            change(bad)
            with self.subTest(number=number), self.assertRaises(RuntimeError):
                QA.validate_envelope(bad, image, label, "pre-commit", mounts)

    def test_networked_audit_has_no_compilation_cache_bind(self):
        result, receipt, calls = self.run_fake_container(phase="audit")
        self.assertEqual(result, 0)
        self.assertTrue(receipt["actual_envelope_verified"])
        create = next(call for call in calls if call[0] == "create")
        mounts = [create[n + 1] for n, value in enumerate(create) if value == "--mount"]
        self.assertEqual(len(mounts), 5)
        self.assertFalse(any("/output/focused-core" in mount for mount in mounts))

    def test_raw_batch_rejects_wrong_order_hash_type_count_and_truncation(self):
        first, second = b"first\n", b"second\x00bytes\n"
        a, b = blob(first), blob(second)
        entries = [("a.ps1", "100644", a), ("b.bin", "100755", b)]
        one = a.encode() + b" blob " + str(len(first)).encode() + b"\n" + first + b"\n"
        two = (
            b.encode() + b" blob " + str(len(second)).encode() + b"\n" + second + b"\n"
        )
        parsed = QA.parse_blob_batch(one + two, entries)
        self.assertEqual(
            parsed, [("a.ps1", "100644", first), ("b.bin", "100755", second)]
        )
        for output in (
            two + one,
            one,
            one + two + b"extra",
            (one + two)[:-1],
            (one + two).replace(b"first", b"FIRST"),
            (one + two).replace(b" blob ", b" tree ", 1),
        ):
            with self.subTest(output=output), self.assertRaises(RuntimeError):
                QA.parse_blob_batch(output, entries)

    def test_indexed_paths_refuse_modes_aliases_and_windows_case_collisions(self):
        oid = "a" * 40

        def record(path, mode="100644", kind="blob"):
            return f"{mode} {kind} {oid}\t{path}\0".encode()

        for raw in (
            record("a", "120000"),
            record("submodule", "160000", "commit"),
            record("Dir/a") + record("dir/b"),
            record("A") + record("a"),
            record("space. "),
            record("CON.txt"),
            record("$NULL"),
            record("a|b"),
            record("../escape"),
        ):
            with self.subTest(raw=raw), self.assertRaises(RuntimeError):
                QA.tree_entries(raw)

    def test_actual_git_crlf_attributes_export_exact_indexed_lf_bytes(self):
        """Real Git index fixture: no commit, hook change, Docker or Cargo."""
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            repo, destination = root / "repository", root / "export"
            destination.mkdir()
            env = QA.clean_host_environment(os.environ)
            env.pop("GIT_INDEX_FILE", None)

            def git(*args):
                return subprocess.check_output(
                    ["git", "-C", str(repo), *args],
                    env=env,
                    stderr=subprocess.PIPE,
                    timeout=20,
                )

            subprocess.run(
                ["git", "init", "--quiet", str(repo)],
                env=env,
                check=True,
                capture_output=True,
                timeout=20,
            )
            git("config", "core.autocrlf", "false")
            (repo / ".gitattributes").write_bytes(b"*.ps1 text eol=crlf\n")
            original = b"Write-Output 'raw indexed bytes'\r\n# second line\r\n"
            (repo / "fixture.ps1").write_bytes(original)
            git("add", ".gitattributes", "fixture.ps1")
            tree = git("write-tree").decode().strip()
            with patch.dict(os.environ, env, clear=True):
                manifest = QA.snapshot(repo, tree, destination)
            expected = original.replace(b"\r\n", b"\n")
            self.assertEqual((destination / "fixture.ps1").read_bytes(), expected)
            self.assertEqual((repo / "fixture.ps1").read_bytes(), original)
            self.assertEqual(
                manifest["fixture.ps1"], hashlib.sha256(expected).hexdigest()
            )
            receipt = json.loads((root / "git-blob-export.json").read_text())
            self.assertEqual(receipt["state"], "PASS")
            self.assertEqual(receipt["requested_blobs"], 2)
            self.assertTrue(receipt["both_eof"] and receipt["original_child_settled"])


if __name__ == "__main__":
    unittest.main()
