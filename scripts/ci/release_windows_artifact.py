"""Reuse only the exact native Windows artifact admitted by reviewed main CI."""

from __future__ import annotations

import argparse
import base64
from datetime import datetime
import hashlib
import io
import json
import os
from pathlib import Path
import re
import stat
import time
import urllib.error
import urllib.parse
import urllib.request
import zipfile

CI_PATH = ".github/workflows/ci.yml"
POLICY_PATH = ".github/workflows/dependabot-automerge.yml"
ARTIFACT = "agent-bus-windows-x64"
MANUAL = "Windows Native Validation (manual)"
WINDOWS = "Windows Build And Unit Tests"
BINARIES = ("agent-bus.exe", "agent-bus-http.exe", "agent-bus-mcp.exe")
MEMBERS = {*BINARIES, "artifact-sha256.json"}
MAX_JSON = 4 * 1024 * 1024
MAX_ARCHIVE = 256 * 1024 * 1024
MAX_CALLS = 32
SHA = re.compile(r"[a-f0-9]{40}\Z")
DIGEST = re.compile(r"[a-f0-9]{64}\Z")


class Refused(ValueError):
    """A bounded admission failure containing no remote payload or credentials."""


def require(condition, reason):
    if not condition:
        raise Refused(reason)


def positive(value):
    return isinstance(value, int) and not isinstance(value, bool) and value > 0


def sha256(data):
    return hashlib.sha256(data).hexdigest()


def unique_object(pairs):
    result = {}
    for key, value in pairs:
        require(key not in result, "duplicate JSON key")
        result[key] = value
    return result


def decode_json(data):
    require(len(data) <= MAX_JSON, "JSON exceeds size bound")
    return json.loads(data, object_pairs_hook=unique_object)


def timestamp(value):
    require(isinstance(value, str) and value.endswith("Z"), "missing UTC timestamp")
    try:
        return datetime.fromisoformat(value[:-1] + "+00:00")
    except ValueError:
        raise Refused("invalid UTC timestamp") from None


def declaration(source, key):
    values = re.findall(rf"(?m)^\s*{key}: '((?:[^']|'')*)'$", source)
    require(len(values) == 1, "missing or ambiguous reviewed inventory")
    return decode_json(values[0].replace("''", "'").encode())


def policy(repo_root):
    raw = (repo_root / CI_PATH).read_bytes()
    inventory = declaration(
        (repo_root / POLICY_PATH).read_text(), "REVIEWED_JOB_INVENTORY_JSON"
    )
    require(isinstance(inventory, dict), "CI inventory is not a mapping")
    rule = inventory.get("CI")
    require(isinstance(rule, dict), "CI inventory absent")
    require(
        set(rule) == {"path", "source_sha256", "required_jobs", "auxiliary_sources"},
        "CI inventory schema changed",
    )
    require(
        rule["path"] == CI_PATH and rule["source_sha256"] == sha256(raw),
        "reviewed CI producer hash differs",
    )
    jobs = rule["required_jobs"]
    require(
        isinstance(jobs, list)
        and len(jobs) == 16
        and all(
            isinstance(name, str)
            and name
            and len(name) < 200
            and not any(ord(c) < 32 or ord(c) == 127 for c in name)
            for name in jobs
        ),
        "invalid mandatory job inventory",
    )
    require(
        len(set(jobs)) == 16
        and WINDOWS in jobs
        and "CLI And HTTP Smoke" in jobs
        and MANUAL not in jobs,
        "mandatory native job inventory differs",
    )
    require(rule["auxiliary_sources"] == {}, "unimplemented auxiliary source policy")
    manual = re.search(
        r"(?ms)^  windows-native:\r?\n(.*?)(?=^  [A-Za-z0-9_-]+:\r?$|\Z)",
        raw.decode("utf-8-sig"),
    )
    require(manual is not None, "reviewed manual job missing")
    names = re.findall(r"(?m)^    name: ([^\r\n]+)\r?$", manual[1])
    conditions = re.findall(r"(?m)^    if: ([^\r\n]+)\r?$", manual[1])
    require(
        names == [MANUAL]
        and conditions == ["github.event_name == 'workflow_dispatch'"],
        "manual skip predicate changed",
    )
    return rule, raw


class NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, request, response, code, message, headers, new_url):
        return None


class Github:
    """Fixed GitHub GET endpoints; credentials never follow archive redirects."""

    def __init__(self, repository, token, budget=300):
        require(
            re.fullmatch(
                r"[A-Za-z0-9][A-Za-z0-9_.-]*/[A-Za-z0-9][A-Za-z0-9_.-]*", repository
            )
            is not None,
            "invalid repository",
        )
        require(bool(token), "read-only Actions token unavailable")
        self.base = "https://api.github.com/repos/" + repository + "/"
        self.token = token
        self.expires = time.monotonic() + budget
        self.calls = 0
        self.opener = urllib.request.build_opener(NoRedirect)

    def open(self, url, authenticated):
        self.calls += 1
        remaining = self.expires - time.monotonic()
        require(
            self.calls <= MAX_CALLS and remaining > 0,
            "GitHub admission request/deadline bound exceeded",
        )
        headers = {
            "Accept": "application/vnd.github+json",
            "X-GitHub-Api-Version": "2026-03-10",
        }
        if authenticated:
            require(url.startswith(self.base), "API request escaped fixed repository")
            headers["Authorization"] = "Bearer " + self.token
        request = urllib.request.Request(url, headers=headers, method="GET")
        return self.opener.open(request, timeout=min(20, remaining))

    def read(self, response, limit):
        chunks, size = [], 0
        while True:
            require(
                time.monotonic() < self.expires,
                "GitHub response exceeded total deadline",
            )
            chunk = response.read(min(65536, limit - size + 1))
            # A final/EOF read may return after its admitted operation deadline.
            # The socket wait is capped at20s; reject late data and late success.
            require(
                time.monotonic() < self.expires,
                "GitHub response exceeded total deadline",
            )
            if not chunk:
                return b"".join(chunks)
            size += len(chunk)
            require(size <= limit, "GitHub response exceeded size bound")
            chunks.append(chunk)

    def json(self, path):
        require(
            not path.startswith("/") and ".." not in path.split("?", 1)[0].split("/"),
            "invalid API path",
        )
        with self.open(self.base + path, True) as response:
            require(response.status == 200, "unexpected API status")
            return decode_json(self.read(response, MAX_JSON))

    def archive(self, artifact_id):
        try:
            with self.open(self.base + f"actions/artifacts/{artifact_id}/zip", True):
                raise Refused("archive endpoint did not redirect")
        except urllib.error.HTTPError as error:
            require(error.code == 302, "archive redirect unavailable")
            url = error.headers.get("Location", "")
            error.close()
        parsed = urllib.parse.urlsplit(url)
        hostname = parsed.hostname or ""
        require(
            parsed.scheme == "https"
            and not parsed.username
            and not parsed.password
            and parsed.port in (None, 443)
            and not parsed.fragment
            and any(
                hostname.endswith(suffix)
                for suffix in (
                    ".blob.core.windows.net",
                    ".githubusercontent.com",
                    ".actions.githubusercontent.com",
                )
            ),
            "unapproved artifact storage redirect",
        )
        # Use a fresh request without Authorization, even for approved storage.
        with self.open(url, False) as response:
            require(response.status == 200, "archive download failed")
            return self.read(response, MAX_ARCHIVE)


def pages(api, path, key):
    result, total = [], None
    for page in range(1, 5):
        separator = "&" if "?" in path else "?"
        response = api.json(f"{path}{separator}per_page=100&page={page}")
        require(isinstance(response, dict), "API page is not an object")
        rows, count = response.get(key), response.get("total_count")
        require(
            isinstance(rows, list)
            and all(isinstance(row, dict) for row in rows)
            and isinstance(count, int)
            and not isinstance(count, bool)
            and 0 <= count <= 400
            and len(rows) <= 100
            and (total is None or count == total),
            "incomplete/changing API pagination",
        )
        total = count
        result.extend(rows)
        if len(rows) < 100:
            break
    ids = [row.get("id") for row in result]
    require(
        len(result) == total
        and all(positive(value) for value in ids)
        and len(set(ids)) == len(ids),
        "incomplete/duplicate API objects",
    )
    return result


