"""Regression tests for the agent-hub drop prune (Invoke-AgentHubPrune.ps1).

Protects: the history bundles on the dtm-* drop hosts. The prune is the only
thing that deletes there, so it must keep the newest N, keep every referenced
or pinned bundle, refuse to delete anything it cannot verify, and log each
removal with its hashes before deleting.
Needs: PowerShell (`pwsh`, or Windows `powershell`) and the history export
script, whose build_bundle makes real bundles. Without PowerShell the tests
skip and say so; the job itself runs on Windows PowerShell 5.1.
Breadcrumb: packaging/history-export/README.md "Retention"; CI's Unit Tests
step runs `python3 -B -m unittest discover -s scripts/ci -p "test_*.py"`.
"""

import importlib.util
import json
import os
import shutil
import subprocess
import tempfile
import time
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
PKG = ROOT / "packaging" / "history-export"
PRUNE = PKG / "windows" / "Invoke-AgentHubPrune.ps1"
POWERSHELL = shutil.which("pwsh") or shutil.which("powershell")


def load_export():
    spec = importlib.util.spec_from_file_location(
        "agent_hub_history_export", PKG / "agent-hub-history-export.py"
    )
    mod = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(mod)
    return mod


hx = load_export()


def code_lines(ps1: str) -> str:
    """The script without comment lines, so a check reads code, not its explanation."""
    return "\n".join(ln for ln in ps1.splitlines() if not ln.lstrip().startswith("#"))


@unittest.skipUnless(
    POWERSHELL, "PowerShell (pwsh) not installed; the prune is PowerShell"
)
class PruneTests(unittest.TestCase):
    def setUp(self):
        self.tmp = Path(tempfile.mkdtemp(prefix="history-prune-test-"))
        self.addCleanup(shutil.rmtree, self.tmp, True)
        self.art = self.tmp / "agent-hub-artifacts"
        self.root = self.art / "incoming"
        self.root.mkdir(parents=True)
        self.log = self.art / "prune-log" / "prune.ndjson"
        self.names = []

    def add_bundles(self, count: int) -> list[str]:
        """Build real bundles oldest first, each with a receipt, as the export does."""
        for i in range(count):
            raw = self.tmp / f"raw-{len(self.names)}"
            raw.mkdir()
            (raw / "messages.ndjson").write_text(
                json.dumps({"id": f"m{len(self.names)}"}) + "\n"
            )
            for fn in (
                "redis-stream-messages.ndjson",
                "presence-events.ndjson",
                "redis-side-channels.ndjson",
            ):
                (raw / fn).write_text("")
            (raw / "redis-side-channels.counts.json").write_text("{}\n")
            (raw / "reconciliation.json").write_text("{}\n")
            (raw / "snapshot.tsv").write_text(f"messages|{len(self.names)}\n")
            staging = self.tmp / f"stage-{len(self.names)}"
            name = hx.build_bundle(
                raw, staging, {"pg_snapshot_utc": f"2026-10-{i + 1:02d}"}
            )
            # Directory creation time orders the bundles; space them apart.
            time.sleep(0.02)
            shutil.move(str(staging), str(self.root / name))
            receipt = {
                "name": name,
                "path": f"C:\\codedev\\agent-hub-artifacts\\incoming\\{name}",
            }
            (self.root / f"{name}.receipt.json").write_text(json.dumps(receipt))
            self.names.append(name)
        return self.names

    def prune(self, keep: int, *extra: str, ok: bool = True) -> list[dict]:
        cmd = [
            POWERSHELL,
            "-NoLogo",
            "-NoProfile",
            "-NonInteractive",
            "-File",
            str(PRUNE),
            "-Root",
            str(self.root),
            "-ArtifactsRoot",
            str(self.art),
            "-LogPath",
            str(self.log),
            "-Keep",
            str(keep),
            *extra,
        ]
        res = subprocess.run(
            cmd, capture_output=True, text=True, timeout=300, check=False
        )
        if ok:
            self.assertEqual(res.returncode, 0, res.stdout + res.stderr)
        else:
            self.assertNotEqual(res.returncode, 0, res.stdout + res.stderr)
        return [json.loads(line) for line in self.log.read_text().splitlines()]

    def present(self) -> set[str]:
        return {p.name for p in self.root.iterdir() if p.is_dir()}

    def test_keeps_newest_and_logs_each_removal_with_hashes(self):
        """Protects: the keep-newest-N rule and the removal record. Detects: deleting a
        bundle inside the newest N, keeping one outside it, a removal without a
        sha256sums/bytes log line, or a removed bundle whose receipt is left behind.
        Needs: PowerShell. Breadcrumb: README "Retention"."""
        names = self.add_bundles(5)
        log = self.prune(3)
        self.assertEqual(self.present(), set(names[2:]))
        removed = {e["bundle"]: e for e in log if e["event"] == "remove"}
        self.assertEqual(set(removed), set(names[:2]))
        for name, entry in removed.items():
            self.assertEqual(entry["sha256sums"][:16], name.removeprefix("history-"))
            self.assertGreater(entry["bytes"], 0)
            self.assertTrue(entry["receipt_sha256"])
            self.assertFalse((self.root / f"{name}.receipt.json").exists())
        summary = log[-1]
        self.assertEqual(
            (summary["event"], summary["removed"], summary["bundles"]),
            ("summary", 2, 5),
        )

    def test_referenced_and_pinned_bundles_are_kept(self):
        """Protects: bundles another artifact names. Detects: deleting a bundle that a
        different receipt or manifest references, or one listed in the KEEP pin file,
        and a bundle's own receipt wrongly counting as a reference.
        Needs: PowerShell. Breadcrumb: Invoke-AgentHubPrune.ps1 help text."""
        names = self.add_bundles(5)
        (self.art / "history").mkdir()
        (self.art / "history" / "MANIFEST.json").write_text(
            json.dumps({"supersedes": names[0]})
        )
        (self.root / "KEEP").write_text(names[1] + "\n")
        log = self.prune(2)
        self.assertEqual(self.present(), {names[0], names[1], names[3], names[4]})
        kept = {e["bundle"] for e in log if e["event"] == "kept-referenced"}
        self.assertEqual(kept, {names[0], names[1]})
        self.assertEqual(
            [e["bundle"] for e in log if e["event"] == "remove"], [names[2]]
        )

    def test_unverifiable_bundles_are_kept_as_anomalies(self):
        """Protects: damaged or planted bundles, which are evidence. Detects: deleting a
        bundle whose bytes no longer match SHA256SUMS, or one with an unlisted file,
        instead of keeping and logging it.
        Needs: PowerShell. Breadcrumb: Test-Bundle in Invoke-AgentHubPrune.ps1."""
        names = self.add_bundles(2)
        victim = self.root / names[0] / "reconciliation.json"
        victim.write_text(victim.read_text() + " ")
        (self.root / names[1] / "stray.txt").write_text("not listed")
        # Tamper first, then add newer bundles: on Linux, .NET's CreationTime follows
        # the directory's change time, so a later tamper would reorder the bundles.
        names = self.add_bundles(2)
        log = self.prune(1)
        self.assertEqual(self.present(), {names[0], names[1], names[3]})
        anomalies = {
            e["bundle"]: e["reason"] for e in log if e["event"] == "anomaly-kept"
        }
        self.assertEqual(set(anomalies), {names[0], names[1]})
        self.assertIn("hash mismatch", anomalies[names[0]])
        self.assertIn("unlisted file", anomalies[names[1]])

    def test_whatif_and_small_drops_delete_nothing(self):
        """Protects: a dry run, and a drop that holds no more than N bundles. Detects:
        -WhatIf removing anything, or a prune run when bundles <= Keep.
        Needs: PowerShell. Breadcrumb: README "Retention"."""
        names = self.add_bundles(3)
        log = self.prune(1, "-WhatIf")
        self.assertEqual(self.present(), set(names))
        self.assertEqual(len([e for e in log if e["event"] == "would-remove"]), 2)
        self.assertTrue(log[-1]["whatif"])
        log = self.prune(3)
        self.assertEqual(self.present(), set(names))
        self.assertEqual(log[-1]["removed"], 0)

    @unittest.skipUnless(
        os.name == "posix" and os.geteuid() != 0,
        "needs a non-root POSIX user to make the drop root read-only",
    )
    def test_failed_delete_is_logged_and_fails_the_run(self):
        """Protects: the removal log as a record of what happened. Detects: a delete
        that fails after its remove line is written but leaves no remove-failed
        line, or a run that still exits 0 (the task result is how a failure shows).
        Needs: PowerShell, a non-root POSIX user. Breadcrumb: README "Retention"."""
        names = self.add_bundles(2)
        self.root.chmod(0o555)
        self.addCleanup(self.root.chmod, 0o755)
        log = self.prune(1, ok=False)
        failed = [e for e in log if e["event"] == "remove-failed"]
        self.assertEqual([e["bundle"] for e in failed], [names[0]])
        self.assertTrue(failed[0]["error"])
        self.assertTrue(failed[0]["still_present"])


