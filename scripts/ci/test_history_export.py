"""Regression tests for the agent-hub history export job (packaging/history-export).

Protects: the exported bundle's integrity contract (content-addressed name,
SHA256SUMS over every file, byte-deterministic parts), the push-then-readback
proof, the never-overwrite rule, the snapshot positive control, and the
install and unit wiring that keeps the timer alerting and on the right PATH.
Needs: python3 standard library and bash on Linux. A fake `sftp` written by
the tests stands in for the drop, enforcing its no-overwrite rule; no network,
PostgreSQL, Redis or Windows host is used.
Breadcrumb: packaging/history-export/README.md "Test"; CI's Unit Tests step
runs `python3 -B -m unittest discover -s scripts/ci -p "test_*.py"`.
"""

import gzip
import hashlib
import importlib.util
import json
import os
import re
import shutil
import subprocess
import sys
import tempfile
import textwrap
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
PKG = ROOT / "packaging" / "history-export"
SCRIPT = PKG / "agent-hub-history-export.py"


def load_module():
    spec = importlib.util.spec_from_file_location("agent_hub_history_export", SCRIPT)
    mod = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(mod)
    return mod


hx = load_module()

# Behaves like the drop: mkdir of an existing name fails, put never overwrites,
# and like `sftp -b` the batch stops with exit 1 at the first failing command.
FAKE_SFTP = textwrap.dedent("""\
    #!/usr/bin/env python3
    import os, shlex, shutil, sys
    from pathlib import Path
    args = sys.argv[1:]
    batch = Path(args[args.index("-b") + 1]).read_text()
    root = Path(os.environ["FAKE_SFTP_ROOT"]) / args[-1].split("@", 1)[1]
    root.mkdir(parents=True, exist_ok=True)
    for line in batch.splitlines():
        cmd = shlex.split(line)
        if not cmd:
            continue
        if cmd[0] == "mkdir":
            p = root / cmd[1]
            if p.exists():
                sys.exit(f"remote mkdir {cmd[1]}: Failure")
            p.mkdir()
        elif cmd[0] == "put":
            dst = root / cmd[2]
            if dst.exists():
                sys.exit(f'dest open "{cmd[2]}": Permission denied')
            shutil.copyfile(cmd[1], dst)
        elif cmd[0] == "get" and cmd[1] == "-r":
            out = Path(cmd[3]) / Path(cmd[2]).name
            shutil.copytree(root / cmd[2], out)
            if os.environ.get("FAKE_SFTP_CORRUPT"):
                victim = sorted(out.rglob("*.gz"))[0]
                data = bytearray(victim.read_bytes())
                data[-1] ^= 0xFF
                victim.write_bytes(bytes(data))
            if os.environ.get("FAKE_SFTP_EXTRA"):
                (out / "stray.txt").write_text("not in SHA256SUMS")
        else:
            sys.exit(f"fake sftp: unsupported {line}")
""")


def make_raw(raw: Path, n_messages: int = 5, body: str = "hello") -> None:
    """Write the raw export files build_bundle expects, as the exporters would."""
    raw.mkdir(parents=True)
    with open(raw / "messages.ndjson", "w") as f:
        f.writelines(
            json.dumps({"id": f"m{i}", "body": f"{body}-{i}"}) + "\n"
            for i in range(n_messages)
        )
    (raw / "redis-stream-messages.ndjson").write_text(
        json.dumps({"stream_id": "1-0", "fields": {"id": "m0"}}) + "\n"
    )
    (raw / "presence-events.ndjson").write_text(
        json.dumps({"id": 1, "agent": "a"}) + "\n"
    )
    (raw / "redis-side-channels.ndjson").write_text("")
    (raw / "redis-side-channels.counts.json").write_text("{}\n")
    (raw / "reconciliation.json").write_text(
        json.dumps({"uuid_union": n_messages}) + "\n"
    )
    (raw / "snapshot.tsv").write_text(
        f"snapshot_utc|2026-10-29T00:00:00Z\nmessages|{n_messages}\n"
    )


class TempDirCase(unittest.TestCase):
    def setUp(self):
        self.tmp = Path(tempfile.mkdtemp(prefix="history-export-test-"))
        self.addCleanup(shutil.rmtree, self.tmp, True)