def run_identity(run):
    identity = {
        key: run.get(key)
        for key in (
            "id",
            "workflow_id",
            "head_sha",
            "head_branch",
            "event",
            "run_number",
            "run_attempt",
            "status",
            "conclusion",
            "path",
            "name",
            "created_at",
            "run_started_at",
            "updated_at",
        )
    }
    for key in ("repository", "head_repository"):
        repo = run.get(key, {})
        identity[key] = {field: repo.get(field) for field in ("id", "full_name")}
    return identity


def choose_run(api, repository, revision):
    workflow = api.json("actions/workflows/ci.yml")
    require(
        workflow.get("path") == CI_PATH
        and workflow.get("name") == "CI"
        and positive(workflow.get("id")),
        "CI workflow identity differs",
    )
    runs = pages(
        api,
        f"actions/workflows/{workflow['id']}/runs?head_sha={revision}&branch=main&event=push",
        "workflow_runs",
    )
    require(bool(runs), "no exact-commit main push CI exists")
    for run in runs:
        require(
            run.get("head_sha") == revision
            and run.get("head_branch") == "main"
            and run.get("event") == "push"
            and run.get("workflow_id") == workflow["id"]
            and run.get("name") == "CI"
            and run.get("path") in (CI_PATH, CI_PATH + "@main")
            and run.get("repository", {}).get("full_name") == repository
            and run.get("head_repository", {}).get("full_name") == repository,
            "CI run source/event/repository mismatch",
        )
        timestamp(run.get("created_at"))
    selected = max(runs, key=lambda run: (timestamp(run["created_at"]), run["id"]))
    # Do not select an older success when a newer run or attempt is incomplete/failed.
    run = api.json(f"actions/runs/{selected['id']}")
    require(
        run_identity(run) == run_identity(selected),
        "CI attempt changed during selection",
    )
    require(
        run.get("status") == "completed"
        and run.get("conclusion") == "success"
        and positive(run.get("run_attempt"))
        and positive(run.get("run_number")),
        "latest exact main CI is not successful",
    )
    return run


def verify_source(api, revision, expected):
    source = api.json(f"contents/{CI_PATH}?ref={revision}")
    size, blob = source.get("size"), source.get("sha")
    require(
        source.get("type") == "file"
        and source.get("path") == CI_PATH
        and source.get("encoding") == "base64"
        and positive(size)
        and size <= 1048576
        and isinstance(source.get("content"), str)
        and isinstance(blob, str)
        and SHA.fullmatch(blob),
        "unverified CI source response",
    )
    raw = base64.b64decode(
        source["content"].replace("\n", "").replace("\r", ""), validate=True
    )
    require(
        len(raw) == size
        and hashlib.sha1(b"blob " + str(size).encode() + b"\0" + raw).hexdigest()
        == blob
        and sha256(raw) == expected,
        "raw CI source differs from reviewed producer",
    )


def jobs_snapshot(api, run, required):
    jobs = pages(
        api, f"actions/runs/{run['id']}/attempts/{run['run_attempt']}/jobs", "jobs"
    )
    names = [job.get("name") for job in jobs]
    require(
        len(names) == 17
        and len(set(names)) == 17
        and set(names) == {*required, MANUAL},
        "mandatory job inventory missing/duplicated/unknown",
    )
    keys = (
        "id",
        "name",
        "run_id",
        "run_attempt",
        "head_sha",
        "status",
        "conclusion",
        "started_at",
        "completed_at",
        "runner_name",
    )
    snapshot = {}
    for job in jobs:
        require(
            job.get("run_id") == run["id"]
            and job.get("run_attempt") == run["run_attempt"]
            and job.get("head_sha") == run["head_sha"]
            and job.get("status") == "completed",
            "job head/run/attempt/status mismatch",
        )
        require(
            job.get("conclusion")
            == ("skipped" if job["name"] == MANUAL else "success"),
            "mandatory job not successful or manual skip not explicit",
        )
        if job["name"] != MANUAL:
            require(
                timestamp(job.get("started_at")) <= timestamp(job.get("completed_at")),
                "invalid job execution interval",
            )
        snapshot[job["name"]] = {key: job.get(key) for key in keys}
    return snapshot


