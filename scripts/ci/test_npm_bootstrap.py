"""Exercise private npm publication and corrupted-archive refusal without network."""

import hashlib
import base64
import json
import os
from pathlib import Path
import shutil
import shlex
import subprocess
import sys
import tarfile
import tempfile
import unittest

SOURCE = Path(__file__).with_name("setup-npm.sh")
BASH = shutil.which("bash")
NODE = shutil.which("node")


@unittest.skipUnless(
    sys.platform == "linux" and BASH and NODE, "Linux Node/bash bootstrap control"
)
class NpmBootstrapTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="npm-bootstrap-control-")
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        shutil.copyfile(SOURCE, self.root / "setup-npm.sh")
        package = self.root / "package/bin"
        package.mkdir(parents=True)
        for name in ("npm-cli.js", "npx-cli.js"):
            (package / name).write_text("console.log('12.2.0')\n")
        self.archive = self.root / "fixture.tgz"
        with tarfile.open(self.archive, "w:gz") as tar:
            tar.add(self.root / "package", arcname="package")
        integrity = (
            "sha512-"
            + base64.b64encode(
                hashlib.sha512(self.archive.read_bytes()).digest()
            ).decode()
        )
        (self.root / "toolchain-policy.json").write_text(
            json.dumps({"node": "26.12.0", "npm": "12.2.0", "npm_integrity": integrity})
        )
        tools = self.root / "fixture-tools"
        tools.mkdir()
        # Only the version advertisement is synthetic. JSON and archive hashing
        # execute real Node; this control does not qualify a Node/npm release.
        node = tools / "node"
        node.write_text(
            '#!/bin/bash\nif [[ "$1" == --version ]]; then echo v26.12.0; '
            f'else exec {shlex.quote(NODE)} "$@"; fi\n'
        )
        curl = tools / "curl"
        curl.write_text(
            '#!/bin/bash\nwhile (($#)); do if [[ "$1" == --output ]]; then '
            'output="$2";shift;fi;shift;done\n'
            'cp "$NPM_FIXTURE_ARCHIVE" "$output"\n'
            'if [[ "${CORRUPT:-0}" == 1 ]]; then printf corrupted > "$output";fi\n'
        )
        node.chmod(0o755)
        curl.chmod(0o755)
        self.env = os.environ.copy()
        self.env.update(
            PATH=str(tools) + os.pathsep + self.env["PATH"],
            RUNNER_TEMP=str(self.root),
            NPM_FIXTURE_ARCHIVE=str(self.archive),
            GITHUB_PATH=str(self.root / "github-path"),
            GITHUB_ENV=str(self.root / "github-env"),
        )

    def invoke(self):
        return subprocess.run(
            [BASH, str(self.root / "setup-npm.sh")],
            env=self.env,
            cwd=self.root,
            text=True,
            capture_output=True,
            timeout=20,
        )

    def test_private_wrappers_published_without_replacing_fixture_tools(self):
        """
        Protects: Job-only npm/npx publication after integrity qualification.
        Detects: Wrong wrapper paths or writes into the inherited tool directory.
        Needs: Linux bash/Node and an owned archive/transfer fixture; no network.
        Breadcrumb: scripts/ci/setup-npm.sh integrity and GITHUB_PATH publication.
        """
        inherited = self.root / "fixture-tools/node"
        before = inherited.read_bytes()
        result = self.invoke()
        self.assertEqual(result.returncode, 0, result.stderr)
        private = Path((self.root / "github-path").read_text().strip())
        self.assertNotEqual(private.parent, inherited.parent)
        self.assertTrue((private / "npm").is_file())
        self.assertTrue((private / "npx").is_file())
        self.assertEqual(inherited.read_bytes(), before)
        self.assertIn("NPM_CONFIG_USERCONFIG=", (self.root / "github-env").read_text())

    def test_corrupt_archive_never_extracts_or_publishes_paths(self):
        """
        Protects: Fail-closed integrity before archive extraction/tool publication.
        Detects: Corrupted downloaded bytes accepted by the bootstrap.
        Needs: Linux bash/Node and an owned archive/transfer fixture; no network.
        Breadcrumb: scripts/ci/setup-npm.sh actual-versus-pinned digest check.
        """
        self.env["CORRUPT"] = "1"
        result = self.invoke()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("Pinned npm archive integrity differs", result.stderr)
        self.assertFalse((self.root / "github-path").exists())
        self.assertFalse((self.root / "github-env").exists())
        self.assertFalse(list(self.root.glob("agent-hub-npm-tools.*/package")))


if __name__ == "__main__":
    unittest.main()
