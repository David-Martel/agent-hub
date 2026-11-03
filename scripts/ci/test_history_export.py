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
import tarfile
import tempfile
import textwrap
import unittest
from pathlib import Path
from unittest import mock

ROOT = Path(__file__).resolve().parents[2]
PKG = ROOT / "packaging" / "history-export"
SCRIPT = PKG / "agent-hub-history-export.py"
NOTIFY = PKG / "agent-hub-history-export-notify.sh"


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
        f"snapshot_utc|2026-10-29T00:00:00Z\nmessages|{n_messages}\npresence_events|1\n"
    )


class TempDirCase(unittest.TestCase):
    def setUp(self):
        self.tmp = Path(tempfile.mkdtemp(prefix="history-export-test-"))
        self.addCleanup(shutil.rmtree, self.tmp, True)


class DriveTests(TempDirCase):
    def setUp(self):
        super().setUp()
        self.production_reserve = hx.DRIVE_RESERVE
        # Positive fixtures use a small isolated tmpfs, never a100GiB real export.
        reserve = mock.patch.object(hx, "DRIVE_RESERVE", 0)
        reserve.start()
        self.addCleanup(reserve.stop)

    def bundle(self):
        raw, bundle = self.tmp / "raw", self.tmp / "bundle"
        make_raw(raw)
        name = hx.build_bundle(
            raw,
            bundle,
            {
                "pg_messages": 5,
                "pg_presence_events": 1,
                "pg_snapshot_utc": "2026-10-29T00:00:00Z",
            },
        )
        return bundle, name

    def mismatched_snapshot_bundle(self, snapshot, messages=5, presence=1):
        raw, bundle = self.tmp / "raw", self.tmp / "bundle"
        make_raw(raw)
        (raw / "snapshot.tsv").write_text(snapshot)
        hx.build_bundle(
            raw,
            bundle,
            {
                "pg_messages": messages,
                "pg_presence_events": presence,
                "pg_snapshot_utc": "2026-10-29T00:00:00Z",
            },
        )
        return bundle

    def test_drive_truncated_pg_rows_fail_even_with_a_valid_health_window(self):
        """Protects: the captured PG snapshot, beyond an internally hashed bundle.
        Detects: five exported rows being labelled as a100-row snapshot even when
        health counts admit100. Needs: synthetic rows only. Breadcrumb:
        validate_drive_bundle snapshot/count binding and check_window.
        """
        hx.check_window(90, 100, 110)
        bundle = self.mismatched_snapshot_bundle(
            "snapshot_utc|2026-10-29T00:00:00Z\nmessages|100\npresence_events|1\n",
            messages=100,
        )
        with self.assertRaisesRegex(hx.DriveHold, "export_snapshot_count_mismatch"):
            hx.validate_drive_bundle(bundle)

    def test_drive_truncated_presence_rows_fail_snapshot_binding(self):
        """Protects: complete presence history, absent from decoded export--all.
        Detects: one presence row accepted against a seven-row recorded snapshot.
        Needs: synthetic PG/presence rows. Breadcrumb: validate_drive_bundle.
        """
        bundle = self.mismatched_snapshot_bundle(
            "snapshot_utc|2026-10-29T00:00:00Z\nmessages|5\npresence_events|7\n",
            presence=7,
        )
        with self.assertRaisesRegex(hx.DriveHold, "export_snapshot_count_mismatch"):
            hx.validate_drive_bundle(bundle)

    def test_drive_snapshot_tsv_counts_cannot_disagree_with_manifest(self):
        """Protects: snapshot.tsv as the same source binding as MANIFEST/counts.
        Detects: all five parsed PG rows passing while snapshot.tsv records four.
        Needs: synthetic rows; no live stores. Breadcrumb: validate_drive_bundle.
        """
        bundle = self.mismatched_snapshot_bundle(
            "snapshot_utc|2026-10-29T00:00:00Z\nmessages|4\npresence_events|1\n"
        )
        with self.assertRaisesRegex(hx.DriveHold, "export_snapshot_count_mismatch"):
            hx.validate_drive_bundle(bundle)

    def test_drive_snapshot_time_and_complete_fields_are_required(self):
        """Protects: a unique complete snapshot identity for restore qualification.
        Detects: wrong snapshot time or a missing presence count being accepted.
        Needs: synthetic bundle clones only. Breadcrumb: validate_drive_bundle.
        """
        bundle = self.mismatched_snapshot_bundle(
            "snapshot_utc|different\nmessages|5\npresence_events|1\n"
        )
        with self.assertRaisesRegex(hx.DriveHold, "export_snapshot_count_mismatch"):
            hx.validate_drive_bundle(bundle)
        shutil.rmtree(self.tmp / "raw")
        shutil.rmtree(bundle)
        bundle = self.mismatched_snapshot_bundle(
            "snapshot_utc|2026-10-29T00:00:00Z\nmessages|5\n"
        )
        with self.assertRaisesRegex(hx.DriveHold, "export_snapshot_fields_invalid"):
            hx.validate_drive_bundle(bundle)

    def test_drive_bundle_validation_rejects_extra_and_traversal_files(self):
        """Protects: exact, safely restorable backup census. Detects: an unlisted
        file or a checksum path escaping the backup being accepted. Needs: only
        synthetic files. Breadcrumb: validate_drive_bundle and README Drive restore.
        """
        bundle, _ = self.bundle()
        hx.validate_drive_bundle(bundle)
        (bundle / "stray").write_text("unlisted")
        with self.assertRaises(hx.DriveHold):
            hx.validate_drive_bundle(bundle)
        (bundle / "stray").unlink()
        with (bundle / "SHA256SUMS").open("a") as stream:
            stream.write("0" * 64 + "  ../escape\n")
        with self.assertRaises(hx.DriveHold):
            hx.validate_drive_bundle(bundle)

    def test_drive_private_proof_refuses_shared_or_wrong_owner(self):
        """Protects: owner-only cloud backup. Detects: a shared object or a different
        account passing the provider proof. Needs: synthetic API objects only.
        Breadcrumb: DriveStore.check_private.
        """
        obj = {"owners": [{"emailAddress": "owner@example.invalid"}], "trashed": False}
        perms = {
            "permissions": [
                {
                    "type": "user",
                    "role": "owner",
                    "emailAddress": "owner@example.invalid",
                }
            ]
        }
        hx.DriveStore.check_private(obj, perms, "owner@example.invalid")
        with self.assertRaises(hx.DriveHold):
            hx.DriveStore.check_private(obj, perms, "other@example.invalid")
        perms["permissions"].append({"type": "anyone", "role": "reader"})
        with self.assertRaises(hx.DriveHold):
            hx.DriveStore.check_private(obj, perms, "owner@example.invalid")

    def test_drive_environment_cannot_override_dedicated_remote(self):
        """Protects: fixed qualified Drive configuration. Detects: inherited rclone
        environment redirecting the destination or auth. Needs: patched process env,
        no credentials. Breadcrumb: drive_environment.
        """
        with mock.patch.dict(
            os.environ,
            {"RCLONE_CONFIG": "wrong", "RCLONE_CONFIG_MIGRATION_DRIVE_TYPE": "http"},
        ):
            env = hx.drive_environment()
        self.assertFalse(any(k.startswith("RCLONE_") for k in env))

    def test_drive_cloud_failure_keeps_sealed_cipher_for_exact_retry(self):
        """Protects: durable failed backups and retry without live re-export.
        Detects: failed readback discarding a closed cipher or a retry re-encrypting.
        Needs: synthetic bundle, mocked age and cloud. Breadcrumb: close_drive_job,
        publish_drive_job; no real backends, credentials or package code.
        """
        bundle, name = self.bundle()
        job = self.tmp / "job"
        job.mkdir()
        shutil.move(str(bundle), job / "bundle")
        recipient = self.tmp / "recipient.txt"
        recipient.write_text("synthetic-public-recipient\n")

        def encrypt(args, timeout=120):
            Path(args[args.index("--output") + 1]).write_bytes(
                b"fixture-cipher:" + Path(args[-1]).read_bytes()
            )
            return b""

        self.patch_recipient(recipient)
        with mock.patch.object(hx, "drive_run", side_effect=encrypt) as age:
            binding = hx.close_drive_job(job, name)
            self.assertEqual(age.call_count, 1)
            first = hx.digest(job / binding["cipher_file"])
            hx.close_drive_job(job, name)
            self.assertEqual(age.call_count, 1)
        store = mock.Mock()
        store.put.side_effect = hx.DriveHold("full_remote_stream_mismatch")
        with self.assertRaisesRegex(hx.DriveHold, "full_remote_stream_mismatch"):
            hx.publish_drive_job(job, store)
        store.put.assert_called_once()
        self.assertEqual(hx.digest(job / binding["cipher_file"]), first)
        self.assertTrue((job / "bundle" / "SHA256SUMS").exists())
        self.assertFalse((job / "verified.json").exists())

    def patch_recipient(self, recipient):
        for key, value in [
            ("DRIVE_RECIPIENT", recipient),
            ("DRIVE_RECIPIENT_SHA", hx.digest(recipient)[0]),
        ]:
            patch = mock.patch.object(hx, key, value)
            patch.start()
            self.addCleanup(patch.stop)

    def closed_job(self):
        bundle, name = self.bundle()
        job = self.tmp / "job"
        job.mkdir()
        shutil.move(str(bundle), job / "bundle")
        recipient = self.tmp / "recipient"
        recipient.write_text("fixture-public-recipient")
        self.patch_recipient(recipient)

        def encrypt(args, timeout=120):
            Path(args[args.index("--output") + 1]).write_bytes(
                b"cipher:" + Path(args[-1]).read_bytes()
            )
            return b""

        with mock.patch.object(hx, "drive_run", side_effect=encrypt):
            binding = hx.close_drive_job(job, name)
        return job, binding

    def test_drive_tampered_cipher_is_held_before_any_cloud_call(self):
        """Protects: exact closed-cipher retry. Detects: modified local ciphertext
        being uploaded under a prior proof. Needs: synthetic closed job only.
        Breadcrumb: close_drive_job binding checks before publish_drive_job.
        """
        job, binding = self.closed_job()
        path = job / binding["cipher_file"]
        path.write_bytes(b"tampered")
        store = mock.Mock()
        with self.assertRaisesRegex(hx.DriveHold, "closed_cipher_changed"):
            hx.publish_drive_job(job, store)
        store.account.assert_not_called()
        self.assertEqual(path.read_bytes(), b"tampered")

    def test_drive_success_requires_both_cipher_and_receipt_readback(self):
        """Protects: a remotely recoverable exact bundle and durable receipt.
        Detects: completion after cipher upload alone, plaintext deletion, or unsafe
        tar members. Needs: synthetic job and provider proofs; no network/key.
        Breadcrumb: publish_drive_job, close_drive_job, README Drive restore.
        """
        job, binding = self.closed_job()
        store = mock.Mock()
        store.put.side_effect = lambda path, name, expected: {
            "object": {"id": name},
            "full_readback": {"sha256": expected["sha256"], "bytes": expected["bytes"]},
        }
        result = hx.publish_drive_job(job, store)
        self.assertEqual(store.put.call_count, 2)
        self.assertEqual(result["status"], "REMOTE_VERIFIED")
        self.assertTrue((job / "verified.json").is_file())
        self.assertTrue((job / "bundle/MANIFEST.json").is_file())
        self.assertTrue((job / binding["cipher_file"]).is_file())
        with tarfile.open(job / "archive.tar") as archive:
            self.assertTrue(
                all(
                    m.isfile()
                    and not m.name.startswith("/")
                    and ".." not in Path(m.name).parts
                    for m in archive.getmembers()
                )
            )
            self.assertEqual(
                set(archive.getnames()),
                {
                    p.relative_to(job / "bundle").as_posix()
                    for p in (job / "bundle").rglob("*")
                    if p.is_file()
                },
            )

    def test_drive_receipt_failure_is_not_reported_as_complete(self):
        """Protects: completion-receipt durability. Detects: a failed second upload
        being mistaken for success. Needs: synthetic job and failed fake receipt.
        Breadcrumb: publish_drive_job writes verified.json only after both proofs.
        """
        job, _ = self.closed_job()
        store = mock.Mock()
        store.put.side_effect = [
            {"object": {"id": "cipher"}},
            hx.DriveHold("receipt_readback_failed"),
        ]
        with self.assertRaisesRegex(hx.DriveHold, "receipt_readback_failed"):
            hx.publish_drive_job(job, store)
        self.assertTrue((job / "cloud-receipt.json").is_file())
        self.assertFalse((job / "verified.json").exists())

    def test_drive_receipt_retry_reuses_identical_closed_receipt_bytes(self):
        """Protects: immutable receipt retry after an interrupted second upload.
        Detects: new receipt bytes/name or re-encryption on retry. Needs: synthetic
        job/provider, no live config. Breadcrumb: publish_drive_job receipt reuse.
        """
        job, _ = self.closed_job()
        store = mock.Mock()
        store.put.side_effect = [
            {"object": {"id": "same-cipher"}},
            hx.DriveHold("receipt_interrupted"),
        ]
        with self.assertRaisesRegex(hx.DriveHold, "receipt_interrupted"):
            hx.publish_drive_job(job, store)
        original = (job / "cloud-receipt.json").read_bytes()
        store.put.side_effect = lambda path, name, expected: {
            "object": {"id": "same-cipher" if name.endswith(".age") else "receipt"}
        }
        with mock.patch.object(hx, "drive_run") as encrypt:
            hx.publish_drive_job(job, store)
        encrypt.assert_not_called()
        self.assertEqual((job / "cloud-receipt.json").read_bytes(), original)

    def test_drive_capacity_hold_keeps_plaintext_before_creating_cipher(self):
        """Protects: the source100GiB reserve during other ongoing migrations.
        Detects: encryption or archive allocation when capacity is unavailable.
        Needs: synthetic bundle and disk-usage fixture. Breadcrumb: close_drive_job.
        """
        bundle, name = self.bundle()
        job = self.tmp / "job"
        job.mkdir()
        shutil.move(str(bundle), job / "bundle")
        self.assertEqual(self.production_reserve, 100 * 1024**3)
        usage = shutil.disk_usage(job)._replace(free=self.production_reserve - 1)
        with (
            mock.patch.object(hx, "DRIVE_RESERVE", self.production_reserve),
            mock.patch.object(hx.shutil, "disk_usage", return_value=usage),
            mock.patch.object(hx, "drive_run") as encrypt,
        ):
            with self.assertRaisesRegex(hx.DriveHold, "backup_100GiB_reserve"):
                hx.close_drive_job(job, name)
        encrypt.assert_not_called()
        self.assertTrue((job / "bundle/MANIFEST.json").is_file())
        self.assertFalse((job / "archive.tar").exists())

    def test_drive_conflicting_remote_is_preserved_without_overwrite(self):
        """Protects: immutable cloud history. Detects: a same-name wrong object
        being overwritten or accepted. Needs: fake API metadata only.
        Breadcrumb: DriveStore.put provider binding before readback.
        """
        store = object.__new__(hx.DriveStore)
        store.api = mock.Mock(return_value={"files": [{"id": "existing"}]})
        store.private = mock.Mock(
            return_value={
                "parents": [hx.DRIVE_FOLDER],
                "size": "1",
                "md5Checksum": "wrong",
            }
        )
        store.rc = mock.Mock()
        store.readback = mock.Mock()
        with self.assertRaisesRegex(hx.DriveHold, "remote_object_binding_mismatch"):
            store.put(
                self.tmp / "unused",
                "safe.age",
                {"bytes": 2, "md5": "expected", "sha256": "0" * 64},
            )
        store.rc.assert_not_called()
        store.readback.assert_not_called()

    def test_drive_duplicate_remote_name_and_wrong_parent_are_rejected(self):
        """Protects: unambiguous private object identity. Detects: duplicate names
        or an object outside the qualified folder passing. Needs: fake API only.
        Breadcrumb: DriveStore.put find and parent proof.
        """
        store = object.__new__(hx.DriveStore)
        store.api = mock.Mock(return_value={"files": [{"id": "a"}, {"id": "b"}]})
        with self.assertRaisesRegex(hx.DriveHold, "ambiguous_remote_name"):
            store.put(self.tmp / "unused", "safe.age", {})
        store.api.return_value = {"files": [{"id": "a"}]}
        store.private = mock.Mock(return_value={"parents": ["wrong-folder"]})
        with self.assertRaisesRegex(hx.DriveHold, "remote_object_binding_mismatch"):
            store.put(self.tmp / "unused", "safe.age", {})

    def test_drive_existing_partial_is_retained_without_encrypting(self):
        """Protects: interrupted backup evidence. Detects: an unknown partial
        being replaced, removed or silently retried. Needs: synthetic bundle.
        Breadcrumb: close_drive_job unbound_partial_preserved gate.
        """
        bundle, name = self.bundle()
        job = self.tmp / "job"
        job.mkdir()
        shutil.move(str(bundle), job / "bundle")
        partial = job / "ciphertext.pending"
        partial.write_bytes(b"partial")
        with mock.patch.object(hx, "drive_run") as encrypt:
            with self.assertRaisesRegex(hx.DriveHold, "unbound_partial_preserved"):
                hx.close_drive_job(job, name)
        encrypt.assert_not_called()
        self.assertEqual(partial.read_bytes(), b"partial")

    def test_drive_auth_failure_preserves_job_and_retries_without_export(self):
        """Protects: failure receipts and retry independent of live databases.
        Detects: auth failure deleting a backup or retry querying production stores.
        Needs: synthetic closed job and fake DriveStore; no live configuration.
        Breadcrumb: drive_backup chooses existing unverified job before capture.
        """
        job, binding = self.closed_job()
        state = self.tmp / "state"
        spool = state / "drive-spool"
        spool.mkdir(parents=True)
        target = spool / binding["name"]
        shutil.move(str(job), target)
        store = mock.Mock()
        store.account.side_effect = hx.DriveHold("official_drive_read_failed")
        with (
            mock.patch.object(hx, "STATE", state),
            mock.patch.object(hx, "DriveStore", return_value=store),
            mock.patch.object(hx, "export_pg") as pg,
            mock.patch.object(hx, "export_redis") as redis,
        ):
            self.assertEqual(hx.drive_backup(no_bus=True), 1)
        pg.assert_not_called()
        redis.assert_not_called()
        self.assertTrue((target / binding["cipher_file"]).is_file())
        self.assertEqual(
            json.loads((state / "last-drive-result.json").read_text())["status"], "HELD"
        )

    def test_drive_notices_satisfy_finding_and_status_schema_fields(self):
        """Protects: failed scheduled backups being visible on the coordination bus.
        Detects: a finding rejected for missing FINDING/SEVERITY, or a success
        missing STATUS. Needs: synthetic closed job/provider and mocked bus only.
        Breadcrumb: drive_backup notices, bus schema selection and README alerts.
        """
        job, binding = self.closed_job()
        state = self.tmp / "state"
        spool = state / "drive-spool"
        spool.mkdir(parents=True)
        shutil.move(str(job), spool / binding["name"])
        store = mock.Mock()
        store.account.side_effect = [hx.DriveHold("official_drive_read_failed"), None]
        store.put.side_effect = lambda path, name, expected: {"object": {"id": name}}
        with (
            mock.patch.object(hx, "STATE", state),
            mock.patch.object(hx, "DriveStore", return_value=store),
            mock.patch.object(hx, "bus") as notice,
        ):
            self.assertEqual(hx.drive_backup(), 1)
            self.assertTrue(notice.call_args.args[0].startswith("FINDING:"))
            self.assertIn("SEVERITY: high", notice.call_args.args[0])
            self.assertFalse(notice.call_args.args[1])
            self.assertEqual(hx.drive_backup(), 0)
            self.assertTrue(notice.call_args.args[0].startswith("STATUS:"))
            self.assertTrue(notice.call_args.args[1])

    @unittest.skipUnless(os.name == "posix", "pipe selectors need POSIX")
    def test_drive_full_readback_rejects_corrupted_and_truncated_stream(self):
        """Protects: actual full readback bytes, not a provider listing. Detects:
        equal-length corruption and truncation. Needs: owned Python stdout child,
        no network/credentials. Breadcrumb: DriveStore.readback stream digest/size.
        """
        store = object.__new__(hx.DriveStore)
        real_popen = subprocess.Popen
        expected = {"bytes": 4, "sha256": hashlib.sha256(b"good").hexdigest()}
        for value in [b"good", b"evil", b"bad"]:
            with self.subTest(stream=value):

                def child(*args, **kwargs):
                    return real_popen(
                        [
                            sys.executable,
                            "-c",
                            "import sys;sys.stdout.buffer.write(" + repr(value) + ")",
                        ],
                        stdout=subprocess.PIPE,
                        stderr=subprocess.DEVNULL,
                        env={"PATH": os.defpath},
                    )

                with mock.patch.object(hx.subprocess, "Popen", side_effect=child):
                    if value == b"good":
                        self.assertEqual(
                            store.readback("fixture", expected)["bytes"], 4
                        )
                    else:
                        with self.assertRaisesRegex(
                            hx.DriveHold, "full_remote_stream_mismatch"
                        ):
                            store.readback("fixture", expected)


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
        self.assertIn(
            '"$HOME/.local/share/agent-hub-history-export/'
            'agent-hub-history-export-notify.sh" "%i" "%H"',
            notify,
        )
        self.assertIn('source "$HOME/.config/bash/env.bash" &&', notify)

    def test_service_uses_drive_without_a_gen9_or_gen7_transport(self):
        """Protects: daily backup independence from retiring Windows drops.
        Detects: required or optional old-host transport surviving in ExecStart.
        Needs: source unit text only. Breadcrumb: README private Drive runbook.
        """
        service = self.read("agent-hub-history-export.service")
        self.assertIn("--drive'", service)
        self.assertIn("uv run --offline", service)
        self.assertNotIn("dtm-p1gen9", service)
        self.assertNotIn("dtm-p1gen7", service)

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