class DecTests(unittest.TestCase):
    def test_dec_keeps_text_and_wraps_binary(self):
        """Protects: Redis values reaching the bundle as valid JSON.
        Detects: a non-UTF-8 value (LZ4 bodies, binary keys) crashing json.dumps or
        being decoded lossily instead of carried as base64.
        Needs: nothing. Breadcrumb: export_redis's dec() calls."""
        self.assertEqual(hx.dec(b"plain"), "plain")
        self.assertEqual(hx.dec(b"\xff\x00"), {"_b64": "/wA="})
        self.assertEqual(hx.dec({b"k": [b"v", 2]}), {"k": ["v", 2]})


class GzPartsTests(TempDirCase):
    def test_parts_split_and_bytes_are_deterministic(self):
        """Protects: the content-addressed bundle name, which needs identical bytes for
        identical rows. Detects: a gzip header carrying a timestamp (mtime) or a
        wrong split at the part boundary.
        Needs: a temp dir. Breadcrumb: gz_parts, PARTS."""
        src = self.tmp / "rows.ndjson"
        src.write_bytes(b"".join(f"row-{i}\n".encode() for i in range(5)))
        self.assertEqual(hx.gz_parts(src, self.tmp / "a", 2), 5)
        self.assertEqual(hx.gz_parts(src, self.tmp / "b", 2), 5)
        parts = sorted(p.name for p in (self.tmp / "a").iterdir())
        self.assertEqual(
            parts, ["part-000.ndjson.gz", "part-001.ndjson.gz", "part-002.ndjson.gz"]
        )
        for name in parts:
            data = (self.tmp / "a" / name).read_bytes()
            self.assertEqual(data, (self.tmp / "b" / name).read_bytes())
            # Two writes in one second match even with a clock mtime, so check the
            # header field itself.
            self.assertEqual(
                data[4:8], b"\0\0\0\0", f"{name} gzip header carries an mtime"
            )
        joined = b"".join(
            gzip.decompress((self.tmp / "a" / n).read_bytes()) for n in parts
        )
        self.assertEqual(joined, src.read_bytes())


class BundleTests(TempDirCase):
    def build(self, sub: str, **kw) -> tuple[str, Path]:
        raw, bundle = self.tmp / sub / "raw", self.tmp / sub / "bundle"
        make_raw(raw, **kw)
        return hx.build_bundle(raw, bundle, {"artifact": "test"}), bundle

    def test_name_is_sha_prefix_of_sha256sums_and_covers_every_file(self):
        """Protects: the bundle naming and checksum contract the drop and the runbook
        rely on. Detects: a file left out of SHA256SUMS, a stale MANIFEST hash, or a
        name not derived from SHA256SUMS.
        Needs: a temp dir. Breadcrumb: build_bundle, verify_tree."""
        name, bundle = self.build("one")
        sums = bundle / "SHA256SUMS"
        self.assertEqual(
            name, "history-" + hashlib.sha256(sums.read_bytes()).hexdigest()[:16]
        )
        listed = {line.split("  ", 1)[1] for line in sums.read_text().splitlines()}
        on_disk = {
            p.relative_to(bundle).as_posix() for p in bundle.rglob("*") if p.is_file()
        }
        self.assertEqual(listed, on_disk - {"SHA256SUMS"})
        self.assertEqual(hx.verify_tree(bundle), [])
        manifest = json.loads((bundle / "MANIFEST.json").read_text())
        self.assertEqual(manifest["line_counts"]["messages-pg"], 5)

    def test_same_rows_same_name_changed_row_new_name(self):
        """Protects: the never-overwrite rule, which is safe only if a name maps to one
        content. Detects: non-deterministic output (same rows, different name) or a
        change that keeps the old name and so collides with the old bundle.
        Needs: a temp dir. Breadcrumb: build_bundle."""
        a, _ = self.build("a")
        b, _ = self.build("b")
        c, _ = self.build("c", body="changed")
        self.assertEqual(a, b)
        self.assertNotEqual(a, c)

    def test_verify_tree_reports_a_changed_file(self):
        """Protects: the readback proof. Detects: verify_tree passing a bundle whose
        bytes differ from SHA256SUMS (a check that cannot fail).
        Needs: a temp dir. Breadcrumb: verify_tree, push_and_verify."""
        _, bundle = self.build("v")
        victim = bundle / "reconciliation.json"
        victim.write_text(victim.read_text() + " ")
        self.assertEqual(hx.verify_tree(bundle), ["reconciliation.json"])


class WindowTests(unittest.TestCase):
    def test_snapshot_outside_health_window_stops(self):
        """Protects: the positive control that the export read the live hub.
        Detects: a snapshot count below the pre-export health count or above the
        post-export count (a different, empty or truncated database) being accepted.
        Needs: nothing. Breadcrumb: check_window in main()."""
        hx.check_window(10, 10, 12)
        hx.check_window(10, 12, 12)
        for snapshot in (9, 13, 0):
            with self.assertRaises(SystemExit):
                hx.check_window(10, snapshot, 12)