def artifact_snapshot(api, run, windows):
    artifacts = pages(api, f"actions/runs/{run['id']}/artifacts", "artifacts")
    candidates = [
        artifact for artifact in artifacts if artifact.get("name") == ARTIFACT
    ]
    require(len(candidates) == 1, "native artifact missing/ambiguous")
    artifact = api.json(f"actions/artifacts/{candidates[0]['id']}")
    require(artifact == candidates[0], "artifact metadata changed during selection")
    identity = artifact.get("workflow_run", {})
    require(
        artifact.get("name") == ARTIFACT
        and artifact.get("expired") is False
        and positive(artifact.get("size_in_bytes"))
        and artifact["size_in_bytes"] <= MAX_ARCHIVE
        and isinstance(artifact.get("digest"), str)
        and re.fullmatch(r"sha256:[a-f0-9]{64}", artifact["digest"]),
        "artifact unavailable or archive digest missing",
    )
    require(
        identity.get("id") == run["id"]
        and identity.get("head_sha") == run["head_sha"]
        and identity.get("head_branch") == "main"
        and identity.get("repository_id") == run.get("repository", {}).get("id")
        and identity.get("head_repository_id")
        == run.get("head_repository", {}).get("id"),
        "artifact source identity differs",
    )
    require(
        timestamp(windows["started_at"])
        <= timestamp(artifact.get("created_at"))
        <= timestamp(artifact.get("updated_at"))
        <= timestamp(windows["completed_at"]),
        "artifact does not belong to current Windows job execution",
    )
    return artifact


def verified_members(archive, revision, runner):
    require(len(archive) <= MAX_ARCHIVE, "archive size bound exceeded")
    with zipfile.ZipFile(io.BytesIO(archive)) as bundle:
        entries = bundle.infolist()
        require(
            len(entries) == 4 and {entry.filename for entry in entries} == MEMBERS,
            "archive contains duplicate/unexpected/traversing files",
        )
        require(
            sum(entry.file_size for entry in entries) <= MAX_ARCHIVE,
            "expanded archive size bound exceeded",
        )
        data = {}
        for entry in entries:
            mode = stat.S_IFMT(entry.external_attr >> 16)
            require(
                not entry.is_dir()
                and mode in (0, stat.S_IFREG)
                and not entry.flag_bits & 1
                and entry.compress_type in (zipfile.ZIP_STORED, zipfile.ZIP_DEFLATED),
                "archive link/encryption/compression unsupported",
            )
            with bundle.open(entry) as stream:
                content = stream.read(entry.file_size + 1)
            require(len(content) == entry.file_size, "expanded member size differs")
            data[entry.filename] = content
    manifest = decode_json(data["artifact-sha256.json"])
    require(
        isinstance(manifest, dict)
        and set(manifest) == {"revision", "target", "validation", "runner", "binaries"},
        "native manifest schema differs",
    )
    require(
        manifest["revision"] == revision
        and manifest["target"] == "x86_64-pc-windows-msvc"
        and manifest["validation"] == "native-windows"
        and manifest["runner"] == runner
        and isinstance(runner, str)
        and runner
        and not any(ord(c) < 32 for c in runner),
        "native manifest provenance differs",
    )
    require(
        isinstance(manifest["binaries"], dict)
        and set(manifest["binaries"]) == set(BINARIES),
        "native binary inventory differs",
    )
    for name in BINARIES:
        digest = manifest["binaries"][name]
        require(
            isinstance(digest, str)
            and DIGEST.fullmatch(digest)
            and sha256(data[name]) == digest,
            "native binary digest differs",
        )
        raw = data[name]
        require(len(raw) >= 64 and raw[:2] == b"MZ", "native artifact has no PE header")
        offset = int.from_bytes(raw[60:64], "little")
        require(
            raw[offset : offset + 6] == b"PE\0\0\x64\x86",
            "native artifact is not Windows x64",
        )
    return data, manifest


