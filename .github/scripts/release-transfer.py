#!/usr/bin/env python3
"""Move checked candidate release files without Actions artifact storage."""
from __future__ import annotations

import argparse
import importlib.util
import json
import os
from pathlib import Path
import re
import subprocess
import sys
import tempfile
import time

sys.dont_write_bytecode = True
TARGETS = ("x86_64-pc-windows-msvc", "x86_64-unknown-linux-gnu", "aarch64-apple-darwin")
MARKER = re.compile(r"<!-- ku-release-candidate (\{[^\n]+\}) -->")
MAX_BYTES = 1024 * 1024 * 1024


class TransferError(ValueError):
    pass


def gh(arguments):
    """Bound both API diagnostics and waiting; credentials stay in GH_TOKEN."""
    with tempfile.TemporaryFile() as output, tempfile.TemporaryFile() as error:
        process = subprocess.Popen(["gh", *arguments], stdin=subprocess.DEVNULL,
                                   stdout=output, stderr=error)
        deadline = time.monotonic() + 180
        try:
            while process.poll() is None:
                if time.monotonic() >= deadline:
                    raise TransferError("GitHub release operation exceeded 180 seconds")
                if any(os.fstat(stream.fileno()).st_size > 1024 * 1024 for stream in (output, error)):
                    raise TransferError("GitHub release operation exceeded its output bound")
                time.sleep(0.1)
            if process.returncode:
                raise TransferError(f"GitHub release operation failed (exit {process.returncode})")
            output.seek(0)
            value = output.read(1024 * 1024 + 1)
            if len(value) > 1024 * 1024:
                raise TransferError("GitHub release metadata exceeded 1 MiB")
            return value.decode("utf-8")
        finally:
            if process.poll() is None:
                process.kill()
            process.wait(timeout=10)


def identity(repository, candidate_tag, version, commit, run_id):
    if (not re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9_.-]{0,99}/[A-Za-z0-9][A-Za-z0-9_.-]{0,99}", repository)
            or not re.fullmatch(r"0\.(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)", version)
            or not re.fullmatch(r"[0-9a-f]{40}", commit)
            or not re.fullmatch(r"[1-9][0-9]{0,19}", str(run_id))):
        raise TransferError("invalid candidate source/run identity")
    prefix = f"ku-v{version}-candidate-{commit[:12]}-"
    if not candidate_tag.startswith(prefix) or not re.fullmatch(r"[a-z0-9][a-z0-9-]{0,40}", candidate_tag[len(prefix):]):
        raise TransferError("candidate_tag must identify this version/commit and a unique nonce; never use the final tag")
    return {"schema": "ku-release-candidate-v1", "repository": repository,
            "tag": candidate_tag, "version": version, "commit": commit, "run_id": str(run_id)}


def marker(value):
    return "<!-- ku-release-candidate " + json.dumps(value, sort_keys=True, separators=(",", ":")) + " -->"


def verify_release(metadata, expected):
    matches = MARKER.findall(metadata.get("body", ""))
    if (metadata.get("tagName") != expected["tag"] or metadata.get("isDraft") is not True
            or metadata.get("targetCommitish") != expected["commit"] or len(matches) != 1
            or json.loads(matches[0]) != expected):
        raise TransferError("candidate is not this run's exact-commit draft Release")
    return metadata


def release(expected):
    raw = gh(["release", "view", expected["tag"], "--repo", expected["repository"],
              "--json", "tagName,isDraft,targetCommitish,body,assets,url"])
    return verify_release(json.loads(raw), expected)


def asset_names(version, target):
    if target not in TARGETS:
        raise TransferError("unsupported candidate target")
    archive = f"ku-v{version}-{target}.tar.gz"
    return archive, archive + ".manifest.json"


def load_prepare():
    spec = importlib.util.spec_from_file_location("ku_candidate_prepare", Path(__file__).with_name("prepare-release.py"))
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def verify_pair(directory, expected, target):
    prepare = load_prepare()
    name, manifest_name = asset_names(expected["version"], target)
    path = directory / name
    manifest = prepare.json_object(prepare.read_file(directory / manifest_name, prepare.MAX_JSON))
    prepare.validate_manifest(manifest, expected["version"], target, expected["commit"], name)
    info = prepare.EXPORTER.plain(path)
    if info.st_size != manifest["size"] or info.st_size > MAX_BYTES:
        raise TransferError("candidate archive size differs from its manifest")
    if prepare.EXPORTER.file_hash(path) != manifest["sha256"]:
        raise TransferError("candidate archive SHA differs from its manifest")
    return name, manifest_name


