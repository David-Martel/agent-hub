#!/usr/bin/env python3
"""Upload/download a CI artifact on the fleet S3 store using only the standard library.

The runner images carry python3, tar and curl but no aws CLI or rclone, so requests are
signed here (AWS Signature V4, path style, UNSIGNED-PAYLOAD over the fleet-internal HTTP
endpoint). Objects are <bucket>/<owner>/<repo>/<run_id>/<attempt>/<job>/<name>.tar.zst
(.tar.gz when the runner has no zstd), plus a <name>.sha256 sidecar that download verifies.
Configuration arrives in FA_* environment variables; the key never appears in argv.
"""

# ruff: noqa: E501  (formatted to the vigil-utils 120-column style; vendored copies must stay identical)

from __future__ import annotations

import datetime
import glob
import hashlib
import hmac
import http.client
import os
import re
import shutil
import subprocess
import sys
import tempfile
import urllib.parse
from collections.abc import Iterator
from pathlib import Path

RETENTION = ("short", "ci", "release")
NAME_RE = re.compile(r"^[A-Za-z0-9._-]{1,128}$")
PART_RE = re.compile(r"^[A-Za-z0-9._-]{1,100}$")
REGION = "us-east-1"
CHUNK = 1 << 20


def _hmac(key: bytes, msg: str) -> bytes:
    return hmac.new(key, msg.encode(), hashlib.sha256).digest()


class Store:
    def __init__(self, endpoint: str, key: str) -> None:
        url = urllib.parse.urlsplit(endpoint)
        if url.scheme not in ("http", "https") or not url.hostname:
            raise SystemExit(f"fleet-artifact: bad endpoint {endpoint!r}")
        self.tls = url.scheme == "https"
        self.host = url.netloc
        self.base = endpoint.rstrip("/")
        access, sep, secret = key.partition(":")
        if not sep or not access or not secret:
            raise SystemExit("fleet-artifact: key must be ACCESS:SECRET")
        self.access, self.secret = access, secret

    def _sign(self, method: str, path: str, headers: dict[str, str]) -> dict[str, str]:
        now = datetime.datetime.now(datetime.timezone.utc)
        amz, day = now.strftime("%Y%m%dT%H%M%SZ"), now.strftime("%Y%m%d")
        headers = {
            **headers,
            "host": self.host,
            "x-amz-date": amz,
            "x-amz-content-sha256": "UNSIGNED-PAYLOAD",
        }
        names = sorted(headers)
        canon_headers = "".join(f"{n}:{headers[n].strip()}\n" for n in names)
        signed = ";".join(names)
        canon = "\n".join(
            [method, urllib.parse.quote(path, safe="/-_.~"), "", canon_headers, signed, "UNSIGNED-PAYLOAD"]
        )
        scope = f"{day}/{REGION}/s3/aws4_request"
        to_sign = "\n".join(["AWS4-HMAC-SHA256", amz, scope, hashlib.sha256(canon.encode()).hexdigest()])
        k = _hmac(("AWS4" + self.secret).encode(), day)
        for part in (REGION, "s3", "aws4_request"):
            k = _hmac(k, part)
        sig = hmac.new(k, to_sign.encode(), hashlib.sha256).hexdigest()
        headers["Authorization"] = (
            f"AWS4-HMAC-SHA256 Credential={self.access}/{scope}, SignedHeaders={signed}, Signature={sig}"
        )
        return headers

    def _conn(self) -> http.client.HTTPConnection:
        cls = http.client.HTTPSConnection if self.tls else http.client.HTTPConnection
        return cls(self.host, timeout=300)

    def put(self, path: str, src: Path) -> None:
        size = src.stat().st_size
        headers = self._sign("PUT", path, {"content-length": str(size)})
        conn = self._conn()
        with src.open("rb") as fh:
            conn.request("PUT", urllib.parse.quote(path, safe="/-_.~"), body=fh, headers=headers)
            resp = conn.getresponse()
            body = resp.read(512)
        conn.close()
        if resp.status not in (200, 201):
            raise OSError(f"PUT {path}: HTTP {resp.status} {body[:200]!r}")

    def put_bytes(self, path: str, data: bytes) -> None:
        with tempfile.NamedTemporaryFile() as tmp:
            tmp.write(data)
            tmp.flush()
            self.put(path, Path(tmp.name))

    def get(self, path: str) -> Iterator[bytes]:
        headers = self._sign("GET", path, {})
        conn = self._conn()
        conn.request("GET", urllib.parse.quote(path, safe="/-_.~"), headers=headers)
        resp = conn.getresponse()
        if resp.status != 200:
            body = resp.read(512)
            conn.close()
            raise OSError(f"GET {path}: HTTP {resp.status} {body[:200]!r}")
        while chunk := resp.read(CHUNK):
            yield chunk
        conn.close()

    def get_bytes(self, path: str) -> bytes:
        return b"".join(self.get(path))


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as fh:
        while chunk := fh.read(CHUNK):
            digest.update(chunk)
    return digest.hexdigest()


def object_prefix(env: dict[str, str], download: bool) -> tuple[str, str]:
    repo = (env.get("FA_REPOSITORY") if download else "") or env["FA_CTX_REPOSITORY"]
    owner, _, name = repo.partition("/")
    parts = [
        owner,
        name,
        (env.get("FA_RUN_ID") if download else "") or env["FA_CTX_RUN_ID"],
        (env.get("FA_ATTEMPT") if download else "") or env["FA_CTX_ATTEMPT"],
        (env.get("FA_JOB") if download else "") or env["FA_CTX_JOB"],
    ]
    for part in parts:
        if not PART_RE.match(part):
            raise SystemExit(f"fleet-artifact: unsafe key component {part!r}")
    return env["FA_RETENTION"], "/".join(parts)


