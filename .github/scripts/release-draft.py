#!/usr/bin/env python3
"""Stage verified release bundles on the draft owned by this tag run."""

from __future__ import annotations

import argparse
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import re
import secrets
import sys
import tempfile
import time
import urllib.error
import urllib.parse
import urllib.request

sys.dont_write_bytecode = True
API = "https://api.github.com"
MAX_METADATA = 2 * 1024 * 1024
NUMBER = re.compile(r"[1-9][0-9]{0,19}\Z")


class ReleaseError(ValueError):
    pass


class NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, req, fp, code, msg, headers, newurl):
        return None


def load_prepare():
    path = Path(__file__).with_name("prepare-release.py")
    spec = importlib.util.spec_from_file_location("ku_release_prepare", path)
    if spec is None or spec.loader is None:
        raise ReleaseError("release verifier is unavailable")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


PREPARE = load_prepare()


def open_response(request, allowed_errors=(), timeout=15):
    try:
        return urllib.request.build_opener(NoRedirect()).open(request, timeout=timeout)
    except urllib.error.HTTPError as error:
        if error.code in allowed_errors:
            return error
        error.close()
        raise ReleaseError(f"GitHub {request.get_method()} returned HTTP {error.code}") from None
    except (urllib.error.URLError, TimeoutError):
        raise ReleaseError(f"GitHub {request.get_method()} did not complete") from None


def read_json(response):
    body = response.read(MAX_METADATA + 1)
    if len(body) > MAX_METADATA:
        raise ReleaseError("GitHub metadata exceeds its limit")
    return json.loads(body)