def create(expected):
    # gh refuses an existing Release; do not reuse/clobber another run's draft.
    with tempfile.TemporaryDirectory(prefix="ku-candidate-notes-") as temporary:
        notes = Path(temporary) / "notes.md"
        notes.write_text("Pending platform and archive verification.\n\n" + marker(expected) + "\n", encoding="utf-8")
        gh(["release", "create", expected["tag"], "--repo", expected["repository"],
            "--draft", "--prerelease", "--target", expected["commit"],
            "--title", "Ku candidate verification", "--notes-file", str(notes)])
    return release(expected)


def upload(directory, expected, target):
    metadata = release(expected)
    names = verify_pair(directory, expected, target)
    if any(row.get("name") in names for row in metadata.get("assets", [])):
        raise TransferError("candidate target assets already exist; refusing overwrite")
    gh(["release", "upload", expected["tag"], "--repo", expected["repository"],
        *(str(directory / name) for name in names)])


def download(directory, expected, target=None):
    metadata = release(expected)
    targets = (target,) if target else TARGETS
    names = [name for item in targets for name in asset_names(expected["version"], item)]
    listed = [row.get("name") for row in metadata.get("assets", [])]
    if any(listed.count(name) != 1 for name in names):
        raise TransferError("candidate is missing a unique archive/manifest asset")
    if directory.exists():
        raise TransferError("candidate download directory must be new")
    directory.mkdir(parents=True)
    arguments = ["release", "download", expected["tag"], "--repo", expected["repository"], "--dir", str(directory)]
    for name in names:
        arguments.extend(["--pattern", name])
    gh(arguments)
    if {item.name for item in directory.iterdir()} != set(names):
        raise TransferError("downloaded candidate file set is incomplete or unexpected")
    for item in targets:
        verify_pair(directory, expected, item)


def complete(directory, repo, expected):
    release(expected)
    prepare = load_prepare()
    result = prepare.prepare_release(directory, "v" + expected["version"], expected["commit"], repo)
    gh(["release", "upload", expected["tag"], "--repo", expected["repository"], str(directory / "SHA256SUMS")])
    notes = directory / "RELEASE-NOTES.md"
    notes.write_text(notes.read_text(encoding="utf-8").rstrip() + "\n\n" + marker(expected) + "\n", encoding="utf-8")
    gh(["release", "edit", expected["tag"], "--repo", expected["repository"], "--notes-file", str(notes)])
    release(expected)  # Completion keeps the candidate unpublished draft state.
    return result


def summarize(paths):
    rows = []
    for value in paths[:32]:
        path = Path(value)
        row = {"file": path.name, "exists": path.exists()}
        if path.is_file():
            row["bytes"] = path.stat().st_size
            if path.suffix == ".json" and row["bytes"] <= 1024 * 1024:
                try:
                    data = json.loads(path.read_text(encoding="utf-8"))
                    if isinstance(data, dict):
                        for key in ("schema", "status", "target", "archive_sha256"):
                            if isinstance(data.get(key), str):
                                row[key] = data[key][:512]
                except (ValueError, UnicodeError):
                    row["json_readable"] = False
        rows.append(row)
    value = "```json\n" + json.dumps(rows, ensure_ascii=True) + "\n```\n"
    if os.environ.get("GITHUB_STEP_SUMMARY"):
        with open(os.environ["GITHUB_STEP_SUMMARY"], "a", encoding="utf-8") as output:
            output.write(value)
    print(value, end="")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("mode", choices=("create", "upload", "download", "complete", "summary"))
    for name in ("repository", "candidate-tag", "version", "commit", "run-id"):
        parser.add_argument("--" + name)
    parser.add_argument("--target", choices=TARGETS)
    parser.add_argument("--directory", type=Path)
    parser.add_argument("--repo", type=Path)
    parser.add_argument("--path", action="append", default=[])
    args = parser.parse_args()
    if args.mode == "summary":
        summarize(args.path)
        return
    required = ("repository", "candidate_tag", "version", "commit", "run_id")
    if args.mode in ("upload", "download", "complete"):
        required += ("directory",)
    if args.mode == "upload":
        required += ("target",)
    if args.mode == "complete":
        required += ("repo",)
    if any(getattr(args, name) is None for name in required):
        parser.error("missing required candidate context or directory/target for this operation")
    try:
        expected = identity(args.repository, args.candidate_tag, args.version, args.commit, args.run_id)
        if args.mode == "create":
            create(expected)
        elif args.mode == "upload":
            upload(args.directory.resolve(), expected, args.target)
        elif args.mode == "download":
            download(args.directory, expected, args.target)
        else:
            complete(args.directory.resolve(), args.repo.resolve(), expected)
    except (TransferError, OSError, ValueError, TypeError, KeyError) as error:
        print(f"candidate release transfer failed: {error}", file=sys.stderr)
        raise SystemExit(1)


if __name__ == "__main__":
    main()
