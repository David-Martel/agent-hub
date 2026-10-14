# ruff: noqa: E501, RUF100, S101, S105, S106, S108, ANN, D, PLR2004, PT, INP001, ARG, SLF001
"""Controls for the fleet-artifact action (stdlib unittest, loopback only).

Protects: the object layout, the sha256 + HMAC sidecar check, AWS SigV4 request signing, the
fail-closed policy (required uploads fail on a missing endpoint, a rejected PUT, a digest
mismatch, nothing matched or a missing object), the explicit fork skip, and archive extraction
(absolute names, `..`, symlink and hardlink escapes are rejected before anything is written).
Detects: a store outage that turns a required step green, a download that accepts a corrupted,
re-signed or hostile archive, an upload path that leaves the workspace, or a changed key layout.
Needs: python3, tar and loopback sockets only. A fake S3 server stands in for the store, so the
file runs unchanged inside a no-network container (`--network none` keeps loopback).
Breadcrumb: fleet_artifact.py, action.yml, docs/ci/FLEET_ARTIFACT_STORE.md (vigil-utils).
"""

from __future__ import annotations

import contextlib
import hashlib
import hmac
import importlib.util
import io
import os
import shutil
import tarfile
import tempfile
import threading
import unittest
import urllib.parse
from http.server import BaseHTTPRequestHandler, HTTPServer
from pathlib import Path
from unittest import mock

HERE = Path(__file__).resolve().parent
KEY = "vigilci:s3cret-for-tests"
spec = importlib.util.spec_from_file_location("fleet_artifact", HERE / "fleet_artifact.py")
assert spec and spec.loader
fa = importlib.util.module_from_spec(spec)
spec.loader.exec_module(fa)


def expected_signature(handler: BaseHTTPRequestHandler, secret: str) -> str:
    """Recompute the SigV4 signature server-side from the request as received."""
    auth = handler.headers["Authorization"]
    fields = dict(part.strip().split("=", 1) for part in auth.split(" ", 1)[1].split(","))
    signed = fields["SignedHeaders"].split(";")
    canon_headers = "".join(f"{n}:{handler.headers[n].strip()}\n" for n in signed)
    path, _, query = handler.path.partition("?")
    canon = "\n".join(
        [
            handler.command,
            path,
            query,
            canon_headers,
            fields["SignedHeaders"],
            handler.headers["x-amz-content-sha256"],
        ]
    )
    scope = fields["Credential"].split("/", 1)[1]
    to_sign = "\n".join(
        ["AWS4-HMAC-SHA256", handler.headers["x-amz-date"], scope, hashlib.sha256(canon.encode()).hexdigest()]
    )
    key = ("AWS4" + secret).encode()
    for part in scope.split("/"):
        key = hmac.new(key, part.encode(), hashlib.sha256).digest()
    return hmac.new(key, to_sign.encode(), hashlib.sha256).hexdigest()


class FakeStore:
    def __init__(self, fail_put: int | None = None) -> None:
        self.objects: dict[str, bytes] = {}
        self.fail_put = fail_put
        outer = self

        class Handler(BaseHTTPRequestHandler):
            def log_message(self, *args: object) -> None:
                pass

            def _reply(self, status: int, body: bytes = b"") -> None:
                self.send_response(status)
                self.send_header("Content-Length", str(len(body)))
                self.end_headers()
                self.wfile.write(body)

            def _authorized(self) -> bool:
                auth = self.headers.get("Authorization", "")
                good = (
                    auth.startswith("AWS4-HMAC-SHA256 Credential=vigilci/")
                    and expected_signature(self, KEY.split(":", 1)[1]) == auth.rsplit("Signature=", 1)[1]
                )
                if not good:
                    self._reply(403)
                return good

            def do_PUT(self) -> None:
                body = self.rfile.read(int(self.headers["Content-Length"]))
                if not self._authorized():
                    return
                if outer.fail_put:
                    self._reply(outer.fail_put)
                    return
                outer.objects[self.path] = body
                self._reply(200)

            def do_GET(self) -> None:
                if not self._authorized():
                    return
                path, _, query = self.path.partition("?")
                if "list-type=2" in query:
                    bucket = path.strip("/")
                    prefix = urllib.parse.unquote(dict(p.split("=", 1) for p in query.split("&")).get("prefix", ""))
                    items = "".join(
                        f"<Contents><Key>{k}</Key><Size>{len(v)}</Size></Contents>"
                        for k, v in outer.objects.items()
                        if k.startswith(f"/{bucket}/{prefix}")
                    )
                    xml = f"<ListBucketResult><IsTruncated>false</IsTruncated>{items}</ListBucketResult>"
                    self._reply(200, xml.encode())
                    return
                body = outer.objects.get(path)
                self._reply(200 if body is not None else 404, body or b"")

        self.server = HTTPServer(("127.0.0.1", 0), Handler)
        self.endpoint = f"http://127.0.0.1:{self.server.server_port}"
        threading.Thread(target=self.server.serve_forever, args=(0.01,), daemon=True).start()

    def close(self) -> None:
        self.server.shutdown()
        self.server.server_close()