def admit(api, repository, revision, repo_root):
    require(SHA.fullmatch(revision) is not None, "release revision must be full commit")
    rule, _raw = policy(repo_root)
    run = choose_run(api, repository, revision)
    verify_source(api, revision, rule["source_sha256"])
    jobs = jobs_snapshot(api, run, rule["required_jobs"])
    artifact = artifact_snapshot(api, run, jobs[WINDOWS])
    archive = api.archive(artifact["id"])
    require(
        len(archive) == artifact["size_in_bytes"]
        and "sha256:" + sha256(archive) == artifact["digest"],
        "downloaded archive HTTP digest/size differs",
    )
    data, manifest = verified_members(archive, revision, jobs[WINDOWS]["runner_name"])
    # All run/attempt IDs and job start/completion witnesses are re-read after download.
    require(
        run_identity(api.json(f"actions/runs/{run['id']}")) == run_identity(run),
        "CI attempt changed during archive download",
    )
    require(
        jobs_snapshot(api, run, rule["required_jobs"]) == jobs,
        "job execution snapshot changed during archive download",
    )
    require(
        api.json(f"actions/artifacts/{artifact['id']}") == artifact,
        "artifact changed during archive download",
    )
    require(
        run_identity(choose_run(api, repository, revision)) == run_identity(run),
        "newer exact-commit CI appeared during download",
    )
    receipt = {
        "schema_version": 1,
        "origin": "accepted-main-ci-artifact-reuse",
        "recompiled": False,
        "repository": repository,
        "revision": revision,
        "ci_source_sha256": rule["source_sha256"],
        "run": run_identity(run),
        "jobs": jobs,
        "artifact": artifact,
        "manifest": manifest,
    }
    # Retain original archive bytes for the post-runtime-check staging check.
    data["ci-archive.zip"] = archive
    return data, receipt


def write_bundle(output, data, receipt):
    require(set(data) == MEMBERS | {"ci-archive.zip"}, "bundle files differ")
    require(
        not output.exists() and not output.is_symlink(),
        "artifact destination already exists",
    )
    output.mkdir(parents=True)
    for name, content in data.items():
        (output / name).write_bytes(content)
    (output / "ci-admission.json").write_text(
        json.dumps(receipt, indent=2), encoding="utf-8"
    )


def stage(artifact_dir, dist, revision):
    receipt = decode_json((artifact_dir / "ci-admission.json").read_bytes())
    require(
        receipt.get("origin") == "accepted-main-ci-artifact-reuse"
        and receipt.get("recompiled") is False
        and receipt.get("revision") == revision,
        "admission receipt differs",
    )
    archive_path = artifact_dir / "ci-archive.zip"
    require(
        archive_path.is_file()
        and not archive_path.is_symlink()
        and archive_path.stat().st_size <= MAX_ARCHIVE,
        "staging archive unavailable",
    )
    archive = archive_path.read_bytes()
    require(
        "sha256:" + sha256(archive) == receipt["artifact"]["digest"]
        and len(archive) == receipt["artifact"]["size_in_bytes"],
        "archive changed before staging",
    )
    _members, manifest = verified_members(
        archive, revision, receipt["manifest"]["runner"]
    )
    require(manifest == receipt["manifest"], "admission manifest changed")
    require(
        manifest["revision"] == revision
        and manifest["target"] == "x86_64-pc-windows-msvc"
        and manifest["validation"] == "native-windows",
        "staging native provenance differs",
    )
    require(
        not dist.exists() and not dist.is_symlink(),
        "release staging destination already exists",
    )
    verified = {}
    for name in BINARIES:
        source = artifact_dir / name
        require(
            source.is_file() and not source.is_symlink(),
            "artifact changed before staging",
        )
        verified[name] = source.read_bytes()
        require(
            sha256(verified[name]) == manifest["binaries"][name],
            "artifact changed before staging",
        )
    require(
        decode_json((artifact_dir / "artifact-sha256.json").read_bytes()) == manifest,
        "manifest changed before staging",
    )
    dist.mkdir(parents=True)
    sums = []
    for name in BINARIES:
        destination = name[:-4] + "-windows-x64.exe"
        (dist / destination).write_bytes(verified[name])
        sums.append(f"{manifest['binaries'][name]}  {destination}\n")
    (dist / "SHA256SUMS-windows-x64").write_text(
        "".join(sorted(sums)), encoding="utf-8"
    )
    metadata = {
        "commit": revision,
        "target": manifest["target"],
        "abi": "msvc",
        "validation": "native-windows",
        "native_windows_validated": "true",
        "host_os": "Windows",
        "host_arch": "X64",
        "artifact_origin": receipt["origin"],
        "recompiled": "false",
        "ci_run_id": receipt["run"]["id"],
        "ci_run_attempt": receipt["run"]["run_attempt"],
        "ci_artifact_id": receipt["artifact"]["id"],
        "ci_archive_digest": receipt["artifact"]["digest"],
        "ci_workflow_sha256": receipt["ci_source_sha256"],
        "ci_build_runner": manifest["runner"],
        "configured_shipping_mcp_validation": "native-17-tools",
        "native_windows_dll_validation": "not-separately-tested",
    }
    (dist / "BUILD-windows-x64.txt").write_text(
        "".join(f"{key}={value}\n" for key, value in metadata.items()), encoding="utf-8"
    )