# The fake sftp is a shebang script, which Windows cannot exec; the job is Linux-only.
@unittest.skipUnless(
    os.name == "posix", "fake sftp needs a POSIX exec of a shebang script"
)
class PushTests(TempDirCase):
    def setUp(self):
        super().setUp()
        fake = self.tmp / "bin" / "sftp"
        fake.parent.mkdir()
        fake.write_text(FAKE_SFTP)
        fake.chmod(0o755)
        self.remote = self.tmp / "remote"
        old_sftp, old_env = hx.SFTP, dict(os.environ)
        hx.SFTP = str(fake)
        os.environ["FAKE_SFTP_ROOT"] = str(self.remote)
        os.environ.pop("FAKE_SFTP_CORRUPT", None)
        os.environ.pop("FAKE_SFTP_EXTRA", None)
        self.addCleanup(setattr, hx, "SFTP", old_sftp)
        self.addCleanup(lambda: (os.environ.clear(), os.environ.update(old_env)))
        raw, self.bundle = self.tmp / "raw", self.tmp / "bundle"
        make_raw(raw)
        self.name = hx.build_bundle(raw, self.bundle, {"artifact": "test"})
        self.work = self.tmp / "work"
        self.work.mkdir()

    def push(self) -> dict:
        return hx.push_and_verify(
            "drop-host", self.bundle, self.name, self.work, Path("/dev/null"), "u"
        )

    def test_round_trip_verifies_every_file(self):
        """Protects: the proof that a destination holds the whole bundle.
        Detects: a push that leaves files out, or a verifier that reports success
        without reading back. Needs: the fake sftp. Breadcrumb: push_and_verify."""
        res = self.push()
        self.assertTrue(res["ok"], res)
        self.assertEqual(res["stage"], "verified")
        self.assertEqual(res["files"], res["readback_files"])
        self.assertEqual(hx.verify_tree(self.remote / "drop-host" / self.name), [])
        self.assertFalse((self.work / "readback-drop-host").exists())

    def test_existing_bundle_is_never_overwritten(self):
        """Protects: earlier bundles on the drop. Detects: a second push of the same
        name reaching put (the batch must stop at mkdir) or being reported as success.
        Needs: the fake sftp. Breadcrumb: push_and_verify batch order."""
        self.assertTrue(self.push()["ok"])
        target = self.remote / "drop-host" / self.name / "MANIFEST.json"
        before = target.read_bytes()
        res = self.push()
        self.assertFalse(res["ok"])
        self.assertEqual(res["stage"], "upload")
        self.assertEqual(target.read_bytes(), before)

    def test_corrupted_readback_is_detected(self):
        """Protects: the hash check on readback. Detects: a destination copy that
        differs by one byte being reported as verified.
        Needs: the fake sftp in corrupt mode.
        Breadcrumb: push_and_verify, verify_tree."""
        os.environ["FAKE_SFTP_CORRUPT"] = "1"
        res = self.push()
        self.assertFalse(res["ok"])
        self.assertEqual(res["stage"], "readback-mismatch")
        self.assertTrue(res["bad"])

    def test_extra_file_in_readback_is_detected(self):
        """Protects: the drop copy matching the bundle exactly. Detects: a readback with
        a file SHA256SUMS does not list (a stray or planted file) passing, since the
        per-file hash check only covers listed files.
        Needs: the fake sftp in extra-file mode.
        Breadcrumb: push_and_verify file-list check."""
        os.environ["FAKE_SFTP_EXTRA"] = "1"
        res = self.push()
        self.assertFalse(res["ok"])
        self.assertEqual(res["readback_files"], res["files"] + 1)