class Base(unittest.TestCase):
    def setUp(self) -> None:
        self.store = FakeStore()
        self.addCleanup(self.store.close)
        self.tmp = Path(tempfile.mkdtemp(prefix="fa-test-"))
        self.addCleanup(shutil.rmtree, self.tmp, True)
        self.work = self.tmp / "work"
        self.work.mkdir()
        self.cwd = os.getcwd()
        os.chdir(self.work)
        self.addCleanup(os.chdir, self.cwd)
        self.out_file = self.tmp / "gh_output"
        self.summary_file = self.tmp / "summary.md"
        patcher = mock.patch.dict(
            os.environ,
            {
                "GITHUB_OUTPUT": str(self.out_file),
                "GITHUB_STEP_SUMMARY": str(self.summary_file),
                "GITHUB_WORKSPACE": str(self.work),
                "RUNNER_TEMP": str(self.tmp / "rt"),
            },
        )
        patcher.start()
        self.addCleanup(patcher.stop)

    def env(self, mode: str, path: str, **extra: str) -> dict[str, str]:
        env = {
            "FA_MODE": mode,
            "FA_NAME": "diag",
            "FA_PATH": path,
            "FA_RETENTION": "ci",
            "FA_ENDPOINT": self.store.endpoint,
            "FA_KEY": KEY,
            "FA_REQUIRED": "true",
            "FA_CTX_REPOSITORY": "David-Martel/vigil-utils",
            "FA_CTX_RUN_ID": "42",
            "FA_CTX_ATTEMPT": "2",
            "FA_CTX_JOB": "tooling",
        }
        env.update(extra)
        return env

    def main(self, env: dict[str, str]) -> tuple[int, str]:
        buf = io.StringIO()
        with mock.patch.dict(os.environ, env), contextlib.redirect_stdout(buf):
            code = fa.main()
        return code, buf.getvalue()

    def populate(self) -> None:
        (self.work / "out" / ".hidden").mkdir(parents=True)
        (self.work / "out" / "a.txt").write_text("alpha\n")
        (self.work / "out" / ".hidden" / "b.txt").write_text("beta\n")

    def outputs(self) -> dict[str, str]:
        if not self.out_file.exists():
            return {}
        return dict(line.split("=", 1) for line in self.out_file.read_text().splitlines())

    def object_key(self, suffix: str) -> str:
        return next(k for k in self.store.objects if k.endswith(suffix))

    def plant_archive(self, build) -> Path:
        """Store a hostile archive with a VALID sidecar+HMAC, so only member validation can stop it."""
        raw = self.tmp / "hostile.tar"
        with tarfile.open(raw, "w:gz") as tar:
            build(tar)
        data = raw.read_bytes()
        prefix = "/ci/David-Martel/vigil-utils/42/2/tooling/"
        store = fa.Store(self.store.endpoint, KEY)
        digest = hashlib.sha256(data).hexdigest()
        self.store.objects[prefix + "diag.tar.gz"] = data
        sidecar = f"{digest}  diag.tar.gz\nhmac {store.seal(digest, prefix + 'diag.tar.gz')}\n"
        self.store.objects[prefix + "diag.sha256"] = sidecar.encode()
        dest = self.tmp / "restored"
        return dest


