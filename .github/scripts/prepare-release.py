#!/usr/bin/env python3
"""Offline final verification of the three Ku release assets (Python 3.11+)."""

from __future__ import annotations

import argparse
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import re
import sys
import tarfile
import tempfile
import tomllib

sys.dont_write_bytecode = True
MAX_BYTES = 1024 * 1024 * 1024
MAX_JSON = 2 * 1024 * 1024
MAX_FILES = 1000
TARGETS = (
    "aarch64-apple-darwin",
    "x86_64-pc-windows-msvc",
    "x86_64-unknown-linux-gnu",
)
TAG = re.compile(r"v0\.(?:0|[1-9][0-9]{0,8})\.(?:0|[1-9][0-9]{0,8})\Z")
COMMIT = re.compile(r"[0-9a-f]{40}\Z")
SHA256 = re.compile(r"[0-9a-f]{64}\Z")
REPOSITORY = re.compile(r"[A-Za-z0-9][A-Za-z0-9-]{0,38}/[A-Za-z0-9_.-]{1,100}\Z")


class PrepareError(ValueError):
    pass


def load_exporter():
    # Never import executable code from the downloaded assets or a manifest.
    path = Path(__file__).with_name("export-release.py")
    spec = importlib.util.spec_from_file_location("ku_trusted_release_export", path)
    if spec is None or spec.loader is None:
        raise PrepareError("trusted release exporter is unavailable")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


EXPORTER = load_exporter()


def identity(info):
    # Python 3.14 on Windows reports creation time through Path.stat().st_ctime
    # but change time through fstat().st_ctime. Use the explicit birth time on
    # Windows so the same open file does not look like a substituted input.
    stamp = getattr(info, "st_birthtime_ns", 0) if os.name == "nt" else info.st_ctime_ns
    return (info.st_dev, info.st_ino, info.st_size, info.st_mtime_ns, stamp)


def read_file(path: Path, limit: int) -> bytes:
    before = EXPORTER.plain(path)
    if before.st_size > limit:
        raise PrepareError(f"file exceeds size limit: {path.name}")
    with path.open("rb") as source:
        if identity(os.fstat(source.fileno())) != identity(before):
            raise PrepareError(f"input changed before reading: {path.name}")
        value = source.read(limit + 1)
        if len(value) > limit or identity(os.fstat(source.fileno())) != identity(before):
            raise PrepareError(f"input changed while reading: {path.name}")
    if len(value) != before.st_size or identity(EXPORTER.plain(path)) != identity(before):
        raise PrepareError(f"input changed after reading: {path.name}")
    return value


def json_object(value: bytes):
    def unique(pairs):
        result = {}
        for key, item in pairs:
            if key in result:
                raise PrepareError(f"duplicate JSON field: {key}")
            result[key] = item
        return result

    def invalid_constant(_):
        raise PrepareError("non-finite JSON number is not allowed")

    result = json.loads(value.decode("utf-8"), object_pairs_hook=unique,
                        parse_constant=invalid_constant)
    if not isinstance(result, dict):
        raise PrepareError("JSON root must be an object")
    return result


def repo_version(repo: Path, version: str) -> str:
    EXPORTER.plain(repo, directory=True)
    cargo = tomllib.loads(read_file(repo / "Cargo.toml", MAX_JSON).decode("utf-8-sig"))
    lock = tomllib.loads(read_file(repo / "Cargo.lock", MAX_JSON).decode("utf-8-sig"))
    packages = [item for item in lock.get("package", []) if item.get("name") == "ku"]
    if (cargo.get("package", {}).get("name") != "ku"
            or cargo["package"].get("version") != version
            or len(packages) != 1 or packages[0].get("version") != version):
        raise PrepareError("tag does not match Cargo package and lockfile versions")
    extension = repo / "editors" / "vscode-ku"
    package = json_object(read_file(extension / "package.json", MAX_JSON))
    lock = json_object(read_file(extension / "package-lock.json", MAX_JSON))
    if (package.get("version") != version or lock.get("version") != version
            or lock.get("packages", {}).get("", {}).get("version") != version):
        raise PrepareError("tag does not match editor package and lockfile versions")
    repository = package.get("repository", {})
    url = repository.get("url") if isinstance(repository, dict) else None
    match = re.fullmatch(r"https://github\.com/([^/]+/[^/]+)\.git", url or "")
    if not match or not REPOSITORY.fullmatch(match[1]) or match[1].split("/")[1] in (".", ".."):
        raise PrepareError("repository metadata must identify one public GitHub repository")
    return match[1]