@unittest.skipUnless(os.name == "posix", "the notify script runs under /bin/sh")
class NotifyTests(TempDirCase):
    def notify(self, unit: str) -> list[str]:
        """Run the notify script against a recorder and return the argv it sent."""
        out = self.tmp / "argv.json"
        recorder = self.tmp / "agent-bus"
        recorder.write_text(
            "#!/bin/sh\n"
            f"exec {sys.executable} -c 'import json, sys; "
            f'json.dump(sys.argv[1:], open("{out}", "w"))\' "$@"\n'
        )
        recorder.chmod(0o755)
        env = dict(os.environ, AGENT_BUS=str(recorder))
        res = subprocess.run(
            ["/bin/sh", str(NOTIFY), unit, "asuspro13"],
            env=env,
            capture_output=True,
            text=True,
            timeout=60,
            check=False,
        )
        self.assertEqual(res.returncode, 0, res.stderr)
        return json.loads(out.read_text())

    @staticmethod
    def opt(argv: list[str], name: str) -> list[str]:
        return [argv[i + 1] for i, a in enumerate(argv) if a == name]

    def test_real_failure_is_a_high_finding_on_machine_hygiene(self):
        """Protects: a failed export being seen by the lanes. Detects: an alert that
        loses SEVERITY high, the unit name, or the machine-hygiene thread.
        Needs: /bin/sh. Breadcrumb: README "Run"."""
        argv = self.notify("agent-hub-history-export.service")
        body = self.opt(argv, "--body")[0]
        self.assertEqual(self.opt(argv, "--thread-id"), ["machine-hygiene"])
        self.assertEqual(self.opt(argv, "--schema"), ["finding"])
        self.assertIn("agent-hub-history-export.service failed on asuspro13", body)
        self.assertTrue(body.endswith("\nSEVERITY: high"), body)
        self.assertNotIn("POSITIVE-CONTROL", body)

    def test_positive_control_is_labelled_and_kept_off_the_shared_thread(self):
        """Protects: the shared thread from test alerts read as incidents, which
        happened on 2026-10-29 18:41Z. Detects: a positive-control instance posting
        to machine-hygiene, at high severity, or without the POSITIVE-CONTROL
        label. Needs: /bin/sh. Breadcrumb: README "Run"."""
        for unit in ("positive-control", "selftest-positive-control"):
            argv = self.notify(unit)
            body = self.opt(argv, "--body")[0]
            self.assertEqual(
                self.opt(argv, "--thread-id"), ["agent-hub-alert-selftest"]
            )
            self.assertNotIn("machine-hygiene", argv)
            self.assertTrue(body.startswith("FINDING: POSITIVE-CONTROL"), body)
            self.assertTrue(body.endswith("\nSEVERITY: info"), body)


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
        self.assertEqual(
            (script.parent / NOTIFY.name).read_bytes(), NOTIFY.read_bytes()
        )
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
        self.assertEqual(again.stdout.count("unchanged "), 5)

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