class ReceiptUploadTests(unittest.TestCase):
    def run_uploads(self, failing):
        results = [
            {"host": "required", "ok": True},
            {"host": "mirror", "ok": True},
            {"host": "skipped", "ok": False},
        ]

        def fake_sftp(host, batch, work, key, user):
            if host == "mirror":
                raise failing
            return subprocess.CompletedProcess([], 0, "", "")

        old = hx.sftp
        hx.sftp = fake_sftp
        try:
            hx.upload_receipts(
                results, Path("/r.json"), "bundle", Path("/w"), Path("/k"), "u"
            )
        finally:
            hx.sftp = old
        return results

    def test_an_optional_mirror_receipt_failure_does_not_fail_the_run(self):
        """Protects: a verified required destination keeping its result when the
        receipt upload to an optional mirror times out or hits an OS error.
        Detects: the exception escaping, which skips the final receipt update and
        summary and fires the unit's failure alert for a successful export.
        Needs: a stubbed sftp; no network. Breadcrumb: upload_receipts in main()."""
        for failing in (
            subprocess.TimeoutExpired("sftp", 1),
            OSError("connection reset"),
        ):
            with self.subTest(type(failing).__name__):
                results = self.run_uploads(failing)
                self.assertIs(results[0]["receipt_uploaded"], True)
                self.assertIs(results[1]["receipt_uploaded"], False)
                self.assertEqual(results[1]["receipt_error"], type(failing).__name__)
                self.assertNotIn("receipt_uploaded", results[2])


class UnitFileTests(unittest.TestCase):
    def read(self, name: str) -> str:
        return (PKG / "systemd" / name).read_text()

    def test_service_sources_env_alerts_and_locks(self):
        """Protects: a failed run being noticed, and runs having the toolchain PATH.
        Detects: a lost OnFailure= alert, a unit that does not source env.bash (a bare
        user-unit PATH lacks uv and agent-bus), a missing flock, or an ExecStart path
        that install.sh does not create.
        Needs: nothing. Breadcrumb: README "Run", install.sh."""
        svc = self.read("agent-hub-history-export.service")
        self.assertIn("OnFailure=agent-hub-history-export-notify@%n.service", svc)
        self.assertIn('source "$HOME/.config/bash/env.bash" &&', svc)
        self.assertIn("exec flock -n", svc)
        self.assertIn(
            "$HOME/.local/share/agent-hub-history-export/agent-hub-history-export.py",
            svc,
        )
        notify = self.read("agent-hub-history-export-notify@.service")
        self.assertIn("--schema finding", notify)
        self.assertIn("SEVERITY: high", notify)
        self.assertIn('source "$HOME/.config/bash/env.bash" &&', notify)

    @unittest.skipUnless(shutil.which("flock"), "needs flock")
    def test_fresh_home_without_a_cache_directory_still_runs(self):
        """Protects: the first run on a fresh install, before anything has created
        ~/.cache, taking its lock and reaching the exporter.
        Detects: flock opening the lock file under a missing ~/.cache and exiting
        non-zero before the export starts (the unit then alerts on every first run).
        Needs: a temporary HOME, an empty env.bash and a stub uv; no network.
        Breadcrumb: the service ExecStart; install.sh makes only share and unit dirs."""
        match = re.search(
            r"(?m)^ExecStart=/bin/bash -c '(.*)'$",
            self.read("agent-hub-history-export.service"),
        )
        self.assertIsNotNone(match)
        with tempfile.TemporaryDirectory() as tmp:
            home = Path(tmp) / "home"
            (home / ".config" / "bash").mkdir(parents=True)
            (home / ".config" / "bash" / "env.bash").write_text("")
            stub = Path(tmp) / "bin"
            stub.mkdir()
            (stub / "uv").write_text("#!/bin/sh\nexit 0\n")
            (stub / "uv").chmod(0o755)
            env = {
                **os.environ,
                "HOME": str(home),
                "PATH": f"{stub}:{os.environ['PATH']}",
            }
            done = subprocess.run(
                ["/bin/bash", "-c", match[1]],
                env=env,
                capture_output=True,
                text=True,
                check=False,
            )
            self.assertEqual(done.returncode, 0, done.stderr)
            self.assertTrue((home / ".cache").is_dir())

    def test_timer_is_daily_and_catches_up(self):
        """Protects: the daily cadence across hub downtime. Detects: a timer that skips
        the runs missed while the hub was off (Persistent) or targets another unit.
        Needs: nothing. Breadcrumb: README "Run"."""
        timer = self.read("agent-hub-history-export.timer")
        self.assertIn("OnCalendar=daily", timer)
        self.assertIn("Persistent=true", timer)
        self.assertIn("Unit=agent-hub-history-export.service", timer)