class RoundTrip(Base):
    def test_positive_roundtrip_layout_digest_and_hidden_files(self) -> None:
        self.populate()
        code, _ = self.main(self.env("upload", "out/**"))
        self.assertEqual(code, 0)
        prefix = "/ci/David-Martel/vigil-utils/42/2/tooling/"
        self.assertTrue(all(k.startswith(prefix) for k in self.store.objects), self.store.objects.keys())
        self.assertIn(prefix + "diag.sha256", self.store.objects)
        up = self.outputs()["sha256"]
        self.assertEqual(len(up), 64)
        dest = self.tmp / "restored"
        code, _ = self.main(self.env("download", str(dest)))
        self.assertEqual(code, 0)
        self.assertEqual((dest / "out" / "a.txt").read_text(), "alpha\n")
        self.assertEqual((dest / "out" / ".hidden" / "b.txt").read_text(), "beta\n")
        self.assertEqual(self.outputs()["sha256"], up)
        self.assertIn(up, self.summary_file.read_text())
        self.assertEqual(list(dest.glob(".fleet-artifact-*")), [])

    def test_gzip_fallback_without_zstd(self) -> None:
        self.populate()
        with mock.patch.object(fa.shutil, "which", lambda _n: None):
            self.assertEqual(self.main(self.env("upload", "out"))[0], 0)
            self.assertTrue(any(k.endswith("diag.tar.gz") for k in self.store.objects))
            dest = self.tmp / "restored"
            self.assertEqual(self.main(self.env("download", str(dest)))[0], 0)
        self.assertEqual((dest / "out" / "a.txt").read_text(), "alpha\n")


class FailClosed(Base):
    def test_missing_endpoint_fails_when_required(self) -> None:
        self.populate()
        code, out = self.main(self.env("upload", "out", FA_ENDPOINT=""))
        self.assertEqual(code, 1)
        self.assertIn("::error::", out)

    def test_required_is_the_default_when_the_variable_is_absent(self) -> None:
        self.populate()
        env = self.env("upload", "out", FA_ENDPOINT="")
        del env["FA_REQUIRED"]
        code, out = self.main(env)
        self.assertEqual(code, 1)
        self.assertIn("::error::", out)

    def test_missing_endpoint_only_warns_when_not_required(self) -> None:
        self.populate()
        code, out = self.main(self.env("upload", "out", FA_ENDPOINT="", FA_REQUIRED="false"))
        self.assertEqual(code, 0)
        self.assertIn("::warning::", out)

    def test_fork_pull_request_is_an_explicit_visible_skip(self) -> None:
        self.populate()
        code, out = self.main(self.env("upload", "out", FA_ENDPOINT="", FA_KEY="", FA_CTX_FORK="true"))
        self.assertEqual(code, 0)
        self.assertIn("::warning::", out)
        self.assertIn("SKIPPED", out)
        self.assertEqual(self.outputs().get("skipped"), "true")
        self.assertNotIn("sha256", self.outputs())
        self.assertIn("SKIPPED", self.summary_file.read_text())

    def test_wrong_secret_is_rejected_by_the_store(self) -> None:
        self.populate()
        code, out = self.main(self.env("upload", "out", FA_KEY="vigilci:wrong"))
        self.assertEqual(code, 1)
        self.assertIn("HTTP 403", out)
        self.assertEqual(self.store.objects, {})

    def test_server_error_on_put_fails_when_required_and_warns_otherwise(self) -> None:
        self.populate()
        self.store.fail_put = 500
        code, out = self.main(self.env("upload", "out"))
        self.assertEqual((code, "HTTP 500" in out), (1, True))
        code, out = self.main(self.env("upload", "out", FA_REQUIRED="false"))
        self.assertEqual(code, 0)
        self.assertIn("::warning::", out)

    def test_unreachable_endpoint_fails_when_required(self) -> None:
        self.populate()
        self.assertEqual(self.main(self.env("upload", "out", FA_ENDPOINT="http://127.0.0.1:9"))[0], 1)

    def test_nothing_matched_fails_when_required(self) -> None:
        code, out = self.main(self.env("upload", "does-not-exist/**"))
        self.assertEqual(code, 1)
        self.assertIn("nothing matched", out)
        self.assertEqual(self.store.objects, {})
        self.assertEqual(self.main(self.env("upload", "does-not-exist/**", FA_REQUIRED="false"))[0], 0)

    def test_missing_object_fails_on_download(self) -> None:
        code, out = self.main(self.env("download", str(self.tmp / "restored")))
        self.assertEqual(code, 1)
        self.assertIn("HTTP 404", out)

    def test_digest_mismatch_always_fails_even_when_not_required(self) -> None:
        self.populate()
        self.main(self.env("upload", "out"))
        key = next(k for k in self.store.objects if k.endswith((".tar.zst", ".tar.gz")))
        self.store.objects[key] += b"x"
        for required in ("true", "false"):
            code, out = self.main(self.env("download", str(self.tmp / "restored"), FA_REQUIRED=required))
            self.assertEqual(code, 1)
            self.assertIn("sha256 mismatch", out)

    def test_altered_sidecar_is_rejected_by_the_hmac(self) -> None:
        self.populate()
        self.main(self.env("upload", "out"))
        sidecar_key = self.object_key(".sha256")
        lines = self.store.objects[sidecar_key].decode().splitlines()
        lines[0] = "0" * 64 + lines[0][64:]
        self.store.objects[sidecar_key] = ("\n".join(lines) + "\n").encode()
        code, out = self.main(self.env("download", str(self.tmp / "restored")))
        self.assertEqual(code, 1)
        self.assertIn("HMAC mismatch", out)

    def test_upload_paths_must_stay_inside_the_workspace(self) -> None:
        secret = self.tmp / "outside.txt"
        secret.write_text("x")
        # tmp itself is not under work/ but RUNNER_TEMP is tmp/rt; the sibling file must be refused.
        code, out = self.main(self.env("upload", str(secret)))
        self.assertEqual(code, 1)
        self.assertIn("outside the workspace", out)
        self.assertEqual(self.store.objects, {})

    def test_unsafe_key_components_are_rejected(self) -> None:
        self.populate()
        for job in ("../escape", "a b", "", "x/y"):
            code, _out = self.main(self.env("upload", "out", FA_CTX_JOB=job))
            self.assertEqual(code, 1, job)


