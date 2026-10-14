#!/usr/bin/env python3
# ruff: noqa: E501, RUF100  (formatted to the vigil-utils 120-column style; vendored copies must stay identical)
"""Upload/download a CI artifact on the fleet S3 store using only the standard library.

The runner images carry python3, tar and curl but no aws CLI or rclone, so requests are
signed here (AWS Signature V4, path style, UNSIGNED-PAYLOAD over the fleet-internal HTTP
endpoint). Objects are <bucket>/<owner>/<repo>/<run_id>/<attempt>/<job>/<name>.tar.zst
(.tar.gz when the runner has no zstd), plus a <name>.sha256 sidecar that download verifies.
The sidecar carries an HMAC of the digest and object key under the store secret, so a payload
or sidecar altered on the plain-HTTP channel is rejected by anyone who holds the key.
Configuration arrives in FA_* environment variables; the key never appears in argv.

Failure policy (fail closed):
  * ConfigError and IntegrityError always fail the step: a bad name or key component, a digest or
    HMAC mismatch, a hostile archive. Required or not.
  * StoreError (endpoint unset, connection, PUT/GET error, missing object, nothing matched the
    upload paths) fails the step when FA_REQUIRED is "true" and only warns (::warning::) otherwise.
  * Missing credentials on a fork pull request (no secrets by design) is the one explicit skip:
    a ::warning:: annotation, a step-summary entry and the output skipped=true.
"""

from __future__ import annotations

import datetime
import glob
import hashlib
import hmac
import http.client
import os
import re
import shutil
import stat
import subprocess
import sys
import tarfile
import tempfile
import urllib.parse
import xml.etree.ElementTree as ElementTree  # noqa: S405
from collections.abc import Iterator
from pathlib import Path, PurePosixPath

RETENTION = ("short", "ci", "release")
# Every object-key component: [A-Za-z0-9._-], 1..128 chars, no leading dot, no "..", no "/".
COMPONENT_RE = re.compile(r"^[A-Za-z0-9_-][A-Za-z0-9._-]{0,127}$")


def safe_component(value: str, what: str) -> str:
    if not COMPONENT_RE.match(value) or ".." in value:
        raise ConfigError(f"unsafe {what} {value!r}")
    return value


REGION = "us-east-1"
CHUNK = 1 << 20
DEFAULT_MAX_OBJECT = 2 << 30  # per packed object
DEFAULT_MAX_RUN = 8 << 30  # per run, per bucket
DEFAULT_MAX_UNPACKED = 4 << 30  # extraction (decompression-bomb guard)
MAX_MEMBERS = 200_000


class FleetError(Exception):
    """Base class; the subclasses decide whether `required: false` may downgrade the failure."""


class ConfigError(FleetError):
    """Bad input. Always fails."""


class IntegrityError(FleetError):
    """Digest, HMAC or archive-content violation. Always fails."""


class StoreError(FleetError):
    """The store or the inputs could not deliver. Fails when required, warns otherwise."""


class LimitError(FleetError):
    """A custody bound (object, run or extraction size) was exceeded. Always fails."""


def limit(env: dict[str, str], key: str, default: int) -> int:
    raw = env.get(key, "").strip()
    if not raw:
        return default
    if not raw.isdigit() or int(raw) <= 0:
        raise ConfigError(f"{key} must be a positive integer, got {raw!r}")
    return int(raw)


def _hmac(key: bytes, msg: str) -> bytes:
    return hmac.new(key, msg.encode(), hashlib.sha256).digest()


