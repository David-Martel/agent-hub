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
import io
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
from datetime import datetime, timedelta, timezone
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


def make_raw(
    raw: Path, n_messages: int = 5, body: str = "hello", redis_ids=("m0",)
) -> None:
    """Write the raw export files build_bundle expects, as the exporters would."""
    raw.mkdir(parents=True, exist_ok=True)
    with open(raw / "messages.ndjson", "w") as f:
        f.writelines(
            json.dumps({"id": f"m{i}", "body": f"{body}-{i}"}) + "\n"
            for i in range(n_messages)
        )
    (raw / "redis-stream-messages.ndjson").write_text(
        "".join(
            json.dumps({"stream_id": f"{n + 1}-0", "fields": {"id": ident}}) + "\n"
            for n, ident in enumerate(redis_ids)
        )
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


def fixture_proof(name, expected):
    """A complete synthetic provider proof: object binding plus full readback."""
    return {
        "object": {
            "id": name,
            "parents": [hx.DRIVE_FOLDER],
            "size": str(expected["bytes"]),
            "md5Checksum": expected["md5"],
        },
        "full_readback": {"sha256": expected["sha256"], "bytes": expected["bytes"]},
    }


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
        store.put.side_effect = lambda path, name, expected: fixture_proof(
            name, expected
        )
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

        def put(path, name, expected):
            if name.endswith(".receipt.json"):
                raise hx.DriveHold("receipt_readback_failed")
            return fixture_proof("cipher", expected)

        store.put.side_effect = put
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

        def put(path, name, expected):
            if name.endswith(".receipt.json"):
                raise hx.DriveHold("receipt_interrupted")
            return fixture_proof("same-cipher", expected)

        store.put.side_effect = put
        with self.assertRaisesRegex(hx.DriveHold, "receipt_interrupted"):
            hx.publish_drive_job(job, store)
        original = (job / "cloud-receipt.json").read_bytes()
        store.put.side_effect = lambda path, name, expected: fixture_proof(
            "same-cipher" if name.endswith(".age") else "receipt", expected
        )
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
        store.put.side_effect = lambda path, name, expected: fixture_proof(
            name, expected
        )
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

    def test_drive_incomplete_provider_proof_is_not_verified(self):
        """Protects: REMOTE_VERIFIED meaning the destination hash and length were read
        back, not that an uploader returned. Detects: a proof lacking the full
        readback, or with another hash, length, parent or checksum, completing the
        job. Needs: synthetic closed job and provider proofs; no network or key.
        Breadcrumb: publish_drive_job, verify_proof.
        """
        wrong = {
            "no readback": lambda p: {"object": p["object"]},
            "other hash": lambda p: {
                **p,
                "full_readback": {**p["full_readback"], "sha256": "0" * 64},
            },
            "short length": lambda p: {
                **p,
                "full_readback": {**p["full_readback"], "bytes": 1},
            },
            "wrong parent": lambda p: {
                **p,
                "object": {**p["object"], "parents": ["elsewhere"]},
            },
            "wrong checksum": lambda p: {
                **p,
                "object": {**p["object"], "md5Checksum": "0" * 32},
            },
        }
        for label, damage in wrong.items():
            with self.subTest(label):
                job, _ = self.closed_job()
                store = mock.Mock()
                store.put.side_effect = lambda path, name, expected, d=damage: d(
                    fixture_proof(name, expected)
                )
                with self.assertRaisesRegex(hx.DriveHold, "remote_proof_incomplete"):
                    hx.publish_drive_job(job, store)
                self.assertFalse((job / "verified.json").exists())
                shutil.rmtree(job)

    def test_drive_archive_must_be_exactly_the_validated_bundle(self):
        """Protects: the encrypted payload being the bundle that was validated.
        Detects: a tar with a changed, extra, missing or duplicate member being
        accepted for encryption. Needs: synthetic bundle only.
        Breadcrumb: verify_archive, close_drive_job.
        """
        bundle, _ = self.bundle()
        files = sorted(p for p in bundle.rglob("*") if p.is_file())

        def tar_of(path, skip=None, extra=None, tweak=None, twice=None):
            with tarfile.open(path, "w") as out:
                for f in files:
                    rel = f.relative_to(bundle).as_posix()
                    if rel == skip:
                        continue
                    data = f.read_bytes() + (b"x" if rel == tweak else b"")
                    for _ in range(2 if rel == twice else 1):
                        info = tarfile.TarInfo(rel)
                        info.size = len(data)
                        out.addfile(info, io.BytesIO(data))
                if extra:
                    info = tarfile.TarInfo(extra)
                    info.size = 1
                    out.addfile(info, io.BytesIO(b"x"))

        good = self.tmp / "good.tar"
        tar_of(good)
        hx.verify_archive(good, bundle)
        for label, kwargs in {
            "changed": {"tweak": "MANIFEST.json"},
            "extra": {"extra": "stray.bin"},
            "missing": {"skip": "snapshot.tsv"},
            "traversal": {"extra": "../escape"},
            "duplicate": {"twice": "snapshot.tsv"},
        }.items():
            with self.subTest(label):
                bad = self.tmp / (label + ".tar")
                tar_of(bad, **kwargs)
                with self.assertRaises(hx.DriveHold):
                    hx.verify_archive(bad, bundle)

    def uncached_reader(self, wrong_for=()):
        """A stand-in for the external uncached reader, plus the age stand-in."""
        calls = []

        def run(args, timeout=120):
            if args[0] == "reader":
                path = Path(args[1])
                calls.append(path.name)
                sha, _, size = hx.digest(path)
                if path.name in wrong_for:
                    sha = "0" * 64
                return json.dumps({"sha256": sha, "bytes": size}).encode()
            Path(args[args.index("--output") + 1]).write_bytes(
                b"cipher:" + Path(args[-1]).read_bytes()
            )
            return b""

        return run, calls

    def test_drive_uncached_hash_must_match_before_the_cipher_is_closed(self):
        """Protects: the stored plaintext tar and ciphertext, which a hash taken right
        after writing cannot prove on a disk that returns wrong bytes. Detects: an
        uncached read that differs from the cached hash being ignored, or never
        being taken. Needs: synthetic bundle and a stand-in reader.
        Breadcrumb: uncached_digest, check_uncached, close_drive_job.
        """
        bundle, name = self.bundle()
        job = self.tmp / "job"
        job.mkdir()
        shutil.move(str(bundle), job / "bundle")
        recipient = self.tmp / "recipient"
        recipient.write_text("fixture-public-recipient")
        self.patch_recipient(recipient)
        mode = mock.patch.object(
            hx, "UNCACHED", {"mode": "command", "command": ["reader", "{path}"]}
        )
        mode.start()
        self.addCleanup(mode.stop)
        run, calls = self.uncached_reader(wrong_for=("ciphertext.pending",))
        with (
            mock.patch.object(hx, "drive_run", side_effect=run),
            self.assertRaisesRegex(hx.DriveHold, "uncached_hash_mismatch"),
        ):
            hx.close_drive_job(job, name)
        self.assertEqual(calls, ["archive.tar", "ciphertext.pending"])
        self.assertFalse((job / "binding.json").exists())
        self.assertTrue((job / "archive.tar").exists())
        self.assertTrue((job / "ciphertext.pending").exists())

    def test_drive_uncached_hash_is_rechecked_on_retry_and_garbage_is_held(self):
        """Protects: a ciphertext that rotted in the spool before a delayed upload.
        Detects: a retry trusting the cached hash alone, and an unreadable reader
        result being treated as a pass. Needs: synthetic closed job.
        Breadcrumb: close_drive_job existing-binding branch, uncached_digest.
        """
        job, binding = self.closed_job()
        mode = mock.patch.object(
            hx, "UNCACHED", {"mode": "command", "command": ["reader", "{path}"]}
        )
        mode.start()
        self.addCleanup(mode.stop)
        run, _ = self.uncached_reader(wrong_for=(binding["cipher_file"],))
        with (
            mock.patch.object(hx, "drive_run", side_effect=run),
            self.assertRaisesRegex(hx.DriveHold, "uncached_hash_mismatch"),
        ):
            hx.publish_drive_job(job, mock.Mock())
        with (
            mock.patch.object(hx, "drive_run", return_value=b"not json"),
            self.assertRaisesRegex(hx.DriveHold, "uncached_hash_unreadable"),
        ):
            hx.publish_drive_job(job, mock.Mock())
        run, calls = self.uncached_reader()
        with mock.patch.object(hx, "drive_run", side_effect=run):
            hx.close_drive_job(job, binding["name"])
        self.assertEqual(calls, [binding["cipher_file"]])

    def test_drive_refresh_failure_is_a_distinct_failed_outcome(self):
        """Protects: a dead OAuth grant (revoked, or expired under a Testing-mode
        consent screen) being reported as an actionable failure. Detects: it
        surfacing as a generic exit code, passing, or leaking rclone's output.
        Needs: a stand-in rclone through subprocess.run. Breadcrumb:
        DriveStore.auth_check.
        """
        store = object.__new__(hx.DriveStore)
        for stderr, reason in (
            (
                b'Failed to refresh: oauth2: "invalid_grant" SECRET',
                "remote_auth_failed",
            ),
            (b"Token has been expired or revoked.", "remote_auth_failed"),
            (b"dial tcp: no route", "remote_unreachable_exit_1"),
        ):
            done = subprocess.CompletedProcess([], 1, b"", stderr)
            with (
                mock.patch.object(hx.subprocess, "run", return_value=done),
                self.assertRaises(hx.DriveHold) as failed,
            ):
                store.account()
            self.assertEqual(str(failed.exception), reason)
        ok = subprocess.CompletedProcess([], 0, b"{}", b"")
        with mock.patch.object(hx.subprocess, "run", return_value=ok):
            store.auth_check()

    def test_drive_command_failure_names_the_tool_but_not_its_arguments(self):
        """Protects: diagnosable holds without payload in logs. Detects: a generic
        exit code that cannot say which command failed, a missing tool escaping as
        an unexplained OSError, or an argument leaking into the reason.
        Needs: the running Python only. Breadcrumb: drive_run.
        """
        with self.assertRaises(hx.DriveHold) as failed:
            hx.drive_run([sys.executable, "-c", "import sys; sys.exit(3)", "SECRET"])
        reason = str(failed.exception)
        self.assertRegex(reason, r"^command_failed_.*python.*_exit_3$")
        self.assertNotIn("SECRET", reason)
        with self.assertRaisesRegex(hx.DriveHold, "command_unavailable_no-such-tool"):
            hx.drive_run(["/no/such/dir/no-such-tool"])

    def test_drive_uncached_reader_failure_is_retried_then_held(self):
        """Protects: a reader that fails once on a just-written file (observed on
        dtm-work) not discarding a good export, and a dead reader not passing.
        Detects: no retry, or an unreadable reader being treated as a pass. Needs: a
        stand-in drive_run and sleep. Breadcrumb: uncached_digest.
        """
        target = self.tmp / "file.bin"
        target.write_bytes(b"abc")
        answer = b'{"sha256": "' + b"a" * 64 + b'", "bytes": 3}'
        outcomes = [hx.DriveHold("command_failed_exit_1"), answer]

        def flaky(args, timeout=120):
            outcome = outcomes.pop(0)
            if isinstance(outcome, Exception):
                raise outcome
            return outcome

        with (
            mock.patch.object(hx, "UNCACHED", {"mode": "command", "command": ["r"]}),
            mock.patch.object(hx.time, "sleep"),
            mock.patch.object(hx, "drive_run", side_effect=flaky),
        ):
            self.assertEqual(hx.uncached_digest(target)["bytes"], 3)
        dead = hx.DriveHold("command_failed_exit_1")
        with (
            mock.patch.object(hx, "UNCACHED", {"mode": "command", "command": ["r"]}),
            mock.patch.object(hx.time, "sleep"),
            mock.patch.object(hx, "drive_run", side_effect=dead) as run,
            self.assertRaisesRegex(hx.DriveHold, "uncached_reader_failed"),
        ):
            hx.uncached_digest(target)
        self.assertEqual(run.call_count, hx.UNCACHED_ATTEMPTS)

    def test_drive_uncached_command_gets_a_windows_path_from_wslpath(self):
        """Protects: the Windows-side reader being given a path it can open.
        Detects: {win_path} reaching the command unexpanded, or the reader's
        answer not being lower-cased and parsed. Needs: a stand-in drive_run.
        Breadcrumb: uncached_digest.
        """
        target = self.tmp / "file.bin"
        target.write_bytes(b"abc")
        seen = []

        def run(args, timeout=120):
            seen.append(args)
            if args[0] == "wslpath":
                return ("W:" + args[-1]).encode()
            return b'{"sha256": "' + b"A" * 64 + b'", "bytes": 3}'

        command = ["reader", "-Path", "{win_path}"]
        with (
            mock.patch.object(hx, "UNCACHED", {"mode": "command", "command": command}),
            mock.patch.object(hx, "drive_run", side_effect=run),
        ):
            got = hx.uncached_digest(target)
        self.assertEqual(got, {"sha256": "a" * 64, "bytes": 3})
        self.assertEqual(seen[0][:2], ["wslpath", "-w"])
        self.assertEqual(seen[-1], ["reader", "-Path", "W:" + str(target)])


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

    def test_work_profile_stages_a_revision_and_enables_nothing(self):
        """Protects: the reviewed revision being staged under its own directory with
        a `current` link, and the Work timer staying disabled until the activation
        gate. Detects: files placed over the asuspro13 layout, a missing link or
        windows reader, or any enable/start call. Needs: bash.
        Breadcrumb: install.sh --profile work, README Work profile.
        """
        res = self.run_install("--profile", "work", "--rev", "abc123")
        self.assertEqual(res.returncode, 0, res.stderr)
        share = self.tmp / "home/.local/share/agent-hub-history-export"
        self.assertEqual(
            (share / "abc123/agent-hub-history-export.py").read_bytes(),
            SCRIPT.read_bytes(),
        )
        self.assertEqual(
            (share / "abc123/windows/Get-UncachedHash.ps1").read_bytes(),
            (PKG / "windows/Get-UncachedHash.ps1").read_bytes(),
        )
        self.assertEqual(os.readlink(share / "current"), "abc123")
        self.assertFalse((share / "agent-hub-history-export.py").exists())
        units = self.tmp / "home/.config/systemd/user"
        for unit in (
            "agent-hub-history-export-work.service",
            "agent-hub-history-export-work.timer",
            "agent-hub-history-export-notify@.service",
        ):
            self.assertEqual(
                (units / unit).read_bytes(), (PKG / "systemd" / unit).read_bytes()
            )
        self.assertEqual(self.calls(), ["--user daemon-reload"])
        self.assertIn("NOT enabled", res.stdout)
        windows = self.tmp / "windows-dir"
        again = self.run_install(
            "--profile", "work", "--rev", "abc123", "--windows-dir", str(windows)
        )
        self.assertEqual(again.returncode, 0, again.stderr)
        self.assertEqual(
            (windows / "Get-UncachedHash.ps1").read_bytes(),
            (PKG / "windows/Get-UncachedHash.ps1").read_bytes(),
        )

    def test_work_profile_enables_only_when_asked_and_dry_run_writes_nothing(self):
        """Protects: explicit activation and a safe preview. Detects: --enable not
        reaching the Work timer, or a work dry run writing files or calling
        systemctl. Needs: bash. Breadcrumb: install.sh --profile work --enable.
        """
        dry = self.run_install("--profile", "work", "--rev", "abc123", "--dry-run")
        self.assertEqual(dry.returncode, 0, dry.stderr)
        self.assertFalse((self.tmp / "home/.local").exists())
        self.assertEqual(self.calls(), [])
        res = self.run_install("--profile", "work", "--rev", "abc123", "--enable")
        self.assertEqual(res.returncode, 0, res.stderr)
        self.assertEqual(
            self.calls(),
            [
                "--user daemon-reload",
                "--user enable --now agent-hub-history-export-work.timer",
            ],
        )

    def test_work_profile_refuses_an_unsafe_revision_or_unknown_profile(self):
        """Protects: the staging path staying inside the share directory. Detects:
        a traversal revision or an unknown profile being installed. Needs: bash.
        Breadcrumb: install.sh argument handling.
        """
        for args in (
            ("--profile", "work", "--rev", "../escape"),
            ("--profile", "work", "--rev", "a/b"),
            ("--profile", "bogus"),
        ):
            with self.subTest(args=args):
                res = self.run_install(*args)
                self.assertEqual(res.returncode, 2, res.stderr)
        self.assertFalse((self.tmp / "home/.local").exists())


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


# --- Work-authoritative profile -------------------------------------------------

FIXTURE_OWNER = "owner@example.invalid"
FIXTURE_REPLID = "r" * 40
FIXTURE_SYSID = "7617335056550016368"
CUTOVER = "2026-11-04T07:29:00Z"


def utc(offset_seconds: float = 0.0) -> str:
    moment = datetime.now(timezone.utc) + timedelta(seconds=offset_seconds)
    return moment.strftime("%Y-%m-%dT%H:%M:%S.%fZ")


def reading(**override):
    """One synthetic `agent-bus health` reading from the promoted hub."""
    base = {
        "hub_identity": "dtm-work",
        "backend": {"authoritative": True, "mode": "remote"},
        "maintenance": {
            "mode": "running",
            "write_blocked": False,
            "changed_at_utc": "2026-11-04T07:29:04.405444Z",
        },
        "pg_message_count": 5,
        "stream_length": 4,
        "build_version": "fixture",
    }
    return {**base, **override}


def source_meta(**override):
    meta = {
        "pg_snapshot_utc": utc(),
        "pg_messages": 5,
        "pg_presence_events": 1,
        "pg_max_message_utc": utc(-60),
        "pg_system_identifier": FIXTURE_SYSID,
        "pg_database_oid": "18653",
        "pg_server_version": "18.3",
    }
    return {**meta, **override}


def source_recon(**override):
    """Reconciliation for 5 PG rows and 4 stream entries: one row only in PG."""
    now_ms = int(datetime.now(timezone.utc).timestamp() * 1000)
    recon = {
        "redis_stream_entries": 4,
        "redis_entries_without_id": 0,
        "redis_unique_ids": 4,
        "redis_stream_xlen_before_after": [4, 4],
        "redis_last_stream_id": f"{now_ms}-0",
        "redis_generation": {
            "run_id": "a" * 40,
            "run_id_after": "a" * 40,
            "replid": FIXTURE_REPLID,
            "replid_after": FIXTURE_REPLID,
            "version": "8.6.1",
            "tcp_port": 6380,
        },
        "pg_lines": 5,
        "pg_rows": 5,
        "uuid_union": 5,
        "only_in_redis": 0,
        "only_in_pg": 1,
    }
    return {**recon, **override}


def work_config(tmp: Path) -> dict:
    return {
        "schema": "agent-hub.history-export-config/v1",
        "state_root": str(tmp / "state"),
        "source": {
            "hub_identity": "dtm-work",
            "cutover_utc": CUTOVER,
            "agent_bus": "agent-bus-fixture",
            "max_only_in_redis": 100,
            "postgres": {
                "psql": "psql-fixture",
                "host": "127.0.0.1",
                "port": 5432,
                "user": "postgres",
                "database": "redis_backend",
                "system_identifier": FIXTURE_SYSID,
            },
            "redis": {
                "host": "127.0.0.1",
                "port": 6380,
                "db": 0,
                "replid": FIXTURE_REPLID,
            },
        },
        "drive": {
            "rclone": "rclone-fixture",
            "rclone_config": str(tmp / "rclone.conf"),
            "age": "age-fixture",
            "recipient_file": str(tmp / "recipient.txt"),
            "owner_account": FIXTURE_OWNER,
            "reserve_bytes": 0,
        },
        "uncached": {"mode": "none"},
        "freshness": {"max_snapshot_age_seconds": 172800},
    }


class SettingsCase(TempDirCase):
    """Saves the module settings and the fixed owner hash, then restores them."""

    def setUp(self):
        super().setUp()
        saved = {name: getattr(hx, name) for name in hx.SETTING_NAMES}
        self.addCleanup(lambda: [setattr(hx, k, v) for k, v in saved.items()])
        owner = mock.patch.object(
            hx, "DRIVE_OWNER_SHA", hashlib.sha256(FIXTURE_OWNER.encode()).hexdigest()
        )
        owner.start()
        self.addCleanup(owner.stop)

    def write_config(self, config: dict) -> Path:
        path = self.tmp / "config.json"
        path.write_text(json.dumps(config))
        return path

    def bind(self, **override):
        values = {
            "SOURCE_HUB": "dtm-work",
            "SOURCE_CUTOVER": CUTOVER,
            "SOURCE_PG_SYSTEM": FIXTURE_SYSID,
            "SOURCE_REDIS_REPLID": FIXTURE_REPLID,
            "MAX_ONLY_IN_REDIS": 100,
        }
        patch = mock.patch.multiple(hx, **{**values, **override})
        patch.start()
        self.addCleanup(patch.stop)


class WorkConfigTests(SettingsCase):
    def test_no_config_keeps_the_asuspro13_deployment(self):
        """Protects: the existing ASUS service, which passes no --config.
        Detects: a default that moved off the asuspro13 stores, tools or root.
        Needs: nothing. Breadcrumb: module defaults, README Work profile.
        """
        self.assertEqual(hx.PSQL[hx.PSQL.index("-p") + 1], "5300")
        self.assertEqual(
            hx.DRIVE_CONFIG, Path("/srv/vigil-migration/asuspro13/rclone.conf")
        )
        self.assertEqual(hx.RCLONE, "/usr/bin/rclone")
        self.assertEqual(hx.AGE, "/usr/bin/age")
        self.assertEqual(hx.UNCACHED, {"mode": "none"})
        self.assertIsNone(hx.SOURCE_HUB)

    def test_config_rebinds_every_endpoint_tool_and_path(self):
        """Protects: running the same helper against the Work stores and tools.
        Detects: any asuspro13 value surviving a complete config. Needs: tmp file.
        Breadcrumb: configure.
        """
        hx.configure(self.write_config(work_config(self.tmp)))
        self.assertEqual(hx.PSQL[0], "psql-fixture")
        self.assertEqual(hx.PSQL[hx.PSQL.index("-p") + 1], "5432")
        self.assertEqual(hx.STATE, self.tmp / "state")
        self.assertEqual(hx.DRIVE_CONFIG, self.tmp / "rclone.conf")
        self.assertEqual(hx.DRIVE_RECIPIENT, self.tmp / "recipient.txt")
        self.assertEqual((hx.RCLONE, hx.AGE), ("rclone-fixture", "age-fixture"))
        self.assertEqual(hx.AGENT_BUS, "agent-bus-fixture")
        self.assertEqual(hx.REDIS_ADDRESS, ("127.0.0.1", 6380, 0))
        self.assertEqual(hx.SOURCE_HUB, "dtm-work")
        self.assertEqual(hx.DRIVE_FOLDER, "1mmL8o1upp4ty5zuaghn0YOfv40vsHzyt")

    def test_config_must_bind_hub_cutover_and_pg_generation(self):
        """Protects: a Work export that cannot be told from stale ASUS data.
        Detects: a config accepted without the hub identity, the cutover watermark,
        or the PostgreSQL system identifier; and a refused config leaving any
        setting half applied. Needs: tmp files.
        Breadcrumb: configure required keys.
        """
        for path, key in (
            ((), "hub_identity"),
            ((), "cutover_utc"),
            (("postgres",), "system_identifier"),
        ):
            with self.subTest(missing=key):
                config = work_config(self.tmp)
                parent = config["source"]
                for step in path:
                    parent = parent[step]
                del parent[key]
                before = {n: getattr(hx, n) for n in hx.SETTING_NAMES}
                with self.assertRaisesRegex(hx.ConfigError, key):
                    hx.configure(self.write_config(config))
                self.assertEqual(before, {n: getattr(hx, n) for n in hx.SETTING_NAMES})

    def test_config_never_inherits_an_asuspro13_path(self):
        """Protects: the rclone config path being a configuration value.
        Detects: an omitted path or endpoint silently using the ASUS one.
        Needs: tmp files. Breadcrumb: configure required keys.
        """
        for key in ("rclone_config", "recipient_file", "rclone", "age"):
            with self.subTest(missing=key):
                config = work_config(self.tmp)
                del config["drive"][key]
                with self.assertRaisesRegex(hx.ConfigError, key):
                    hx.configure(self.write_config(config))
        config = work_config(self.tmp)
        del config["state_root"]
        with self.assertRaisesRegex(hx.ConfigError, "state_root"):
            hx.configure(self.write_config(config))

    def test_config_cannot_redirect_the_fixed_destination(self):
        """Protects: the operator-qualified private folder, owner and recipient.
        Detects: a config naming another folder, owner account or recipient hash.
        Needs: tmp files. Breadcrumb: configure fixed values.
        """
        for key, value in (
            ("folder_id", "other-folder"),
            ("recipient_sha256", "0" * 64),
            ("owner_sha256", "0" * 64),
            ("owner_account", "someone-else@example.invalid"),
        ):
            with self.subTest(key=key):
                config = work_config(self.tmp)
                config["drive"][key] = value
                with self.assertRaises(hx.ConfigError):
                    hx.configure(self.write_config(config))

    def test_config_refuses_unknown_keys_and_wrong_types(self):
        """Protects: a typo not silently leaving a binding unset. Detects: an unknown
        key, a string port or a boolean number being accepted. Needs: tmp files.
        Breadcrumb: config_section.
        """
        config = work_config(self.tmp)
        config["source"]["hub_idnetity"] = "x"
        with self.assertRaisesRegex(hx.ConfigError, "unknown config key"):
            hx.configure(self.write_config(config))
        config = work_config(self.tmp)
        config["source"]["postgres"]["port"] = "5432"
        with self.assertRaisesRegex(hx.ConfigError, "wrong type"):
            hx.configure(self.write_config(config))
        config = work_config(self.tmp)
        config["source"]["redis"]["port"] = True
        with self.assertRaisesRegex(hx.ConfigError, "wrong type"):
            hx.configure(self.write_config(config))

    def test_explicit_missing_config_refuses_without_starting_an_export(self):
        """Protects: a Work unit never falling back to the asuspro13 defaults.
        Detects: --config naming an absent file starting an export anyway, and
        --config being accepted for the legacy --dest mode. Needs: tmp path only.
        Breadcrumb: main --config.
        """
        with mock.patch.object(hx, "drive_backup") as backup:
            code = hx.main(["--drive", "--config", str(self.tmp / "absent.json")])
        self.assertEqual(code, 2)
        backup.assert_not_called()
        with self.assertRaises(SystemExit):
            hx.main(["--dest", "host", "--config", str(self.tmp / "absent.json")])


class SourceBindingTests(SettingsCase):
    def setUp(self):
        super().setUp()
        self.bind()

    def check(self, before=None, after=None, meta=None, recon=None):
        return hx.check_source(
            before or reading(),
            after or reading(),
            meta or source_meta(),
            recon or source_recon(),
        )

    def test_matching_source_is_bound_and_recorded(self):
        """Protects: a manifest that names the hub and stores it was read from.
        Detects: identity, generations or the store difference missing from the
        recorded binding. Needs: synthetic readings. Breadcrumb: check_source.
        """
        binding = self.check()
        self.assertEqual(binding["hub_identity"], "dtm-work")
        self.assertEqual(binding["pg"]["system_identifier"], FIXTURE_SYSID)
        self.assertEqual(binding["redis"]["replid"], FIXTURE_REPLID)
        self.assertEqual(binding["store_difference"]["only_in_pg"], 1)

    def test_wrong_hub_identity_is_held(self):
        """Protects: Work history never being exported from another hub.
        Detects: an asuspro13 hub reading accepted before or after the export.
        Needs: synthetic readings. Breadcrumb: check_hub.
        """
        for field in ("before", "after"):
            with (
                self.subTest(reading=field),
                self.assertRaisesRegex(hx.DriveHold, "hub_identity_mismatch"),
            ):
                self.check(**{field: reading(hub_identity="asuspro13")})

    def test_non_authoritative_or_stopped_hub_is_held(self):
        """Protects: only the promoted authoritative running hub being exported.
        Detects: a fallback hub, a hub in maintenance or a state change mid-export.
        Needs: synthetic readings. Breadcrumb: check_hub, check_source.
        """
        with self.assertRaisesRegex(hx.DriveHold, "hub_not_authoritative"):
            self.check(after=reading(backend={"authoritative": False}))
        with self.assertRaisesRegex(hx.DriveHold, "hub_not_running"):
            self.check(before=reading(maintenance={"mode": "draining"}))
        changed = reading()
        changed["maintenance"] = {**changed["maintenance"], "changed_at_utc": "later"}
        with self.assertRaisesRegex(hx.DriveHold, "hub_state_changed_during_export"):
            self.check(after=changed)

    def test_data_older_than_the_cutover_is_held(self):
        """Protects: stale ASUS history never being reported as Work history.
        Detects: a snapshot, newest PG message or newest Redis entry that predates
        the cutover being accepted. Needs: synthetic metadata.
        Breadcrumb: check_source watermarks.
        """
        old = "2026-11-04T07:00:00.000000Z"
        with self.assertRaisesRegex(hx.DriveHold, "snapshot_before_cutover"):
            self.check(meta=source_meta(pg_snapshot_utc=old))
        with self.assertRaisesRegex(hx.DriveHold, "source_before_cutover"):
            self.check(meta=source_meta(pg_max_message_utc=old))
        cutover_ms = (
            datetime(2026, 11, 4, 7, 29, tzinfo=timezone.utc).timestamp() * 1000
        )
        stale_id = f"{int(cutover_ms) - 1000}-0"
        with self.assertRaisesRegex(hx.DriveHold, "redis_before_cutover"):
            self.check(recon=source_recon(redis_last_stream_id=stale_id))

    def test_other_postgres_or_redis_generation_is_held(self):
        """Protects: binding to the PG cluster and Redis dataset actually read.
        Detects: another PG system identifier, a different Redis replication id, or
        a Redis restart during the read being accepted. Needs: synthetic metadata.
        Breadcrumb: check_source generation checks.
        """
        with self.assertRaisesRegex(hx.DriveHold, "pg_generation_mismatch"):
            self.check(meta=source_meta(pg_system_identifier="1"))
        other = source_recon()
        other["redis_generation"] = {
            **other["redis_generation"],
            "replid": "x" * 40,
            "replid_after": "x" * 40,
        }
        with self.assertRaisesRegex(hx.DriveHold, "redis_generation_mismatch"):
            self.check(recon=other)
        restarted = source_recon()
        restarted["redis_generation"] = {
            **restarted["redis_generation"],
            "run_id_after": "b" * 40,
        }
        with self.assertRaisesRegex(hx.DriveHold, "redis_restarted_during_export"):
            self.check(recon=restarted)

    def test_local_stream_length_is_compared_only_with_the_hub_stream_length(self):
        """Protects: Redis being the hub's own stream. Detects: a local stream of 4
        entries accepted because the hub reports 90000 (the two kinds of count were
        mixed into one window). Needs: synthetic readings.
        Breadcrumb: check_stream_window.
        """
        hub = reading(stream_length=90000)
        with self.assertRaisesRegex(hx.DriveHold, "redis_stream_window_mismatch"):
            self.check(before=hub, after=hub)
        near = reading(stream_length=4 + hx.STREAM_WINDOW_TOLERANCE)
        self.check(before=near, after=near)
        far = reading(stream_length=5 + hx.STREAM_WINDOW_TOLERANCE)
        with self.assertRaisesRegex(hx.DriveHold, "redis_stream_window_mismatch"):
            self.check(before=far, after=far)

    def test_redis_replication_id_is_recorded_and_enforced_only_when_pinned(self):
        """Protects: not holding every backup on an unstable identifier. Detects: the
        replication id (observed to change on dtm-work with the same run id and no
        restart) being enforced without a pin, or ignored with one. Needs:
        synthetic metadata. Breadcrumb: check_source.
        """
        moved = source_recon()
        moved["redis_generation"] = {
            **moved["redis_generation"],
            "replid": "x" * 40,
            "replid_after": "x" * 40,
        }
        with mock.patch.object(hx, "SOURCE_REDIS_REPLID", None):
            binding = self.check(recon=moved)
        self.assertEqual(binding["redis"]["replid"], "x" * 40)
        with self.assertRaisesRegex(hx.DriveHold, "redis_generation_mismatch"):
            self.check(recon=moved)

    def test_redis_that_is_not_the_hubs_stream_is_held(self):
        """Protects: Redis being the hub's own store, not a stale copy.
        Detects: a Redis stream whose length disagrees with the hub's reported
        stream_length. Needs: synthetic readings. Breadcrumb: check_source window.
        """
        with self.assertRaisesRegex(hx.DriveHold, "redis_stream_window_mismatch"):
            self.check(
                before=reading(stream_length=90000),
                after=reading(stream_length=90000),
                recon=source_recon(redis_stream_xlen_before_after=[90000, 90000]),
            )

    def test_unbound_profile_records_but_does_not_enforce(self):
        """Protects: the unchanged asuspro13 deployment, which has no configured
        identity. Detects: the new rules holding an unbound ASUS export, or the
        observed identity not being recorded. Needs: synthetic readings.
        Breadcrumb: check_source with SOURCE_HUB unset.
        """
        legacy = reading(hub_identity="asuspro13")
        with mock.patch.multiple(hx, SOURCE_HUB=None, SOURCE_CUTOVER=None):
            binding = self.check(before=legacy, after=legacy)
        self.assertEqual(binding["hub_identity"], "asuspro13")
        self.assertIsNone(binding["hub_identity_required"])


class StoreDifferenceTests(SettingsCase):
    def test_known_postgres_redis_difference_is_reported_not_absorbed(self):
        """Protects: the 95,949 vs 95,889 PG/stream difference being visible.
        Detects: the difference or its by-id split being dropped from the report.
        Needs: synthetic counts. Breadcrumb: store_difference.
        """
        report = hx.store_difference(
            source_recon(
                pg_lines=95949,
                pg_rows=95949,
                redis_stream_entries=95889,
                redis_unique_ids=95889,
                only_in_pg=60,
                only_in_redis=0,
            )
        )
        self.assertEqual(report["pg_minus_stream"], 60)
        self.assertEqual(report["only_in_pg"], 60)
        self.assertEqual(report["in_both"], 95889)
        self.assertTrue(report["accounted"])

    def test_duplicate_or_missing_ids_cannot_explain_a_difference(self):
        """Protects: counts that add up only because ids repeat or are absent.
        Detects: duplicate PG ids, duplicate or missing Redis ids and a split that
        does not sum being accepted. Needs: synthetic counts.
        Breadcrumb: store_difference.
        """
        with self.assertRaisesRegex(hx.DriveHold, "pg_message_ids_not_unique"):
            hx.store_difference(source_recon(pg_lines=6))
        with self.assertRaisesRegex(hx.DriveHold, "redis_stream_ids_not_unique"):
            hx.store_difference(source_recon(redis_unique_ids=3))
        with self.assertRaisesRegex(hx.DriveHold, "redis_stream_ids_not_unique"):
            hx.store_difference(source_recon(redis_entries_without_id=1))
        with self.assertRaisesRegex(hx.DriveHold, "store_difference_unaccounted"):
            hx.store_difference(source_recon(only_in_pg=2))
        with self.assertRaisesRegex(hx.DriveHold, "stores_share_no_messages"):
            hx.store_difference(
                source_recon(
                    pg_rows=5,
                    pg_lines=5,
                    redis_stream_entries=3,
                    redis_unique_ids=3,
                    only_in_pg=5,
                    only_in_redis=3,
                )
            )

    def test_redis_messages_missing_from_postgres_are_capped(self):
        """Protects: PostgreSQL staying the durable record. Detects: Redis holding
        far more messages than PG (a broken write-behind) passing as a backup.
        Needs: synthetic counts. Breadcrumb: store_difference MAX_ONLY_IN_REDIS.
        """
        self.bind(MAX_ONLY_IN_REDIS=0)
        recon = source_recon(
            redis_stream_entries=6, redis_unique_ids=6, only_in_redis=2, only_in_pg=1
        )
        with self.assertRaisesRegex(hx.DriveHold, "redis_ahead_of_pg"):
            hx.store_difference(recon)


def claims_for(n_messages: int, redis_ids):
    """The reconciliation and store difference the rows themselves support."""
    pg = {f"m{i}" for i in range(n_messages)}
    redis = set(redis_ids)
    recon = {
        "redis_stream_entries": len(redis_ids),
        "pg_rows": n_messages,
        "only_in_pg": len(pg - redis),
        "only_in_redis": len(redis - pg),
        "uuid_union": len(pg | redis),
    }
    diff = {
        "pg_messages": n_messages,
        "redis_stream_entries": len(redis_ids),
        "pg_minus_stream": n_messages - len(redis_ids),
        "only_in_pg": len(pg - redis),
        "only_in_redis": len(redis - pg),
        "in_both": n_messages - len(pg - redis),
        "accounted": True,
    }
    return recon, diff


def bound_bundle(
    tmp: Path,
    drop=(),
    max_message=None,
    snapshot=None,
    binding=None,
    redis_ids=("m0",),
    recon=None,
    diff=None,
):
    """A bundle whose snapshot.tsv and MANIFEST carry a source binding."""
    meta = source_meta()
    if max_message:
        meta["pg_max_message_utc"] = max_message
    rows = {
        "snapshot_utc": meta["pg_snapshot_utc"],
        "messages": 5,
        "presence_events": 1,
        "max_message_utc": meta["pg_max_message_utc"],
        "pg_system_identifier": meta["pg_system_identifier"],
        "pg_database_oid": meta["pg_database_oid"],
        "pg_server_version": meta["pg_server_version"],
        **(snapshot or {}),
    }
    for name in drop:
        del rows[name]
    raw, bundle = tmp / "raw", tmp / "bundle"
    make_raw(raw, redis_ids=redis_ids)
    (raw / "snapshot.tsv").write_text("".join(f"{k}|{v}\n" for k, v in rows.items()))
    honest_recon, honest_diff = claims_for(5, redis_ids)
    recon = {**honest_recon, **(recon or {})}
    (raw / "reconciliation.json").write_text(json.dumps(recon) + "\n")
    meta["reconciliation"] = recon
    meta["source_binding"] = {
        "schema": "agent-hub.history-source-binding/v1",
        "hub_identity": "dtm-work",
        "hub_identity_required": "dtm-work",
        "cutover_utc": CUTOVER,
        "pg": {
            "system_identifier": meta["pg_system_identifier"],
            "database_oid": meta["pg_database_oid"],
            "server_version": meta["pg_server_version"],
            "max_message_utc": meta["pg_max_message_utc"],
        },
        "store_difference": {**honest_diff, **(diff or {})},
        **(binding or {}),
    }
    hx.build_bundle(raw, bundle, meta)
    return bundle


class BoundBundleTests(SettingsCase):
    def test_bound_bundle_validates(self):
        """Protects: the bound snapshot/manifest format being restorable.
        Detects: a complete bound bundle being refused. Needs: synthetic bundle.
        Breadcrumb: validate_drive_bundle, validate_source_binding.
        """
        manifest = hx.validate_drive_bundle(bound_bundle(self.tmp))
        self.assertEqual(manifest["source_binding"]["hub_identity"], "dtm-work")

    def test_bound_bundle_needs_every_identity_row(self):
        """Protects: snapshot.tsv carrying the PG generation it was read from.
        Detects: a bound manifest whose snapshot.tsv lacks an identity row.
        Needs: synthetic bundle. Breadcrumb: validate_drive_bundle field set.
        """
        for name in hx.SNAPSHOT_SOURCE_FIELDS:
            with self.subTest(drop=name):
                shutil.rmtree(self.tmp / "raw", ignore_errors=True)
                shutil.rmtree(self.tmp / "bundle", ignore_errors=True)
                with self.assertRaisesRegex(
                    hx.DriveHold, "export_snapshot_fields_invalid"
                ):
                    hx.validate_drive_bundle(bound_bundle(self.tmp, drop=(name,)))

    def test_snapshot_rows_cannot_disagree_with_the_manifest_binding(self):
        """Protects: one PG generation across snapshot.tsv, manifest and binding.
        Detects: a snapshot row naming another cluster than the manifest. Needs:
        synthetic bundle. Breadcrumb: validate_source_binding.
        """
        bundle = bound_bundle(self.tmp, snapshot={"pg_system_identifier": "9"})
        with self.assertRaisesRegex(hx.DriveHold, "source_binding_snapshot_mismatch"):
            hx.validate_drive_bundle(bundle)

    def test_restore_validation_reproves_freshness_and_identity(self):
        """Protects: the restore qualifier re-checking what the exporter checked.
        Detects: a bundle whose newest message predates the cutover, one naming
        another hub, or one with an unaccounted store difference validating.
        Needs: synthetic bundles. Breadcrumb: validate_source_binding.
        """
        cases = {
            "source_before_cutover": {"max_message": "2026-11-04T07:00:00.000000Z"},
            "hub_identity_mismatch": {"binding": {"hub_identity": "asuspro13"}},
            "source_binding_invalid": {"diff": {"accounted": False}},
        }
        for reason, kwargs in cases.items():
            with self.subTest(reason):
                shutil.rmtree(self.tmp / "raw", ignore_errors=True)
                shutil.rmtree(self.tmp / "bundle", ignore_errors=True)
                with self.assertRaisesRegex(hx.DriveHold, reason):
                    hx.validate_drive_bundle(bound_bundle(self.tmp, **kwargs))

    def test_package_must_hold_every_row_and_redis_only_id_it_claims(self):
        """Protects: the only copy of a message not being lost. Detects: a package
        claiming rows or Redis-only ids its parts do not contain (a claimed
        Redis-only id absent from the Redis part, more PG rows than present, an
        unbalanced reconciliation). Needs: synthetic bundles.
        Breadcrumb: validate_package_difference, package_reconciliation.
        """
        honest = bound_bundle(self.tmp, redis_ids=("m0", "x1"))
        manifest = hx.validate_drive_bundle(honest)
        self.assertEqual(
            manifest["source_binding"]["store_difference"]["only_in_redis"], 1
        )
        cases = {
            "redis-only id claimed but not in the Redis part": (
                {"redis_ids": ("m0",), "diff": {"only_in_redis": 1}},
                "store_difference_mismatch",
            ),
            "package holds 5 rows but claims 6": (
                {"diff": {"pg_messages": 6, "pg_minus_stream": 5, "in_both": 1}},
                "store_difference_mismatch",
            ),
            "reconciliation claims a Redis-only id that is absent": (
                {"recon": {"only_in_redis": 1, "uuid_union": 6}},
                "reconciliation_mismatch",
            ),
            "reconciliation claims fewer PG-only ids than the rows hold": (
                {"recon": {"only_in_pg": 3}},
                "reconciliation_mismatch",
            ),
        }
        for label, (kwargs, reason) in cases.items():
            with self.subTest(label):
                shutil.rmtree(self.tmp / "raw", ignore_errors=True)
                shutil.rmtree(self.tmp / "bundle", ignore_errors=True)
                with self.assertRaisesRegex(hx.DriveHold, reason):
                    hx.validate_drive_bundle(bound_bundle(self.tmp, **kwargs))

    def test_package_with_repeated_redis_ids_is_refused(self):
        """Protects: counts that equal claims only because ids repeat. Detects:
        duplicate Redis ids passing validation. Needs: a synthetic bundle.
        Breadcrumb: validate_package_difference.
        """
        with self.assertRaisesRegex(hx.DriveHold, "redis_stream_ids_not_unique"):
            hx.validate_drive_bundle(bound_bundle(self.tmp, redis_ids=("m0", "m0")))

    def test_legacy_bundle_without_a_binding_still_validates(self):
        """Protects: bundles written before the binding (the asuspro13 history).
        Detects: the three-row snapshot being refused when no binding is recorded.
        Needs: synthetic bundle. Breadcrumb: validate_drive_bundle.
        """
        raw, bundle = self.tmp / "raw", self.tmp / "bundle"
        make_raw(raw)
        hx.build_bundle(
            raw,
            bundle,
            {
                "pg_messages": 5,
                "pg_presence_events": 1,
                "pg_snapshot_utc": "2026-10-29T00:00:00Z",
            },
        )
        self.assertNotIn("source_binding", hx.validate_drive_bundle(bundle))


class FakeRedis:
    """Just enough redis-py for export_redis: no side channels, one stream."""

    def __init__(self, entries, generation=None):
        self.entries = entries
        self.generation = generation or {"run_id": "a" * 40, "master_replid": "r" * 40}

    def scan_iter(self, count=0):
        return iter(())

    def info(self, section):
        if section == "server":
            return {
                "run_id": self.generation["run_id"],
                "redis_version": "8.6.1",
                "tcp_port": 6380,
            }
        return {"master_replid": self.generation["master_replid"]}

    def xlen(self, name):
        return len(self.entries)

    def xrange(self, name, min="-", max="+", count=None):
        start = 0 if min == "-" else [i for i, _ in self.entries].index(min.encode())
        return self.entries[start : start + (count or len(self.entries))]


class ExportEvidenceTests(SettingsCase):
    def test_pg_snapshot_reads_identity_inside_the_one_transaction(self):
        """Protects: the PG generation and watermark coming from the same
        REPEATABLE READ snapshot as the rows. Detects: identity or the newest
        message read outside the READ ONLY transaction, or not read at all.
        Needs: a stand-in psql. Breadcrumb: PG_SQL, export_pg.
        """
        sql = hx.PG_SQL
        begin = sql.index("BEGIN ISOLATION LEVEL REPEATABLE READ READ ONLY")
        end = sql.index("COMMIT")
        for needle in ("pg_control_system()", "max(timestamp_utc)", "pg_database"):
            self.assertTrue(begin < sql.index(needle) < end, needle)
        raw = self.tmp / "raw"
        raw.mkdir()

        def psql(args, cwd, check):
            (Path(cwd) / "snapshot.tsv").write_text(
                "snapshot_utc|2026-11-04T15:00:00.000000Z\nmessages|5\n"
                "presence_events|1\nmax_message_utc|2026-11-04T14:59:00.000000Z\n"
                "pg_system_identifier|7617335056550016368\npg_database_oid|18653\n"
                "pg_server_version|18.3\n"
            )

        with mock.patch.object(hx.subprocess, "run", side_effect=psql):
            got = hx.export_pg(raw)
        self.assertEqual(got["pg_system_identifier"], "7617335056550016368")
        self.assertEqual(got["pg_max_message_utc"], "2026-11-04T14:59:00.000000Z")
        self.assertEqual(got["pg_messages"], 5)

    def test_redis_export_records_generation_and_id_accounting(self):
        """Protects: the Redis instance and the id accounting behind the store
        difference. Detects: a missing run id or replication id, an entry count that
        hides a duplicated id, or a stream tail that is not recorded.
        Needs: a fake redis client and synthetic NDJSON. Breadcrumb: export_redis.
        """
        raw = self.tmp / "raw"
        raw.mkdir()
        (raw / "messages.ndjson").write_text(
            "".join(json.dumps({"id": f"u{i}"}) + "\n" for i in range(5))
        )
        stream = [
            (b"1-0", {b"id": b"u0"}),
            (b"2-0", {b"id": b"u1"}),
            (b"3-0", {b"id": b"u1"}),
            (b"4-0", {b"id": b"u9"}),
        ]
        recon = hx.export_redis(raw, FakeRedis(stream))
        self.assertEqual(recon["redis_stream_entries"], 4)
        self.assertEqual(recon["redis_unique_ids"], 3)
        self.assertEqual(recon["redis_last_stream_id"], "4-0")
        self.assertEqual(recon["redis_stream_xlen_before_after"], [4, 4])
        self.assertEqual(recon["redis_generation"]["replid"], "r" * 40)
        self.assertEqual(recon["redis_generation"]["run_id_after"], "a" * 40)
        self.assertEqual((recon["only_in_redis"], recon["only_in_pg"]), (1, 3))
        with self.assertRaisesRegex(hx.DriveHold, "redis_stream_ids_not_unique"):
            hx.store_difference(recon)


class WorkCaptureTests(SettingsCase):
    """drive_backup end to end against synthetic stores, bus and provider."""

    def setUp(self):
        super().setUp()
        self.bind()
        self.state = self.tmp / "state"
        recipient = self.tmp / "recipient.txt"
        recipient.write_text("fixture-public-recipient\n")
        patches = [
            mock.patch.object(hx, "STATE", self.state),
            mock.patch.object(hx, "DRIVE_RESERVE", 0),
            mock.patch.object(hx, "DRIVE_CONFIG", self.tmp / "rclone.conf"),
            mock.patch.object(hx, "DRIVE_OWNER", FIXTURE_OWNER),
            mock.patch.object(hx, "DRIVE_RECIPIENT", recipient),
            mock.patch.object(hx, "DRIVE_RECIPIENT_SHA", hx.digest(recipient)[0]),
            mock.patch.object(hx, "export_pg", side_effect=self.export_pg),
            mock.patch.object(hx, "export_redis", side_effect=self.export_redis),
            mock.patch.dict(sys.modules, {"redis": mock.MagicMock()}),
        ]
        for patch in patches:
            patch.start()
            self.addCleanup(patch.stop)
        self.meta = source_meta()
        self.health = mock.patch.object(hx, "health", side_effect=lambda: reading())
        self.health.start()
        self.addCleanup(self.health.stop)
        self.bus = mock.patch.object(hx, "bus")
        self.notice = self.bus.start()
        self.addCleanup(self.bus.stop)
        self.age_calls = []

    def export_pg(self, raw):
        ids = ["m0", "m1", "m2", "m3"]
        make_raw(raw, redis_ids=ids)
        (raw / "reconciliation.json").write_text(
            json.dumps(claims_for(5, ids)[0]) + "\n"
        )
        rows = {
            "snapshot_utc": self.meta["pg_snapshot_utc"],
            "messages": 5,
            "presence_events": 1,
            "max_message_utc": self.meta["pg_max_message_utc"],
            "pg_system_identifier": self.meta["pg_system_identifier"],
            "pg_database_oid": self.meta["pg_database_oid"],
            "pg_server_version": self.meta["pg_server_version"],
        }
        (raw / "snapshot.tsv").write_text(
            "".join(f"{k}|{v}\n" for k, v in rows.items())
        )
        return dict(self.meta)

    def export_redis(self, raw, client):
        return source_recon()

    def encrypt(self, args, timeout=120):
        self.age_calls.append(args[0])
        Path(args[args.index("--output") + 1]).write_bytes(
            b"cipher:" + Path(args[-1]).read_bytes()
        )
        return b""

    def last(self):
        return json.loads((self.state / "last-drive-result.json").read_text())

    def spool(self):
        return sorted(p.name for p in (self.state / "drive-spool").iterdir())

    def provider(self):
        store = mock.Mock()
        store.put.side_effect = lambda path, name, expected: fixture_proof(
            name, expected
        )
        return store

    def test_wrong_hub_is_refused_before_any_spool_entry_exists(self):
        """Protects: Work history never being captured from another hub, and an
        unattended retry not being blocked by it. Detects: an asuspro13 reading
        starting an export or leaving a pending spool entry. Needs: synthetic
        health only. Breadcrumb: drive_backup, check_hub.
        """
        self.health.stop()
        with mock.patch.object(
            hx, "health", return_value=reading(hub_identity="asuspro13")
        ):
            self.assertEqual(hx.drive_backup(no_bus=True), 1)
        self.assertEqual(self.last()["reason"], "hub_identity_mismatch")
        self.assertEqual(self.spool(), [])
        hx.export_pg.assert_not_called()

    def test_health_window_failure_is_a_recorded_hold_not_a_crash(self):
        """Protects: a failed snapshot control leaving a durable HELD result and a
        bus finding. Detects: check_window's SystemExit escaping the Drive path
        with nothing recorded. Needs: synthetic stores. Breadcrumb: drive_backup.
        """
        self.health.stop()
        with mock.patch.object(
            hx, "health", return_value=reading(pg_message_count=100)
        ):
            self.assertEqual(hx.drive_backup(), 1)
        self.assertEqual(self.last()["reason"], "snapshot_outside_health_window")
        self.assertTrue(self.notice.call_args.args[0].startswith("FINDING:"))

    def test_stale_capture_is_quarantined_and_the_next_run_proceeds(self):
        """Protects: stale ASUS data never becoming an upload candidate, and one
        rejected capture not blocking every later run. Detects: a newest message
        before the cutover being closed, or its spool entry blocking the retry.
        Needs: synthetic stores and provider. Breadcrumb: quarantine, drive_backup.
        """
        self.meta = source_meta(pg_max_message_utc="2026-11-04T07:00:00.000000Z")
        self.assertEqual(hx.drive_backup(no_bus=True), 1)
        self.assertEqual(self.last()["reason"], "source_before_cutover")
        (rejected,) = self.spool()
        self.assertTrue(rejected.startswith("rejected-"))
        marker = json.loads(
            (self.state / "drive-spool" / rejected / "REJECTED.json").read_text()
        )
        self.assertFalse(marker["uploaded"])
        self.assertEqual(self.age_calls, [])
        self.meta = source_meta()
        with (
            mock.patch.object(hx, "drive_run", side_effect=self.encrypt),
            mock.patch.object(hx, "DriveStore", return_value=self.provider()),
        ):
            self.assertEqual(hx.drive_backup(no_bus=True), 0)
        self.assertEqual(self.last()["status"], "REMOTE_VERIFIED")
        self.assertEqual(len(self.spool()), 2)

    def test_unprovisioned_remote_holds_after_closing_and_never_verifies(self):
        """Protects: the owner-pending Drive access outcome. Detects: an absent
        rclone config being reported as success, or as a generic metadata error,
        or any rclone call being made, or the closed cipher not being kept.
        Needs: synthetic stores, no rclone config. Breadcrumb: DriveStore.provisioned,
        drive_backup.
        """
        with mock.patch.object(hx, "drive_run", side_effect=self.encrypt):
            self.assertEqual(hx.drive_backup(), 1)
        result = self.last()
        self.assertEqual(result, {**result, "status": "HELD"})
        self.assertEqual(result["reason"], "remote_not_provisioned")
        self.assertEqual(set(self.age_calls), {hx.AGE})
        (job,) = self.spool()
        files = {p.name for p in (self.state / "drive-spool" / job).iterdir()}
        self.assertIn("binding.json", files)
        self.assertIn("archive.tar", files)
        self.assertTrue(any(f.endswith(".age") for f in files))
        self.assertNotIn("verified.json", files)
        self.assertNotIn("cloud-receipt.json", files)
        self.assertFalse((self.state / "receipts").exists())
        self.assertTrue(self.notice.call_args.args[0].startswith("FINDING:"))
        self.assertIn("remote_not_provisioned", self.notice.call_args.args[0])

    def closed_job_in_spool(self, bundle_kwargs=None, skip_validator=False):
        """Close a bundle (as an older revision would) and park it in the spool."""
        bundle = bound_bundle(self.tmp / "b", **(bundle_kwargs or {}))
        name = "history-" + hx.digest(bundle / "SHA256SUMS")[0][:16]
        job = self.state / "drive-spool" / name
        job.mkdir(parents=True)
        shutil.move(str(bundle), job / "bundle")
        with (
            mock.patch.object(hx, "check_job_source"),
            mock.patch.object(hx, "drive_run", side_effect=self.encrypt),
        ):
            if skip_validator:
                with mock.patch.object(hx, "validate_package_difference"):
                    hx.close_drive_job(job, name)
            else:
                hx.close_drive_job(job, name)
        return job, name

    def test_resume_revalidates_a_job_closed_by_an_older_revision(self):
        """Protects: a retained closed job never uploading on hashes alone. Detects:
        a job closed before the package checks existed, whose claims its rows do
        not support, being uploaded once a config and remote appear.
        Needs: synthetic job and provider. Breadcrumb: close_drive_job resume branch.
        """
        job, _ = self.closed_job_in_spool(
            {"redis_ids": ("m0",), "diff": {"only_in_redis": 1}}, skip_validator=True
        )
        store = self.provider()
        with mock.patch.object(hx, "DriveStore", return_value=store):
            self.assertEqual(hx.drive_backup(no_bus=True), 1)
        self.assertEqual(self.last()["reason"], "store_difference_mismatch")
        store.put.assert_not_called()
        self.assertFalse((job / "verified.json").exists())

    def test_resume_refuses_a_job_not_bound_to_the_configured_source(self):
        """Protects: a job closed for another source not uploading just because a
        Work config appeared. Detects: an other-hub, other-cutover or other-cluster
        job being published. Needs: synthetic jobs and provider.
        Breadcrumb: check_job_source.
        """
        for label, binding in (
            (
                "other hub",
                {"hub_identity": "asuspro13", "hub_identity_required": "asuspro13"},
            ),
            ("other cutover", {"cutover_utc": "2026-01-01T00:00:00Z"}),
        ):
            with self.subTest(label):
                shutil.rmtree(self.tmp / "b", ignore_errors=True)
                shutil.rmtree(self.state, ignore_errors=True)
                self.closed_job_in_spool({"binding": binding})
                store = self.provider()
                with mock.patch.object(hx, "DriveStore", return_value=store):
                    self.assertEqual(hx.drive_backup(no_bus=True), 1)
                self.assertEqual(
                    self.last()["reason"], "job_not_bound_to_configured_source"
                )
                store.put.assert_not_called()
        with self.subTest("other cluster"):
            shutil.rmtree(self.tmp / "b", ignore_errors=True)
            shutil.rmtree(self.state, ignore_errors=True)
            self.closed_job_in_spool()
            store = self.provider()
            with (
                mock.patch.object(hx, "SOURCE_PG_SYSTEM", "1"),
                mock.patch.object(hx, "DriveStore", return_value=store),
            ):
                self.assertEqual(hx.drive_backup(no_bus=True), 1)
            self.assertEqual(
                self.last()["reason"], "job_not_bound_to_configured_source"
            )
            store.put.assert_not_called()

    def test_resume_refuses_a_legacy_unbound_job_under_a_work_config(self):
        """Protects: an asuspro13-era closed job not uploading under the Work
        config. Detects: a job without a source binding being published.
        Needs: a synthetic legacy job. Breadcrumb: check_job_source.
        """
        bundle, name = DriveTests.bundle(self)
        job = self.state / "drive-spool" / name
        job.mkdir(parents=True)
        shutil.move(str(bundle), job / "bundle")
        with (
            mock.patch.object(hx, "SOURCE_HUB", None),
            mock.patch.object(hx, "drive_run", side_effect=self.encrypt),
        ):
            hx.close_drive_job(job, name)
        store = self.provider()
        with mock.patch.object(hx, "DriveStore", return_value=store):
            self.assertEqual(hx.drive_backup(no_bus=True), 1)
        self.assertEqual(self.last()["reason"], "job_not_bound_to_configured_source")
        store.put.assert_not_called()

    def test_valid_closed_bound_job_still_resumes(self):
        """Protects: the retry path for a good job. Detects: the new revalidation
        refusing an honest, correctly bound closed job. Needs: synthetic job and
        provider. Breadcrumb: close_drive_job resume branch.
        """
        job, _ = self.closed_job_in_spool({"redis_ids": ("m0", "x1")})
        store = self.provider()
        with mock.patch.object(hx, "DriveStore", return_value=store):
            self.assertEqual(hx.drive_backup(no_bus=True), 0)
        self.assertTrue((job / "verified.json").exists())

    def test_provisioned_check_needs_a_real_config_with_a_token(self):
        """Protects: remote_not_provisioned meaning exactly "no usable remote".
        Detects: a missing, empty, symlinked, section-less or token-less rclone
        config passing as provisioned, or a complete one being refused. Needs:
        tmp files; no token value is read. Breadcrumb: DriveStore.provisioned.
        """
        config = self.tmp / "rclone.conf"
        with self.assertRaisesRegex(hx.DriveHold, "remote_not_provisioned"):
            hx.DriveStore.provisioned()
        config.write_text("")
        with self.assertRaisesRegex(hx.DriveHold, "remote_not_provisioned"):
            hx.DriveStore.provisioned()
        config.write_text("[other]\ntoken = x\n")
        with self.assertRaisesRegex(hx.DriveHold, "remote_not_provisioned"):
            hx.DriveStore.provisioned()
        config.write_text("[migration-drive]\ntype = drive\n")
        with self.assertRaisesRegex(hx.DriveHold, "remote_not_provisioned"):
            hx.DriveStore.provisioned()
        config.write_text("[migration-drive]\ntype = drive\ntoken = {}\n")
        hx.DriveStore.provisioned()
        link = self.tmp / "link.conf"
        try:
            link.symlink_to(config)
        except OSError:
            return
        with (
            mock.patch.object(hx, "DRIVE_CONFIG", link),
            self.assertRaisesRegex(hx.DriveHold, "remote_not_provisioned"),
        ):
            hx.DriveStore.provisioned()

    def test_work_profile_never_reads_the_asuspro13_qualification_file(self):
        """Protects: the Work host not depending on any ASUS file. Detects: the
        owner being read from /srv/vigil-migration instead of the config.
        Needs: tmp config. Breadcrumb: DriveStore.__init__.
        """
        (self.tmp / "rclone.conf").write_text("[migration-drive]\ntoken = {}\n")
        with mock.patch.object(hx, "DRIVE_QUALIFICATION", None):
            store = hx.DriveStore()
        self.assertEqual(store.owner, FIXTURE_OWNER)

    def test_verified_receipt_records_counts_freshness_and_pending_restore(self):
        """Protects: an honest off-site result. Detects: REMOTE_VERIFIED without the
        snapshot age, the store difference and cipher hash in the notice, a stale
        snapshot being called fresh, or a plaintext restore being claimed.
        Needs: synthetic stores and provider. Breadcrumb: publish_drive_job,
        snapshot_freshness, drive_counts.
        """
        with (
            mock.patch.object(hx, "drive_run", side_effect=self.encrypt),
            mock.patch.object(hx, "DriveStore", return_value=self.provider()),
        ):
            self.assertEqual(hx.drive_backup(), 0)
        (job,) = self.spool()
        verified = json.loads(
            (self.state / "drive-spool" / job / "verified.json").read_text()
        )
        self.assertEqual(
            verified["restore_qualification"], "PENDING_INDEPENDENT_DECRYPT"
        )
        self.assertTrue(verified["freshness"]["within_limit"])
        self.assertEqual(
            verified["binding"]["source_binding"]["store_difference"]["only_in_pg"], 1
        )
        notice = self.notice.call_args.args[0]
        for part in (
            "only_in_pg=1",
            "only_in_redis=0",
            "cipher_sha256=",
            "pg_messages=5",
        ):
            self.assertIn(part, notice)
        self.assertTrue(self.last()["fresh"])
        self.assertFalse(verified["source_private_age_key"])

    def test_stale_snapshot_at_upload_is_reported_not_called_fresh(self):
        """Protects: a delayed upload not being mistaken for a current backup, and
        not blocking the next capture either. Detects: a snapshot older than the
        limit being marked fresh or held forever. Needs: synthetic stores.
        Breadcrumb: snapshot_freshness, drive_counts.
        """
        with (
            mock.patch.object(hx, "MAX_SNAPSHOT_AGE", -1),
            mock.patch.object(hx, "drive_run", side_effect=self.encrypt),
            mock.patch.object(hx, "DriveStore", return_value=self.provider()),
        ):
            self.assertEqual(hx.drive_backup(), 0)
        self.assertFalse(self.last()["fresh"])
        self.assertIn(" STALE", self.notice.call_args.args[0])


class WorkPackagingTests(unittest.TestCase):
    def read(self, name: str) -> str:
        return (PKG / "systemd" / name).read_text()

    def test_work_service_passes_an_explicit_config_and_names_no_asus_path(self):
        """Protects: the Work unit never running on the asuspro13 defaults, and the
        asuspro13 unit staying byte-for-byte on them. Detects: --config missing from
        the Work ExecStart, an ASUS path in it, or --config added to the old unit.
        Needs: unit text only. Breadcrumb: README Work profile.
        """
        work = self.read("agent-hub-history-export-work.service")
        self.assertIn(
            '--drive --config "$HOME/.config/agent-hub-history-export/config.json"',
            work,
        )
        self.assertIn("OnFailure=agent-hub-history-export-notify@%n.service", work)
        self.assertIn('source "$HOME/.config/bash/env.bash" &&', work)
        self.assertIn("exec flock -n", work)
        self.assertIn(
            "agent-hub-history-export/current/agent-hub-history-export.py", work
        )
        for text in (work, self.read("agent-hub-history-export-work.timer")):
            self.assertNotIn("/srv/vigil-migration", text)
            self.assertNotIn("asuspro13", text)
        self.assertNotIn("--config", self.read("agent-hub-history-export.service"))

    def test_work_timer_targets_the_work_service_and_is_daily(self):
        """Protects: the staged timer starting the right service. Detects: a timer
        pointing at the asuspro13 service, or losing Persistent/daily.
        Needs: unit text only. Breadcrumb: systemd/agent-hub-history-export-work.timer.
        """
        timer = self.read("agent-hub-history-export-work.timer")
        self.assertIn("Unit=agent-hub-history-export-work.service", timer)
        self.assertIn("OnCalendar=daily", timer)
        self.assertIn("Persistent=true", timer)

    def test_uncached_reader_is_unbuffered_and_read_only(self):
        """Protects: the Windows-side uncached read. Detects: the reader dropping
        FILE_FLAG_NO_BUFFERING (a cached read proves nothing about stored bytes) or
        gaining any write, delete or network call. Needs: script text only.
        Breadcrumb: windows/Get-UncachedHash.ps1.
        """
        ps1 = (PKG / "windows" / "Get-UncachedHash.ps1").read_text()
        self.assertIn("0x20000000", ps1)
        self.assertIn("SHA256", ps1)
        self.assertIn("[IO.FileAccess]::Read", ps1)
        for forbidden in (
            "Set-Content",
            "Out-File",
            "Add-Content",
            "Remove-Item",
            "Invoke-WebRequest",
            "Invoke-RestMethod",
            "[IO.FileAccess]::Write",
        ):
            self.assertNotIn(forbidden, ps1)


if __name__ == "__main__":
    unittest.main()