class KeysAndLimits(Base):
    def test_hostile_key_components_are_refused_on_upload_and_download(self) -> None:
        self.populate()
        hostile = ["..", "a..b", ".hidden", "-lead/slash", "x/y", "a b", "", "a" * 129, "\u00e9"]
        for value in hostile:
            for var in ("FA_CTX_JOB", "FA_CTX_RUN_ID", "FA_CTX_ATTEMPT"):
                code, _out = self.main(self.env("upload", "out", FA_REQUIRED="false", **{var: value}))
                self.assertEqual(code, 1, (var, value))
            code, _out = self.main(self.env("upload", "out", FA_REQUIRED="false", FA_NAME=value))
            self.assertEqual(code, 1, ("FA_NAME", value))
            for var in ("FA_RUN_ID", "FA_ATTEMPT", "FA_JOB"):
                code, _out = self.main(self.env("download", str(self.tmp / "r"), FA_REQUIRED="false", **{var: value}))
                if value:  # empty download overrides fall back to the current run
                    self.assertEqual(code, 1, (var, value))
        for repo in ("../victim/repo", "owner/..", ".github/x", "owner/.hidden"):
            code, _out = self.main(self.env("download", str(self.tmp / "r"), FA_REPOSITORY=repo))
            self.assertEqual(code, 1, repo)
        self.assertEqual(self.store.objects, {})

    def test_object_size_limit_fails_even_when_not_required(self) -> None:
        (self.work / "big.bin").write_bytes(os.urandom(200_000))
        for required in ("true", "false"):
            code, out = self.main(self.env("upload", "big.bin", FA_MAX_OBJECT_BYTES="50000", FA_REQUIRED=required))
            self.assertEqual(code, 1, required)
            self.assertIn("per-object limit", out)
        self.assertEqual(self.store.objects, {})

    def test_per_run_limit_counts_what_the_run_already_stored(self) -> None:
        (self.work / "a.bin").write_bytes(os.urandom(60_000))
        (self.work / "b.bin").write_bytes(os.urandom(60_000))
        self.assertEqual(self.main(self.env("upload", "a.bin", FA_MAX_RUN_BYTES="100000"))[0], 0)
        code, out = self.main(
            self.env("upload", "b.bin", FA_NAME="second", FA_MAX_RUN_BYTES="100000", FA_REQUIRED="false")
        )
        self.assertEqual(code, 1)
        self.assertIn("per-run limit", out)
        self.assertFalse(any("second" in k for k in self.store.objects))
        self.assertEqual(self.main(self.env("upload", "b.bin", FA_NAME="second", FA_MAX_RUN_BYTES="1000000"))[0], 0)

    def test_download_stops_at_the_object_limit(self) -> None:
        (self.work / "big.bin").write_bytes(os.urandom(200_000))
        self.assertEqual(self.main(self.env("upload", "big.bin"))[0], 0)
        code, out = self.main(self.env("download", str(self.tmp / "r"), FA_MAX_OBJECT_BYTES="50000"))
        self.assertEqual(code, 1)
        self.assertIn("per-object limit", out)
        self.assertFalse((self.tmp / "r" / "big.bin").exists())

    def test_decompression_bomb_is_refused_before_extraction(self) -> None:
        def build(tar: tarfile.TarFile) -> None:
            info = tarfile.TarInfo("zeros.bin")
            info.size = 40 << 20
            tar.addfile(info, io.BytesIO(bytes(40 << 20)))

        dest = self.plant_archive(build)
        code, out = self.main(self.env("download", str(dest), FA_MAX_UNPACKED_BYTES="1000000", FA_REQUIRED="false"))
        self.assertEqual(code, 1)
        self.assertIn("unpacks to more than", out)
        self.assertFalse((dest / "zeros.bin").exists())

    def test_invalid_limit_value_is_a_configuration_error(self) -> None:
        self.populate()
        self.assertEqual(self.main(self.env("upload", "out", FA_MAX_OBJECT_BYTES="lots", FA_REQUIRED="false"))[0], 1)