def summary(env: dict[str, str], lines: list[str]) -> None:
    target = os.environ.get("GITHUB_STEP_SUMMARY")
    if target:
        with open(target, "a", encoding="utf-8") as fh:
            fh.write("\n".join(lines) + "\n")


def emit_output(key: str, value: str) -> None:
    target = os.environ.get("GITHUB_OUTPUT")
    if target:
        with open(target, "a", encoding="utf-8") as fh:
            fh.write(f"{key}={value}\n")


def expand(patterns: list[str]) -> list[str]:
    """Resolve upload patterns. `dir/**` packs the whole directory (hidden files included)."""
    found: list[str] = []
    for raw in patterns:
        pattern = raw.strip()
        if pattern.endswith("/**"):
            pattern = pattern[:-3]
        matches = glob.glob(pattern, recursive=True)
        if not matches and os.path.lexists(pattern):
            matches = [pattern]
        found.extend(m for m in matches if m not in found)
    return found


def pack(paths: list[str], dest: Path) -> str:
    """tar the paths; zstd when available, gzip otherwise. Returns the object suffix."""
    zstd = shutil.which("zstd")
    suffix = ".tar.zst" if zstd else ".tar.gz"
    comp = ["--use-compress-program", f"{zstd} -T0 -3"] if zstd else ["-z"]
    subprocess.run(["tar", *comp, "-cf", str(dest), "--", *paths], check=True)
    return suffix


def unpack(archive: Path, out: Path, suffix: str) -> None:
    out.mkdir(parents=True, exist_ok=True)
    comp = ["--use-compress-program", "zstd -d"] if suffix == ".tar.zst" else ["-z"]
    subprocess.run(["tar", *comp, "-xf", str(archive), "-C", str(out)], check=True)


def run(env: dict[str, str]) -> None:
    mode, name, retention = env["FA_MODE"], env["FA_NAME"], env["FA_RETENTION"]
    if mode not in ("upload", "download"):
        raise SystemExit(f"fleet-artifact: mode must be upload or download, got {mode!r}")
    if retention not in RETENTION:
        raise SystemExit(f"fleet-artifact: retention must be one of {RETENTION}")
    if not NAME_RE.match(name):
        raise SystemExit(f"fleet-artifact: bad name {name!r}")
    if not env.get("FA_ENDPOINT") or not env.get("FA_KEY"):
        raise OSError("endpoint or key not configured (FLEET_ARTIFACT_ENDPOINT / FLEET_ARTIFACT_KEY)")
    store = Store(env["FA_ENDPOINT"], env["FA_KEY"])
    bucket, prefix = object_prefix(env, mode == "download")
    with tempfile.TemporaryDirectory() as tmpdir:
        tmp = Path(tmpdir)
        if mode == "upload":
            paths = expand([p for p in env["FA_PATH"].splitlines() if p.strip()])
            if not paths:
                print(f"::warning::fleet-artifact: nothing matched {env['FA_PATH']!r}; no upload")
                return
            archive = tmp / "payload"
            suffix = pack(paths, archive)
            digest = sha256_file(archive)
            key = f"{prefix}/{name}{suffix}"
            store.put(f"/{bucket}/{key}", archive)
            store.put_bytes(f"/{bucket}/{prefix}/{name}.sha256", f"{digest}  {name}{suffix}\n".encode())
            size = archive.stat().st_size
        else:
            found = None
            for suffix in (".tar.zst", ".tar.gz"):
                try:
                    sidecar = store.get_bytes(f"/{bucket}/{prefix}/{name}.sha256").decode()
                    want = sidecar.split()[0]
                    if sidecar.split()[1] != f"{name}{suffix}":
                        continue
                    found = suffix
                    break
                except OSError:
                    break
            if found is None:
                raise OSError(f"no object {bucket}/{prefix}/{name}.tar.* (or sidecar mismatch)")
            suffix, key = found, f"{prefix}/{name}{found}"
            archive = tmp / "payload"
            with archive.open("wb") as fh:
                for chunk in store.get(f"/{bucket}/{key}"):
                    fh.write(chunk)
            digest = sha256_file(archive)
            if digest != want:
                raise OSError(f"sha256 mismatch: sidecar {want}, downloaded {digest}")
            unpack(archive, Path(env["FA_PATH"].strip()), suffix)
            size = archive.stat().st_size
    location = f"{env['FA_ENDPOINT'].rstrip('/')}/{bucket}/{key}"
    emit_output("sha256", digest)
    emit_output("location", location)
    summary(
        env,
        [
            f"### Fleet artifact ({mode}): `{name}`",
            f"- sha256: `{digest}`",
            f"- size: {size} bytes",
            f"- fleet path: `/srv/vigil-share/artifacts/ci/{bucket}/{key}` on asuspro13",
            f"- url: `{location}`",
            f"- retention: {retention}",
        ],
    )
    print(f"fleet-artifact {mode} ok: {name} sha256={digest} {location}")


def main() -> int:
    env = dict(os.environ)
    required = env.get("FA_REQUIRED", "false").lower() == "true"
    try:
        run(env)
    except (OSError, subprocess.CalledProcessError, http.client.HTTPException) as exc:
        message = f"fleet-artifact {env.get('FA_MODE')} {env.get('FA_NAME')} failed: {exc}"
        print(f"::{'error' if required else 'warning'}::{message}")
        summary(env, [f"### Fleet artifact FAILED: `{env.get('FA_NAME')}`", f"- {exc}"])
        return 1 if required else 0
    return 0


if __name__ == "__main__":
    sys.exit(main())