class PruneAclTests(unittest.TestCase):
    def test_drop_installer_keeps_the_prune_rights_identical(self):
        """Protects: the prune's delete rights surviving a re-run of the drop installer,
        which rebuilds the incoming DACL from scratch. Detects: the two installers
        drifting apart on the prune's rights expression or inheritance flags.
        Needs: nothing (static check). Breadcrumb: both installers' comments."""
        drop = (PKG / "windows" / "Install-AgentHubDrop.ps1").read_text()
        prune = (PKG / "windows" / "Install-AgentHubPrune.ps1").read_text()
        rights = "$pruneDirRights = $rx -bor $F::DeleteSubdirectoriesAndFiles"
        self.assertIn(rights, drop)
        self.assertIn(rights, prune)
        for flags in (
            "$pruneDirRights $I::ContainerInherit $P::None",
            "$rx $I::ObjectInherit $P::InheritOnly",
        ):
            self.assertIn(flags, drop)
            self.assertIn(flags, prune)
        # S4U registration is refused on dtm-p1gen7 (README "Retention"), so the task
        # uses a password logon and the installer reads the principal back.
        self.assertIn("Register-ScheduledTask @task -Password", prune)
        self.assertIn("$t.Principal.LogonType -ne 'Password'", prune)
        self.assertNotIn("-LogonType S4U", code_lines(prune))
        self.assertNotIn(
            "agenthubdrop", prune.split("$user = ", 1)[1].split("\n", 1)[0]
        )

    def test_prune_deletes_without_remove_item(self):
        """Protects: deletes under the prune account, which holds DELETE-child but not
        WriteAttributes. Detects: a return to Remove-Item -Force, which on Windows
        PowerShell 5.1 was denied on dtm-p1gen7 (2026-10-29) while .NET deletes
        succeeded. pwsh on Linux cannot reproduce that ACL, hence a static check.
        Needs: nothing. Breadcrumb: the delete comment in Invoke-AgentHubPrune.ps1."""
        script = PRUNE.read_text()
        self.assertNotIn("Remove-Item", code_lines(script))
        self.assertIn("[IO.Directory]::Delete($b.FullName, $true)", script)


if __name__ == "__main__":
    unittest.main()