class HostileArchives(Base):
    def assert_rejected(self, build, label: str, reason: str) -> None:
        sentinel = self.tmp / "sentinel.txt"
        sentinel.write_text("intact")
        dest = self.plant_archive(build)
        code, out = self.main(self.env("download", str(dest), FA_REQUIRED="false"))
        self.assertEqual(code, 1, f"{label}: {out}")
        self.assertIn(reason, out, label)  # rejected by member validation, not by a later accident
        self.assertEqual(sentinel.read_text(), "intact")
        self.assertFalse((self.tmp / "evil.txt").exists(), label)
        self.assertFalse(Path("/tmp/fleet-artifact-evil").exists(), label)
        leftovers = [p for p in dest.rglob("*") if p.is_file()] if dest.exists() else []
        self.assertEqual(leftovers, [], label)

    @staticmethod
    def add_bytes(tar: tarfile.TarFile, name: str, data: bytes = b"evil") -> None:
        info = tarfile.TarInfo(name)
        info.size = len(data)
        tar.addfile(info, io.BytesIO(data))

    def test_absolute_member_name(self) -> None:
        self.assert_rejected(lambda t: self.add_bytes(t, "/tmp/fleet-artifact-evil"), "absolute", "unsafe member name")

    def test_dotdot_traversal(self) -> None:
        self.assert_rejected(lambda t: self.add_bytes(t, "../evil.txt"), "dotdot", "unsafe member name")

    def test_nested_dotdot_traversal(self) -> None:
        self.assert_rejected(lambda t: self.add_bytes(t, "a/../../evil.txt"), "nested dotdot", "unsafe member name")

    def test_symlink_escape_then_write_through_it(self) -> None:
        def build(tar: tarfile.TarFile) -> None:
            link = tarfile.TarInfo("link")
            link.type = tarfile.SYMTYPE
            link.linkname = str(self.tmp)
            tar.addfile(link)
            self.add_bytes(tar, "link/evil.txt")

        self.assert_rejected(build, "symlink", "escapes the destination")

    def test_relative_symlink_escape(self) -> None:
        def build(tar: tarfile.TarFile) -> None:
            link = tarfile.TarInfo("d/link")
            link.type = tarfile.SYMTYPE
            link.linkname = "../../.."
            tar.addfile(link)

        self.assert_rejected(build, "relative symlink", "escapes the destination")

    def test_hardlink_escape(self) -> None:
        def build(tar: tarfile.TarFile) -> None:
            link = tarfile.TarInfo("h")
            link.type = tarfile.LNKTYPE
            link.linkname = "/etc/passwd"
            tar.addfile(link)

        self.assert_rejected(build, "hardlink", "escapes the destination")

    def test_device_node(self) -> None:
        def build(tar: tarfile.TarFile) -> None:
            node = tarfile.TarInfo("dev")
            node.type = tarfile.CHRTYPE
            tar.addfile(node)

        self.assert_rejected(build, "device", "unsupported member type")

    def test_benign_archive_with_safe_relative_symlink_extracts(self) -> None:
        def build(tar: tarfile.TarFile) -> None:
            self.add_bytes(tar, "dir/real.txt", b"ok")
            link = tarfile.TarInfo("dir/alias")
            link.type = tarfile.SYMTYPE
            link.linkname = "real.txt"
            tar.addfile(link)

        dest = self.plant_archive(build)
        self.assertEqual(self.main(self.env("download", str(dest)))[0], 0)
        self.assertEqual((dest / "dir" / "alias").read_text(), "ok")


if __name__ == "__main__":
    unittest.main()