def ci_url(expected_repository: str) -> str:
    server = os.environ.get("GITHUB_SERVER_URL", "")
    repository = os.environ.get("GITHUB_REPOSITORY", "")
    run_id = os.environ.get("GITHUB_RUN_ID", "")
    if (server != "https://github.com" or not REPOSITORY.fullmatch(repository)
            or repository.casefold() != expected_repository.casefold()
            or not re.fullmatch(r"[1-9][0-9]{0,19}", run_id)):
        raise PrepareError("invalid or mismatched GitHub CI identity")
    return f"{server}/{repository}/actions/runs/{run_id}"


def inventory(assets: Path, expected: set[str]) -> dict:
    EXPORTER.plain(assets, directory=True)
    found = {}
    total = 0
    with os.scandir(assets) as entries:
        for entry in entries:
            if len(found) >= 6 or entry.name not in expected:
                raise PrepareError(f"unexpected release asset: {entry.name!r}")
            info = EXPORTER.plain(Path(entry.path))
            if not 0 < info.st_size <= MAX_BYTES:
                raise PrepareError(f"release asset size is invalid: {entry.name}")
            total += info.st_size
            if total > MAX_BYTES:
                raise PrepareError("combined release inputs exceed 1 GiB")
            found[entry.name] = identity(info)
    if set(found) != expected:
        raise PrepareError("release requires exactly three archives and three manifests")
    return found


def validate_manifest(manifest: dict, version: str, target: str, commit: str, asset: str):
    required = {"version", "target", "commit", "asset", "sha256", "size", "files"}
    optional = {"format", "rust_toolchain", "status", "ku_license"}
    if not required.issubset(manifest) or set(manifest) - required - optional:
        raise PrepareError("manifest field set is invalid")
    for name, value in (("version", version), ("target", target), ("commit", commit), ("asset", asset)):
        if manifest[name] != value:
            raise PrepareError(f"manifest {name} does not match release identity")
    if (type(manifest["size"]) is not int or not 0 < manifest["size"] <= MAX_BYTES
            or not isinstance(manifest["sha256"], str) or not SHA256.fullmatch(manifest["sha256"])):
        raise PrepareError("manifest compressed size/hash is invalid")
    files = manifest["files"]
    if not isinstance(files, dict) or not 0 < len(files) <= MAX_FILES or "RELEASE.json" not in files:
        raise PrepareError("manifest files are missing or exceed the limit")
    folded = set()
    total = 0
    for name, spec in files.items():
        EXPORTER.checked_relative(name)
        if name.casefold() in folded:
            raise PrepareError("manifest contains case-colliding file names")
        folded.add(name.casefold())
        if (not isinstance(spec, dict) or set(spec) != {"size", "sha256", "mode"}
                or type(spec["size"]) is not int or spec["size"] < 0
                or type(spec["mode"]) is not int or spec["mode"] not in (0o644, 0o755)
                or not isinstance(spec["sha256"], str) or not SHA256.fullmatch(spec["sha256"])):
            raise PrepareError("manifest file specification is invalid")
        total += spec["size"]
        if total > MAX_BYTES:
            raise PrepareError("manifest expanded size exceeds 1 GiB")


def copy_verified(archive: Path, snapshot: Path, expected: dict, original: tuple):
    if identity(EXPORTER.plain(archive)) != original:
        raise PrepareError("archive changed before verification")
    size = 0
    digest = hashlib.sha256()
    with archive.open("rb") as source, snapshot.open("xb") as output:
        if identity(os.fstat(source.fileno())) != original:
            raise PrepareError("archive was replaced before reading")
        while chunk := source.read(1024 * 1024):
            size += len(chunk)
            if size > MAX_BYTES or size > expected["size"]:
                raise PrepareError("compressed archive exceeds its declared size")
            digest.update(chunk)
            output.write(chunk)
        if identity(os.fstat(source.fileno())) != original:
            raise PrepareError("archive changed while reading")
    if (size != expected["size"] or digest.hexdigest() != expected["sha256"]
            or identity(EXPORTER.plain(archive)) != original):
        raise PrepareError("compressed archive size/hash mismatch")