@unittest.skipUnless(
    sys.platform.startswith("linux"),
    "install.sh targets systemd user units; Linux only",
)
class InstallTests(TempDirCase):
    def run_install(
        self, *args: str, env_bash: bool = True
    ) -> subprocess.CompletedProcess:
        home = self.tmp / "home"
        (home / ".config" / "bash").mkdir(parents=True, exist_ok=True)
        if env_bash:
            (home / ".config" / "bash" / "env.bash").write_text("")
        recorder = self.tmp / "systemctl"
        recorder.write_text(f'#!/bin/sh\necho "$@" >> "{self.tmp}/systemctl.log"\n')
        recorder.chmod(0o755)
        env = {
            "PATH": os.environ.get("PATH", "/usr/bin:/bin"),
            "HOME": str(home),
            "SYSTEMCTL": str(recorder),
        }
        bash = shutil.which("bash")
        self.assertIsNotNone(bash, "bash is required on Linux CI runners")
        return subprocess.run(
            [bash, str(PKG / "install.sh"), *args],
            env=env,
            capture_output=True,
            text=True,
            timeout=60,
            check=False,
        )

    def calls(self) -> list[str]:
        log = self.tmp / "systemctl.log"
        return log.read_text().splitlines() if log.exists() else []

    def test_dry_run_writes_nothing(self):
        """Protects: --dry-run as a safe preview. Detects: a dry run that installs files
        or calls systemctl. Needs: bash. Breadcrumb: README "Install"."""
        res = self.run_install("--dry-run")
        self.assertEqual(res.returncode, 0, res.stderr)
        self.assertFalse((self.tmp / "home" / ".local").exists())
        self.assertEqual(self.calls(), [])

    def test_install_places_files_and_enables_timer(self):
        """Protects: the deployed layout the service's ExecStart names. Detects: a file
        missing from the install, or a missing daemon-reload/enable, and a second run
        that rewrites unchanged files. Needs: bash. Breadcrumb: README "Install"."""
        res = self.run_install()
        self.assertEqual(res.returncode, 0, res.stderr)
        home = self.tmp / "home"
        script = (
            home / ".local/share/agent-hub-history-export/agent-hub-history-export.py"
        )
        self.assertEqual(script.read_bytes(), SCRIPT.read_bytes())
        for unit in (
            "agent-hub-history-export.service",
            "agent-hub-history-export.timer",
            "agent-hub-history-export-notify@.service",
        ):
            self.assertEqual(
                (home / ".config/systemd/user" / unit).read_bytes(),
                (PKG / "systemd" / unit).read_bytes(),
            )
        self.assertEqual(
            self.calls(),
            [
                "--user daemon-reload",
                "--user enable --now agent-hub-history-export.timer",
            ],
        )
        again = self.run_install("--no-enable")
        self.assertEqual(again.returncode, 0, again.stderr)
        self.assertNotIn("installed ", again.stdout)
        self.assertEqual(again.stdout.count("unchanged "), 4)

    def test_missing_env_bash_refuses(self):
        """Protects: never installing a timer that cannot start. Detects: install.sh
        succeeding when env.bash, which the units source for PATH, is absent.
        Needs: bash. Breadcrumb: install.sh preflight."""
        res = self.run_install(env_bash=False)
        self.assertEqual(res.returncode, 1)
        self.assertIn("env.bash is missing", res.stderr)
        self.assertFalse((self.tmp / "home" / ".local").exists())


class DropInstallerTests(unittest.TestCase):
    def test_sshd_config_is_validated_before_replacing_the_live_one(self):
        """Protects: sshd on the Windows drop hosts. Detects: the installer copying a
        candidate sshd_config over the live one before `sshd -t -f` accepts it, which
        could lock every ssh user out. Needs: nothing (static order check).
        Breadcrumb: windows/Install-AgentHubDrop.ps1, README "The drop"."""
        ps1 = (PKG / "windows" / "Install-AgentHubDrop.ps1").read_text()
        test_at = ps1.index("& $sshd -t -f $candidate")
        self.assertLess(test_at, ps1.index("Copy-Item $candidate $cfg -Force"))
        self.assertIn("ForceCommand internal-sftp", ps1)
        self.assertIn("ChrootDirectory C:\\codedev\\agent-hub-artifacts\\incoming", ps1)
        self.assertRegex(ps1, r"\$pub = 'restrict ssh-ed25519 ")

    def test_package_carries_no_private_key_or_fingerprint(self):
        """Protects: host secrets staying out of the repo. Detects: a private key block
        or an ssh key fingerprint added to any packaged file; only the public key may
        appear. Needs: nothing. Breadcrumb: README "Install"."""
        for p in sorted(PKG.rglob("*")):
            if p.is_file():
                text = p.read_text(errors="replace")
                self.assertNotIn("PRIVATE KEY", text, p)
                self.assertIsNone(re.search(r"SHA256:[A-Za-z0-9+/]{43}", text), p)


if __name__ == "__main__":
    unittest.main()