class ReleaseRun:
    def __init__(self, repo):
        self.repo = repo.resolve()
        self.tag = os.environ.get("GITHUB_REF_NAME", "")
        self.commit = os.environ.get("GITHUB_SHA", "")
        self.run = os.environ.get("GITHUB_RUN_ID", "")
        self.attempt = os.environ.get("GITHUB_RUN_ATTEMPT", "")
        match = PREPARE.TAG.fullmatch(self.tag)
        if (os.environ.get("GITHUB_EVENT_NAME") != "push"
                or os.environ.get("GITHUB_REF") != f"refs/tags/{self.tag}"
                or not match or not PREPARE.COMMIT.fullmatch(self.commit)
                or not NUMBER.fullmatch(self.run) or not NUMBER.fullmatch(self.attempt)):
            raise ReleaseError("expected one canonical release tag push identity")
        self.version = match["version"]
        self.repository = PREPARE.repo_version(self.repo, self.version)
        PREPARE.ci_url(self.repository)
        self.base = f"/repos/{self.repository}"
        self.title = f"Ku {self.tag}（实验预发行版）"
        token = os.environ.get("GH_TOKEN", "")
        if not token:
            raise ReleaseError("GitHub Release token is unavailable")
        self.headers = {"Authorization": "Bearer " + token,
                        "Accept": "application/vnd.github+json",
                        "X-GitHub-Api-Version": "2022-11-28"}
        self.release_id = os.environ.get("KU_RELEASE_ID", "")
        self.nonce = os.environ.get("KU_RELEASE_NONCE", "")

    @property
    def label(self):
        return f"ku-run-{self.run}-{self.attempt}-{self.nonce}"

    @property
    def marker(self):
        return f"<!-- {self.label} {self.commit} -->"

    @property
    def endpoint(self):
        if not NUMBER.fullmatch(self.release_id) or not re.fullmatch(r"[0-9a-f]{32}", self.nonce):
            raise ReleaseError("missing exact draft identity from this run")
        return f"/releases/{self.release_id}"

    def api(self, method, path, expected=200, payload=None):
        headers = dict(self.headers)
        data = None
        if payload is not None:
            headers["Content-Type"] = "application/json"
            data = json.dumps(payload).encode("utf-8")
        request = urllib.request.Request(API + self.base + path, data=data, headers=headers, method=method)
        with open_response(request) as response:
            if response.status != expected:
                raise ReleaseError("unexpected GitHub metadata response")
            return None if expected == 204 else read_json(response)

    def check_tag(self):
        ref = self.api("GET", f"/git/ref/tags/{self.tag}")
        if ref.get("ref") != f"refs/tags/{self.tag}":
            raise ReleaseError("the requested Git tag is missing or differs")
        obj = ref.get("object", {})
        for _ in range(8):
            sha = obj.get("sha", "")
            if not isinstance(sha, str) or not PREPARE.COMMIT.fullmatch(sha):
                break
            if obj.get("type") == "commit" and sha == self.commit:
                return
            if obj.get("type") != "tag":
                break
            obj = self.api("GET", f"/git/tags/{sha}").get("object", {})
        raise ReleaseError("release tag does not resolve to the checked-out commit")

    def releases_for_tag(self):
        # Listing with push permission includes drafts. Tag lookup alone may
        # only find a published release.
        for page in range(1, 101):
            releases = self.api("GET", f"/releases?per_page=100&page={page}")
            if not isinstance(releases, list) or len(releases) > 100:
                raise ReleaseError("unexpected release listing")
            yield from (release for release in releases if release.get("tag_name") == self.tag)
            if len(releases) < 100:
                return
        raise ReleaseError("release listing exceeds its limit")

    def owned(self):
        draft = self.api("GET", self.endpoint)
        if (draft.get("id") != int(self.release_id) or draft.get("tag_name") != self.tag
                or draft.get("draft") is not True or draft.get("prerelease") is not True
                or draft.get("body") != self.marker or draft.get("name") != self.title):
            raise ReleaseError("release does not match this run's draft")
        return draft

    def create(self):
        self.check_tag()
        PREPARE.read_file(self.repo / "docs" / f"{self.tag}.md", PREPARE.MAX_JSON).decode("utf-8")
        if any(self.releases_for_tag()):
            raise ReleaseError("a release or draft already exists for this tag")
        self.nonce = secrets.token_hex(16)
        draft = self.api("POST", "/releases", 201, {
            "tag_name": self.tag, "target_commitish": self.commit, "name": self.title,
            "body": self.marker, "draft": True, "prerelease": True,
        })
        self.release_id = str(draft.get("id", ""))
        try:
            self.owned()
            with open(os.environ["GITHUB_OUTPUT"], "a", encoding="utf-8") as output:
                output.write(f"release-id={self.release_id}\nrelease-nonce={self.nonce}\n")
        except BaseException:
            if NUMBER.fullmatch(self.release_id):
                self.cleanup()
            raise
        print(f"Created release draft {self.release_id} for {self.tag}")

    def archive_name(self, target):
        return f"ku-v{self.version}-{target}.tar.gz"

    def expected_assets(self):
        archives = {self.archive_name(target) for target in PREPARE.TARGETS}
        return archives | {name + ".manifest.json" for name in archives}

    def list_assets(self):
        values = self.api("GET", self.endpoint + "/assets?per_page=100")
        if not isinstance(values, list) or len(values) >= 100:
            raise ReleaseError("asset listing exceeds its limit")
        result = {asset.get("name"): asset for asset in values}
        if len(result) != len(values):
            raise ReleaseError("duplicate release asset names")
        return result

    def asset_identity(self, asset):
        asset_id, size, digest = (asset.get(key) for key in ("id", "size", "digest"))
        if (type(asset_id) is not int or not NUMBER.fullmatch(str(asset_id))
                or type(size) is not int or not 0 < size <= PREPARE.MAX_BYTES
                or not isinstance(digest, str) or not digest.startswith("sha256:")
                or not PREPARE.SHA256.fullmatch(digest[7:]) or asset.get("state") != "uploaded"
                or asset.get("label") != self.label):
            raise ReleaseError("asset does not have this run's completed upload identity")
        return asset_id, size, digest[7:]

    def upload(self, path, digest):
        draft = self.owned()
        endpoint = "https://uploads.github.com" + self.base + self.endpoint + "/assets"
        template = draft.get("upload_url", "")
        if not isinstance(template, str) or template.casefold() != (endpoint + "{?name,label}").casefold():
            raise ReleaseError("GitHub returned an unexpected asset upload URL")
        info = PREPARE.EXPORTER.plain(path)
        if not 0 < info.st_size <= PREPARE.MAX_BYTES:
            raise ReleaseError("release asset size exceeds its limit")
        if PREPARE.EXPORTER.file_hash(path) != digest:
            raise ReleaseError("local release asset does not match its verified digest")
        query = urllib.parse.urlencode({"name": path.name, "label": self.label})
        headers = dict(self.headers, **{"Content-Type": "application/octet-stream",
                                        "Content-Length": str(info.st_size)})
        with path.open("rb") as data:
            if PREPARE.identity(os.fstat(data.fileno())) != PREPARE.identity(info):
                raise ReleaseError("local release asset changed before upload")
            request = urllib.request.Request(endpoint + "?" + query, data=data, headers=headers, method="POST")
            with open_response(request, timeout=60) as response:
                if response.status != 201:
                    raise ReleaseError("unexpected GitHub asset upload response")
                asset = read_json(response)
            if PREPARE.identity(os.fstat(data.fileno())) != PREPARE.identity(info):
                raise ReleaseError("local release asset changed during upload")
        if asset.get("name") != path.name or self.asset_identity(asset)[1:] != (info.st_size, digest):
            raise ReleaseError("GitHub upload does not match the verified local file")
        print(f"Staged {path.name} on draft {self.release_id}")

    def stage(self, assets, target, report_path):
        name = self.archive_name(target)
        manifest_path = assets / (name + ".manifest.json")
        manifest_bytes = PREPARE.read_file(manifest_path, PREPARE.MAX_JSON)
        manifest = PREPARE.json_object(manifest_bytes)
        PREPARE.validate_manifest(manifest, self.version, target, self.commit, name)
        manifest_digest = hashlib.sha256(manifest_bytes).hexdigest()
        report = PREPARE.json_object(PREPARE.read_file(report_path, PREPARE.MAX_JSON))
        if (report.get("status") != "passed" or report.get("fixture_cleanup_confirmed") is not True
                or report.get("target") != target or report.get("commit") != self.commit
                or report.get("archive_sha256") != manifest["sha256"]
                or report.get("manifest_sha256") != manifest_digest):
            raise ReleaseError("release files differ from the passing installed-package check")
        self.upload(assets / name, manifest["sha256"])
        self.upload(manifest_path, manifest_digest)

    def download(self, path, identity):
        asset_id, size, digest = identity
        url = API + self.base + f"/releases/assets/{asset_id}"
        headers = dict(self.headers, Accept="application/octet-stream")
        request = urllib.request.Request(url, headers=headers)
        response = open_response(request, (302,), timeout=30)
        if response.status == 302:
            with response:
                location = response.headers.get("Location", "")
            parsed = urllib.parse.urlsplit(location)
            if (parsed.scheme != "https" or not (parsed.hostname or "").endswith(".githubusercontent.com")
                    or parsed.username or parsed.password or parsed.port not in (None, 443)):
                raise ReleaseError("asset download redirected outside GitHub storage")
            response = open_response(urllib.request.Request(location), timeout=30)
        actual = hashlib.sha256()
        remaining = size
        deadline = time.monotonic() + 120
        with response, path.open("xb") as destination:
            if response.status != 200:
                raise ReleaseError("unexpected GitHub asset download response")
            while True:
                chunk = response.read(min(1024 * 1024, remaining + 1))
                if len(chunk) > remaining or time.monotonic() > deadline:
                    raise ReleaseError("release asset download exceeded its bound")
                if not chunk:
                    break
                actual.update(chunk)
                destination.write(chunk)
                remaining -= len(chunk)
        if remaining or actual.hexdigest() != digest:
            raise ReleaseError("downloaded release asset does not match GitHub's identity")

    def snapshot(self, names):
        assets = self.list_assets()
        if set(assets) != names:
            raise ReleaseError("draft does not contain exactly the expected release files")
        result = {name: self.asset_identity(asset) for name, asset in assets.items()}
        if sum(size for _, size, _ in result.values()) > PREPARE.MAX_BYTES:
            raise ReleaseError("total release asset size exceeds its limit")
        return result

    def publish(self):
        self.owned()
        names = self.expected_assets()
        original = self.snapshot(names)
        with tempfile.TemporaryDirectory(prefix="ku-release-final-") as directory:
            assets = Path(directory).resolve()
            for name, identity in sorted(original.items()):
                self.download(assets / name, identity)
            PREPARE.prepare_release(assets, self.tag, self.commit, self.repo)
            sums = assets / "SHA256SUMS"
            sums_digest = PREPARE.EXPORTER.file_hash(sums)
            self.upload(sums, sums_digest)
            current = self.snapshot(names | {sums.name})
            if (any(current[name] != identity for name, identity in original.items())
                    or current[sums.name][1:] != (sums.stat().st_size, sums_digest)):
                raise ReleaseError("remote release inputs changed during verification")
            self.check_tag()
            if [item.get("id") for item in self.releases_for_tag()] != [int(self.release_id)]:
                raise ReleaseError("another release or draft now exists for this tag")
            self.owned()
            notes = PREPARE.read_file(assets / "RELEASE-NOTES.md", PREPARE.MAX_JSON).decode("utf-8")
            published = self.api("PATCH", self.endpoint, payload={"body": notes, "draft": False})
            if (published.get("id") != int(self.release_id) or published.get("draft") is not False
                    or published.get("tag_name") != self.tag or published.get("prerelease") is not True):
                raise ReleaseError("GitHub did not confirm the expected published release")
        print(f"Published https://github.com/{self.repository}/releases/tag/{self.tag}")

    def cleanup(self):
        draft = self.owned()
        allowed = self.expected_assets() | {"SHA256SUMS"}
        for name, asset in self.list_assets().items():
            # GitHub can leave a zero-byte starter after a failed upload.
            starter = (asset.get("state") == "starter" and asset.get("size") == 0
                       and draft.get("author", {}).get("id") is not None
                       and asset.get("uploader", {}).get("id") == draft["author"]["id"])
            if name not in allowed or (asset.get("label") != self.label and not starter):
                raise ReleaseError("draft has an asset outside this run's ownership")
        self.owned()
        self.api("DELETE", self.endpoint, 204)
        print(f"Removed this run's unpublished release draft {self.release_id}")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("action", choices=("create", "stage", "publish", "cleanup"))
    parser.add_argument("--repo", required=True, type=Path)
    parser.add_argument("--assets", type=Path)
    parser.add_argument("--target", choices=PREPARE.TARGETS)
    parser.add_argument("--report", type=Path)
    args = parser.parse_args()
    if args.action == "stage" and (args.assets is None or args.target is None or args.report is None):
        parser.error("stage requires --assets, --target and --report")
    try:
        release = ReleaseRun(args.repo)
        if args.action == "stage":
            release.stage(args.assets, args.target, args.report)
        elif args.action == "create":
            release.create()
        elif args.action == "publish":
            release.publish()
        else:
            release.cleanup()
    except ReleaseError as error:
        print(f"Release {args.action} failed: {error}", file=sys.stderr)
        return 1
    except Exception as error:
        # Do not expose response bodies, signed URLs or authorization data.
        print(f"Release {args.action} failed ({type(error).__name__})", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