def refresh_and_stage(api, repository, revision, repo_root, artifact_dir, dist):
    """Re-admit current metadata after native execution, without running binaries."""
    require(SHA.fullmatch(revision) is not None, "release revision must be full commit")
    receipt = decode_json((artifact_dir / "ci-admission.json").read_bytes())
    require(
        receipt.get("repository") == repository and receipt.get("revision") == revision,
        "staging admission source differs",
    )
    rule, _raw = policy(repo_root)
    require(
        rule["source_sha256"] == receipt.get("ci_source_sha256"),
        "reviewed producer changed before staging",
    )
    run = choose_run(api, repository, revision)
    require(run_identity(run) == receipt["run"], "latest CI changed before staging")
    verify_source(api, revision, rule["source_sha256"])
    jobs = jobs_snapshot(api, run, rule["required_jobs"])
    require(jobs == receipt["jobs"], "CI jobs changed before staging")
    artifact = artifact_snapshot(api, run, jobs[WINDOWS])
    require(artifact == receipt["artifact"], "CI artifact changed before staging")
    require(
        run_identity(choose_run(api, repository, revision)) == receipt["run"],
        "latest CI changed during staging admission",
    )
    # Existing strict archive/manifest/all-three-byte checks run after fresh API
    # admission. No native executable is started while a token is in scope.
    stage(artifact_dir, dist, revision)
    (artifact_dir / "ci-stage-admission.json").write_text(
        json.dumps(
            {
                "status": "PASS",
                "run": run_identity(run),
                "jobs": jobs,
                "artifact": artifact,
                "ci_source_sha256": rule["source_sha256"],
            },
            indent=2,
        ),
        encoding="utf-8",
    )


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("operation", choices=("download", "stage"))
    parser.add_argument("--repo-root", required=True, type=Path)
    parser.add_argument("--repository", required=True)
    parser.add_argument("--revision", required=True)
    parser.add_argument("--artifact-dir", required=True, type=Path)
    parser.add_argument("--dist", type=Path)
    args = parser.parse_args()
    try:
        if args.operation == "download":
            api = Github(args.repository, os.environ.get("GH_TOKEN", ""))
            data, receipt = admit(api, args.repository, args.revision, args.repo_root)
            write_bundle(args.artifact_dir, data, receipt)
            print(
                "Accepted native CI artifact "
                f"run={receipt['run']['id']} "
                f"attempt={receipt['run']['run_attempt']} "
                f"artifact={receipt['artifact']['id']}; recompiled=false"
            )
        else:
            require(args.dist is not None, "staging directory required")
            api = Github(args.repository, os.environ.get("GH_TOKEN", ""))
            refresh_and_stage(
                api,
                args.repository,
                args.revision,
                args.repo_root,
                args.artifact_dir,
                args.dist,
            )
    except (
        Refused,
        OSError,
        KeyError,
        TypeError,
        json.JSONDecodeError,
        zipfile.BadZipFile,
    ) as error:
        # Never print exception payloads, URLs, configured data or token values.
        print(
            "Native release artifact admission refused: "
            + (str(error) if isinstance(error, Refused) else type(error).__name__)
        )
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