class Store:
    def __init__(self, endpoint: str, key: str) -> None:
        url = urllib.parse.urlsplit(endpoint)
        if url.scheme not in ("http", "https") or not url.hostname:
            raise ConfigError(f"bad endpoint {endpoint!r}")
        self.tls = url.scheme == "https"
        self.host = url.netloc
        access, sep, secret = key.partition(":")
        if not sep or not access or not secret:
            raise ConfigError("key must be ACCESS:SECRET")
        self.access, self.secret = access, secret

    def _sign(self, method: str, path: str, headers: dict[str, str], query: str = "") -> dict[str, str]:
        now = datetime.datetime.now(datetime.timezone.utc)
        amz, day = now.strftime("%Y%m%dT%H%M%SZ"), now.strftime("%Y%m%d")
        headers = {**headers, "host": self.host, "x-amz-date": amz, "x-amz-content-sha256": "UNSIGNED-PAYLOAD"}
        names = sorted(headers)
        canon_headers = "".join(f"{n}:{headers[n].strip()}\n" for n in names)
        signed = ";".join(names)
        canon = "\n".join(
            [method, urllib.parse.quote(path, safe="/-_.~"), query, canon_headers, signed, "UNSIGNED-PAYLOAD"]
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
        headers = self._sign("PUT", path, {"content-length": str(src.stat().st_size)})
        conn = self._conn()
        try:
            with src.open("rb") as fh:
                conn.request("PUT", urllib.parse.quote(path, safe="/-_.~"), body=fh, headers=headers)
                resp = conn.getresponse()
                body = resp.read(512)
        except (OSError, http.client.HTTPException) as exc:
            raise StoreError(f"PUT {path}: {exc}") from exc
        finally:
            conn.close()
        if resp.status not in (200, 201):
            raise StoreError(f"PUT {path}: HTTP {resp.status} {body[:200]!r}")

    def put_bytes(self, path: str, data: bytes) -> None:
        with tempfile.NamedTemporaryFile() as tmp:
            tmp.write(data)
            tmp.flush()
            self.put(path, Path(tmp.name))

    def get(self, path: str) -> Iterator[bytes]:
        headers = self._sign("GET", path, {})
        conn = self._conn()
        try:
            conn.request("GET", urllib.parse.quote(path, safe="/-_.~"), headers=headers)
            resp = conn.getresponse()
            if resp.status != 200:
                body = resp.read(512)
                raise StoreError(f"GET {path}: HTTP {resp.status} {body[:200]!r}")
            while chunk := resp.read(CHUNK):
                yield chunk
        except (OSError, http.client.HTTPException) as exc:
            raise StoreError(f"GET {path}: {exc}") from exc
        finally:
            conn.close()

    def get_bytes(self, path: str) -> bytes:
        return b"".join(self.get(path))

    def list_size(self, bucket: str, prefix: str) -> int:
        """Sum the sizes of the objects under bucket/prefix (ListObjectsV2, paginated)."""
        total, token = 0, ""
        while True:
            params = {"list-type": "2", "prefix": prefix}
            if token:
                params["continuation-token"] = token
            query = "&".join(
                f"{urllib.parse.quote(k, safe='-_.~')}={urllib.parse.quote(v, safe='-_.~')}"
                for k, v in sorted(params.items())
            )
            headers = self._sign("GET", f"/{bucket}", {}, query)
            conn = self._conn()
            try:
                conn.request("GET", f"/{bucket}?{query}", headers=headers)
                resp = conn.getresponse()
                body = resp.read(8 << 20)
            except (OSError, http.client.HTTPException) as exc:
                raise StoreError(f"LIST {bucket}/{prefix}: {exc}") from exc
            finally:
                conn.close()
            if resp.status == 404:
                return total
            if resp.status != 200:
                raise StoreError(f"LIST {bucket}/{prefix}: HTTP {resp.status} {body[:200]!r}")
            try:
                root = ElementTree.fromstring(body)  # noqa: S314  fleet-internal store, size-bounded read
            except ElementTree.ParseError as exc:
                raise StoreError(f"LIST {bucket}/{prefix}: unreadable listing") from exc
            for node in root.iter():
                if node.tag.rsplit("}", 1)[-1] == "Size" and node.text and node.text.isdigit():
                    total += int(node.text)
            truncated = next((n.text for n in root.iter() if n.tag.rsplit("}", 1)[-1] == "IsTruncated"), "false")
            nxt = next((n.text for n in root.iter() if n.tag.rsplit("}", 1)[-1] == "NextContinuationToken"), None)
            if str(truncated).lower() != "true" or not nxt:
                return total
            token = nxt

    def seal(self, digest: str, object_path: str) -> str:
        """HMAC binding the digest to the object path under the store secret."""
        return hmac.new(self.secret.encode(), f"{digest} {object_path}".encode(), hashlib.sha256).hexdigest()


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
    for label, part in zip(("owner", "repo", "run id", "attempt", "job"), parts, strict=True):
        safe_component(part, f"key component ({label})")
    return env["FA_RETENTION"], "/".join(parts)


def summary(lines: list[str]) -> None:
    target = os.environ.get("GITHUB_STEP_SUMMARY")
    if target:
        with open(target, "a", encoding="utf-8") as fh:
            fh.write("\n".join(lines) + "\n")


def emit_output(key: str, value: str) -> None:
    target = os.environ.get("GITHUB_OUTPUT")
    if target:
        with open(target, "a", encoding="utf-8") as fh:
            fh.write(f"{key}={value}\n")


def allowed_roots() -> list[Path]:
    roots = [Path.cwd()]
    for var in ("GITHUB_WORKSPACE", "RUNNER_TEMP"):
        if os.environ.get(var):
            roots.append(Path(os.environ[var]))
    return [r.resolve() for r in roots]


def expand(patterns: list[str]) -> list[str]:
    """Resolve upload patterns inside the workspace. `dir/**` packs the whole directory (hidden files included)."""
    roots = allowed_roots()
    found: list[str] = []
    for raw in patterns:
        pattern = raw.strip()
        if pattern.endswith("/**"):
            pattern = pattern[:-3]
        matches = glob.glob(pattern, recursive=True)
        if not matches and os.path.lexists(pattern):
            matches = [pattern]
        for match in matches:
            # Resolve the parent only: a symlink member itself is stored as a link and checked on download.
            resolved = (Path(match).parent.resolve() / Path(match).name) if Path(match).name else Path(match).resolve()
            if not any(resolved == r or r in resolved.parents for r in roots):
                raise ConfigError(f"path {match!r} resolves outside the workspace and runner temp directory")
            if match not in found:
                found.append(match)
    return found


def pack(paths: list[str], dest: Path) -> str:
    """tar the paths; zstd when available, gzip otherwise. Returns the object suffix."""
    zstd = shutil.which("zstd")
    suffix = ".tar.zst" if zstd else ".tar.gz"
    comp = ["--use-compress-program", f"{zstd} -T0 -3"] if zstd else ["-z"]
    try:
        subprocess.run(["tar", *comp, "-cf", str(dest), "--", *paths], check=True)
    except subprocess.CalledProcessError as exc:
        raise StoreError(f"tar failed ({exc.returncode})") from exc
    return suffix


def _inside(base: Path, candidate: Path) -> bool:
    return candidate == base or base in candidate.parents


def validate_members(
    tar: tarfile.TarFile, dest: Path, max_unpacked: int = DEFAULT_MAX_UNPACKED
) -> list[tarfile.TarInfo]:
    """Reject absolute paths, traversal, special files and any link that leaves `dest`."""
    members: list[tarfile.TarInfo] = []
    total = 0
    base = dest.resolve()
    for m in tar:  # streaming: a bomb is refused at the header that crosses the limit
        members.append(m)
        if len(members) > MAX_MEMBERS:
            raise LimitError(f"archive has more than {MAX_MEMBERS} members")
        name = PurePosixPath(m.name)
        if name.is_absolute() or ".." in name.parts or m.name.startswith("/") or ("\\" in m.name and ":" in m.name[:3]):
            raise IntegrityError(f"unsafe member name {m.name!r}")
        if not (m.isfile() or m.isdir() or m.issym() or m.islnk()):
            raise IntegrityError(f"unsupported member type for {m.name!r}")
        total += m.size if m.isfile() else 0
        if total > max_unpacked:
            raise LimitError(f"archive unpacks to more than {max_unpacked} bytes")
        target = (base / m.name).resolve()
        if not _inside(base, target):
            raise IntegrityError(f"member {m.name!r} resolves outside the destination")
        if m.issym():
            link = PurePosixPath(m.linkname)
            if link.is_absolute() or not _inside(base, ((base / m.name).parent / m.linkname).resolve()):
                raise IntegrityError(f"symlink {m.name!r} -> {m.linkname!r} escapes the destination")
        if m.islnk():
            link = PurePosixPath(m.linkname)
            if link.is_absolute() or ".." in link.parts or not _inside(base, (base / m.linkname).resolve()):
                raise IntegrityError(f"hardlink {m.name!r} -> {m.linkname!r} escapes the destination")
    return members


def unpack(archive: Path, out: Path, suffix: str, max_unpacked: int = DEFAULT_MAX_UNPACKED) -> None:
    """Validate every member, extract into a private temp dir, re-check, then move into `out`."""
    tmp_tar: Path | None = None
    source = archive
    try:
        if suffix == ".tar.zst":
            zstd = shutil.which("zstd")
            if not zstd:
                raise StoreError("zstd is required to read a .tar.zst object")
            tmp_tar = archive.with_suffix(".tar")
            written = 0
            with (
                subprocess.Popen([zstd, "-d", "-c", str(archive)], stdout=subprocess.PIPE) as proc,
                tmp_tar.open("wb") as fh,
            ):
                assert proc.stdout is not None  # noqa: S101
                while chunk := proc.stdout.read(CHUNK):
                    written += len(chunk)
                    if written > max_unpacked + (64 << 20):  # members total plus tar headers and padding
                        proc.kill()
                        raise LimitError(f"decompressed archive is larger than {max_unpacked} bytes")
                    fh.write(chunk)
            if proc.returncode:
                raise IntegrityError(f"zstd could not decompress the archive ({proc.returncode})")
            source = tmp_tar
        out.mkdir(parents=True, exist_ok=True)
        staging = Path(tempfile.mkdtemp(prefix=".fleet-artifact-", dir=out))
        try:
            with tarfile.open(source, "r:*") as tar:
                members = validate_members(tar, staging, max_unpacked)
                for m in members:
                    m.mode &= ~(stat.S_ISUID | stat.S_ISGID | stat.S_ISVTX)
                    m.uid = m.gid = 0
                    m.uname = m.gname = ""
                try:
                    tar.extractall(staging, members=members, filter="data")  # noqa: S202  members validated above
                except TypeError:  # Python before the tarfile filter backport
                    tar.extractall(staging, members=members)  # noqa: S202
            base = staging.resolve()
            for path in staging.rglob("*"):
                if path.is_symlink() and not _inside(base, path.resolve()):
                    raise IntegrityError(f"extracted link {path.name!r} escapes the destination")
            for entry in sorted(staging.iterdir()):
                target = out / entry.name
                if target.is_symlink() or target.is_file():
                    target.unlink()
                elif target.is_dir():
                    shutil.rmtree(target)
                shutil.move(str(entry), str(target))
        finally:
            shutil.rmtree(staging, ignore_errors=True)
    except (tarfile.TarError, subprocess.CalledProcessError, EOFError) as exc:
        raise IntegrityError(f"archive unreadable: {exc}") from exc
    finally:
        if tmp_tar is not None:
            tmp_tar.unlink(missing_ok=True)


def is_fork(env: dict[str, str]) -> bool:
    return env.get("FA_CTX_FORK", "").lower() == "true"


def run(env: dict[str, str]) -> None:
    mode, name, retention = env["FA_MODE"], env["FA_NAME"], env["FA_RETENTION"]
    if mode not in ("upload", "download"):
        raise ConfigError(f"mode must be upload or download, got {mode!r}")
    if retention not in RETENTION:
        raise ConfigError(f"retention must be one of {RETENTION}")
    safe_component(name, "name")
    if not env.get("FA_ENDPOINT") or not env.get("FA_KEY"):
        if is_fork(env):
            msg = f"fleet-artifact {mode} {name} SKIPPED: fork pull request has no secrets by design"
            print(f"::warning::{msg}")
            summary(
                [f"### Fleet artifact SKIPPED: `{name}`", "- fork pull request: no FLEET_ARTIFACT_KEY, nothing stored"]
            )
            emit_output("skipped", "true")
            return
        raise StoreError("endpoint or key not configured (FLEET_ARTIFACT_ENDPOINT / FLEET_ARTIFACT_KEY)")
    store = Store(env["FA_ENDPOINT"], env["FA_KEY"])
    bucket, prefix = object_prefix(env, mode == "download")
    required = env.get("FA_REQUIRED", "true").lower() == "true"
    max_object = limit(env, "FA_MAX_OBJECT_BYTES", DEFAULT_MAX_OBJECT)
    max_run = limit(env, "FA_MAX_RUN_BYTES", DEFAULT_MAX_RUN)
    max_unpacked = limit(env, "FA_MAX_UNPACKED_BYTES", DEFAULT_MAX_UNPACKED)
    with tempfile.TemporaryDirectory() as tmpdir:
        tmp = Path(tmpdir)
        archive = tmp / "payload"
        if mode == "upload":
            paths = expand([p for p in env["FA_PATH"].splitlines() if p.strip()])
            if not paths:
                raise StoreError(f"nothing matched {env['FA_PATH']!r}; no upload")
            suffix = pack(paths, archive)
            packed = archive.stat().st_size
            if packed > max_object:
                raise LimitError(f"packed object is {packed} bytes; the per-object limit is {max_object}")
            run_prefix = "/".join(prefix.split("/")[:3]) + "/"  # owner/repo/run_id/
            used = store.list_size(bucket, run_prefix)
            if used + packed > max_run:
                raise LimitError(
                    f"run already holds {used} bytes in {bucket}; adding {packed} exceeds the per-run limit {max_run}"
                )
            digest = sha256_file(archive)
            key = f"{prefix}/{name}{suffix}"
            object_path = f"/{bucket}/{key}"
            store.put(object_path, archive)
            sidecar = f"{digest}  {name}{suffix}\nhmac {store.seal(digest, object_path)}\n"
            store.put_bytes(f"/{bucket}/{prefix}/{name}.sha256", sidecar.encode())
        else:
            lines = store.get_bytes(f"/{bucket}/{prefix}/{name}.sha256").decode(errors="replace").splitlines()
            try:
                want, fname = lines[0].split()
                seal = lines[1].split()[1]
            except (IndexError, ValueError) as exc:
                raise IntegrityError("malformed sidecar") from exc
            if fname not in (f"{name}.tar.zst", f"{name}.tar.gz"):
                raise IntegrityError(f"sidecar names unexpected object {fname!r}")
            suffix, key = fname[len(name) :], f"{prefix}/{fname}"
            object_path = f"/{bucket}/{key}"
            if not hmac.compare_digest(seal, store.seal(want, object_path)):
                raise IntegrityError("sidecar HMAC mismatch: object or sidecar was altered")
            got = 0
            with archive.open("wb") as fh:
                for chunk in store.get(object_path):
                    got += len(chunk)
                    if got > max_object:
                        raise LimitError(f"object exceeds the per-object limit {max_object}")
                    fh.write(chunk)
            digest = sha256_file(archive)
            if digest != want:
                raise IntegrityError(f"sha256 mismatch: sidecar {want}, downloaded {digest}")
            unpack(archive, Path(env["FA_PATH"].strip()), suffix, max_unpacked)
        size = archive.stat().st_size
    if required and not digest:
        raise IntegrityError("empty digest")
    location = f"{env['FA_ENDPOINT'].rstrip('/')}/{bucket}/{key}"
    emit_output("sha256", digest)
    emit_output("location", location)
    summary(
        [
            f"### Fleet artifact ({mode}): `{name}`",
            f"- sha256: `{digest}`",
            f"- size: {size} bytes",
            f"- fleet path: `/srv/vigil-share/artifacts/ci/{bucket}/{key}` on asuspro13",
            f"- url: `{location}`",
            f"- retention: {retention}",
        ]
    )
    print(f"fleet-artifact {mode} ok: {name} sha256={digest} {location}")


def main() -> int:
    env = dict(os.environ)
    required = env.get("FA_REQUIRED", "true").lower() == "true"
    try:
        run(env)
    except FleetError as exc:
        message = f"fleet-artifact {env.get('FA_MODE')} {env.get('FA_NAME')} failed: {exc}"
        fatal = required or not isinstance(exc, StoreError)  # Config, Integrity and Limit errors are never downgraded
        print(f"::{'error' if fatal else 'warning'}::{message}")
        summary([f"### Fleet artifact {'FAILED' if fatal else 'WARNING'}: `{env.get('FA_NAME')}`", f"- {exc}"])
        return 1 if fatal else 0
    except KeyError as exc:
        print(f"::error::fleet-artifact missing input {exc}")
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