def create_reports(assets: Path, checksums: bytes, notes: bytes):
    created = []
    try:
        # Both paths must be newly created; never replace a previous report.
        for name, value in (("SHA256SUMS", checksums), ("RELEASE-NOTES.md", notes)):
            path = assets / name
            with path.open("xb") as output:
                info = os.fstat(output.fileno())
                created.append((path, info.st_dev, info.st_ino))
                output.write(value)
                output.flush()
                os.fsync(output.fileno())
    except BaseException:
        for path, device, inode in reversed(created):
            try:
                info = path.lstat()
                if not path.is_symlink() and (info.st_dev, info.st_ino) == (device, inode):
                    path.unlink()
            except OSError:
                pass
        raise


def prepare_release(assets: Path, tag: str, commit: str, repo: Path) -> dict:
    if not TAG.fullmatch(tag) or not COMMIT.fullmatch(commit):
        raise PrepareError("expected a canonical v0.x.y tag and lowercase 40-character commit")
    version = tag[1:]
    repository = repo_version(repo, version)
    run_url = ci_url(repository)
    notes = read_file(repo / "docs" / f"v{version}.md", MAX_JSON).decode("utf-8")
    archives = {target: f"ku-{tag}-{target}.tar.gz" for target in TARGETS}
    expected = set(archives.values()) | {f"{name}.manifest.json" for name in archives.values()}
    original = inventory(assets, expected)
    checksums = {}
    for target, name in archives.items():
        manifest_name = f"{name}.manifest.json"
        data = read_file(assets / manifest_name, MAX_JSON)
        manifest = json_object(data)
        validate_manifest(manifest, version, target, commit, name)
        checksums[manifest_name] = hashlib.sha256(data).hexdigest()
        with tempfile.TemporaryDirectory(prefix="ku-release-verify-") as temporary:
            # macOS's system temp root may itself be reached via /var -> /private/var.
            # Canonicalize only our own newly created directory, never caller inputs.
            temporary = Path(temporary).resolve()
            snapshot = temporary / name
            copy_verified(assets / name, snapshot, manifest, original[name])
            extracted = temporary / "extracted"
            extracted.mkdir()
            EXPORTER.verify_archive(snapshot, manifest["files"], extracted)
            release = json_object(read_file(extracted / "RELEASE.json", MAX_JSON))
            if any(release.get(key) != value for key, value in (
                    ("version", version), ("target", target), ("commit", commit))):
                raise PrepareError("archive RELEASE.json identity mismatch")
        checksums[name] = manifest["sha256"]
    if inventory(assets, expected) != original:
        raise PrepareError("release inputs changed during verification")
    sums = "".join(f"{digest}  {name}\n" for name, digest in sorted(checksums.items()))
    notes = (notes.rstrip() + f"\n\n## Release verification\n\n"
             f"- Tag: `{tag}`\n- Commit: `{commit}`\n"
             f"- Actual CI run: [GitHub Actions]({run_url})\n")
    create_reports(assets, sums.encode("utf-8"), notes.encode("utf-8"))
    return {"version": version, "commit": commit, "targets": list(TARGETS),
            "checksums": str(assets / "SHA256SUMS"),
            "notes": str(assets / "RELEASE-NOTES.md"), "ci_url": run_url}


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--assets", required=True, type=Path)
    parser.add_argument("--tag", required=True)
    parser.add_argument("--commit", required=True)
    parser.add_argument("--repo", required=True, type=Path)
    arguments = parser.parse_args()
    try:
        result = prepare_release(**vars(arguments))
    except (PrepareError, EXPORTER.ExportError, OSError, ValueError, KeyError,
            TypeError, AttributeError, RecursionError, tarfile.TarError, EOFError) as error:
        print(f"release preparation failed: {error}", file=sys.stderr)
        return 1
    print(json.dumps(result, sort_keys=True))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
