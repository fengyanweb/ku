#!/usr/bin/env python3
"""Create/verify precise Runner manifests from a same-build release tree.

No Cargo build, toolchain download, network access or dependency-directory
fallback is performed. The caller supplies the output of the actual rustc -vV.
"""
from __future__ import annotations

import argparse
import hashlib
import json
import os
from pathlib import Path, PurePosixPath
import re
import stat

MAX_FILES = 1000
MAX_MANIFEST = 1024 * 1024
MAX_FILE = 512 * 1024 * 1024
MAX_TOTAL = 1024 * 1024 * 1024
SHA = re.compile(r"[0-9a-f]{64}\Z")


def encoded(value):
    return (json.dumps(value, ensure_ascii=False, sort_keys=True, separators=(",", ":")) + "\n").encode()


def digest(path):
    result = hashlib.sha256()
    with path.open("rb") as source:
        for block in iter(lambda: source.read(1024 * 1024), b""):
            result.update(block)
    return result.hexdigest()


def plain(path):
    info = path.lstat()
    if (not stat.S_ISREG(info.st_mode) or stat.S_ISLNK(info.st_mode)
            or getattr(info, "st_file_attributes", 0) & 0x400):
        raise ValueError("runtime bundle file must be plain: " + str(path))
    if not 0 < info.st_size <= MAX_FILE:
        raise ValueError("runtime bundle file size is invalid: " + str(path))
    return info


def relative_file(root, name):
    if (not isinstance(name, str) or not name or "\\" in name or ":" in name
            or "\0" in name or PurePosixPath(name).is_absolute()
            or any(part in ("", ".", "..") for part in name.split("/"))):
        raise ValueError("invalid runtime relative path")
    path = root.joinpath(*name.split("/"))
    current = root
    for part in name.split("/"):
        current /= part
        info = current.lstat()
        if stat.S_ISLNK(info.st_mode) or getattr(info, "st_file_attributes", 0) & 0x400:
            raise ValueError("runtime path contains a link/reparse point: " + name)
    if not path.resolve().is_relative_to(root.resolve()):
        raise ValueError("runtime path escapes bundle")
    plain(path)
    return path


def entry(root, name):
    path = relative_file(root, name)
    return {"path": name, "bytes": path.stat().st_size, "sha256": digest(path)}


def read_json(path):
    if plain(path).st_size > MAX_MANIFEST:
        raise ValueError("runtime manifest exceeds 1MiB")
    def unique(pairs):
        result = {}
        for key, value in pairs:
            if key in result:
                raise ValueError("duplicate runtime manifest key: " + key)
            result[key] = value
        return result
    return json.loads(path.read_text(encoding="utf-8"), object_pairs_hook=unique)


def toolchain_config(repo):
    config = read_json(repo / "scripts/release-toolchain.json")
    if config.get("schema") != "ku-release-toolchain-v1" or not re.fullmatch(r"\d+\.\d+\.\d+", config.get("toolchain", "")):
        raise ValueError("invalid release toolchain configuration")
    return config["toolchain"]


def rust_identity(text, toolchain, target):
    fields = {}
    for line in text.splitlines():
        if ": " in line:
            key, value = line.split(": ", 1)
            if key in fields:
                raise ValueError("duplicate rustc identity field")
            fields[key] = value
    result = {name: fields.get(name.replace("_", "-"), "")
              for name in ("release", "commit_hash", "commit_date", "host")}
    if (result["release"] != toolchain or result["host"] != target
            or not re.fullmatch(r"[0-9a-f]{40}", result["commit_hash"])
            or not re.fullmatch(r"\d{4}-\d{2}-\d{2}", result["commit_date"])):
        raise ValueError("actual compiler does not match publisher toolchain/host")
    result["toolchain"] = toolchain + "-" + target
    return result


def create(root, version, commit, target, rustc_text, toolchain, features=()):
    if not re.fullmatch(r"[0-9a-f]{40}", commit):
        raise ValueError("full source commit required")
    if not re.fullmatch(r"[0-9]+\.[0-9]+\.[0-9]+(?:-[0-9A-Za-z.-]+)?", version):
        raise ValueError("canonical Ku version required")
    compiler = rust_identity(rustc_text, toolchain, target)
    cli = entry(root, "ku.exe" if target.endswith("windows-msvc") else "ku")
    identity = dict(ku_version=version, source_commit=commit, host=target, runtime_target=target,
                    rustc=compiler, profile="release", features=sorted(features), cli=cli)
    bundle_id = hashlib.sha256(encoded(identity)).hexdigest()
    files = [entry(root, "libku.rlib")]
    deps = root / "deps"
    if not deps.is_dir() or deps.is_symlink():
        raise ValueError("same-build dependency directory missing")
    for index, path in enumerate(sorted(deps.iterdir())):
        if index >= MAX_FILES - 1:
            raise ValueError("too many runtime dependencies")
        files.append(entry(root, "deps/" + path.name))
    if len(files) < 2 or sum(row["bytes"] for row in files) > MAX_TOTAL:
        raise ValueError("runtime dependency closure empty or oversized")
    dependencies = dict(schema="ku-runtime-dependencies-v1", bundle_id=bundle_id,
                        rustc=compiler, runtime_target=target, files=files)
    dep_bytes = encoded(dependencies)
    manifest = dict(identity, schema="ku-runtime-bundle-v1", bundle_id=bundle_id,
                    runtime_rlib="libku.rlib", dependency_manifest="ku-runtime-deps.json",
                    dependency_manifest_sha256=hashlib.sha256(dep_bytes).hexdigest())
    for name, value in (("ku-runtime-deps.json", dep_bytes), ("ku-runtime.json", encoded(manifest))):
        if len(value) > MAX_MANIFEST:
            raise ValueError("runtime manifest exceeds 1MiB")
        with (root / name).open("xb") as destination:
            destination.write(value)
        if os.name != "nt":
            (root / name).chmod(0o644)
    return verify(root, version, commit, target)


def verify(root, version=None, commit=None, target=None):
    manifest = read_json(root / "ku-runtime.json")
    if manifest.get("schema") != "ku-runtime-bundle-v1" or not SHA.fullmatch(manifest.get("bundle_id", "")):
        raise ValueError("invalid runtime bundle identity")
    for key, wanted in (("ku_version", version), ("source_commit", commit), ("runtime_target", target)):
        if wanted is not None and manifest.get(key) != wanted:
            raise ValueError("runtime manifest identity mismatch: " + key)
    if manifest.get("host") != manifest.get("runtime_target"):
        raise ValueError("runtime host/target mismatch")
    compiler = manifest["rustc"]
    if (compiler.get('toolchain') != compiler.get('release', '') + '-' + manifest['host']
            or not re.fullmatch(r'[0-9a-f]{40}', manifest.get('source_commit', ''))
            or manifest.get('profile') != 'release'
            or manifest.get('features') != []
            or manifest.get('runtime_rlib') != 'libku.rlib'
            or manifest.get('cli', {}).get('path') != ('ku.exe' if manifest['host'].endswith('windows-msvc') else 'ku')):
        raise ValueError('unsupported/mixed publisher identity or layout')
    rust_identity("\n".join(k.replace("_", "-") + ": " + compiler[k] for k in ("release", "commit_hash", "commit_date", "host")), compiler["release"], manifest["host"])
    identity = {key: manifest[key] for key in ("ku_version", "source_commit", "host", "runtime_target", "rustc", "profile", "features", "cli")}
    if hashlib.sha256(encoded(identity)).hexdigest() != manifest["bundle_id"]:
        raise ValueError("runtime bundle_id does not match identity")
    dep_path = relative_file(root, manifest["dependency_manifest"])
    if digest(dep_path) != manifest["dependency_manifest_sha256"]:
        raise ValueError("runtime dependency manifest SHA mismatch")
    deps = read_json(dep_path)
    if (deps.get("schema") != "ku-runtime-dependencies-v1"
            or any(deps.get(k) != manifest[k] for k in ("bundle_id", "rustc", "runtime_target"))):
        raise ValueError("runtime dependency identity mismatch")
    files = deps.get("files")
    if not isinstance(files, list) or not 2 <= len(files) <= MAX_FILES:
        raise ValueError("invalid runtime dependency count")
    names = set()
    total = 0
    for row in [manifest["cli"], *files]:
        name = row["path"]
        if name in names:
            raise ValueError("duplicate runtime file path")
        names.add(name)
        if type(row["bytes"]) is not int or not 0 < row["bytes"] <= MAX_FILE or not SHA.fullmatch(row["sha256"]):
            raise ValueError("invalid runtime file record")
        total += row["bytes"]
        path = relative_file(root, name)
        if path.stat().st_size != row["bytes"] or digest(path) != row["sha256"]:
            raise ValueError("runtime file missing/corrupt/mixed: " + name)
    if manifest["runtime_rlib"] not in {r["path"] for r in files} or total > MAX_TOTAL:
        raise ValueError("runtime rlib missing from closure or bundle oversized")
    return manifest


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("mode", choices=("create", "verify"))
    parser.add_argument("--bundle", type=Path, required=True)
    parser.add_argument("--version")
    parser.add_argument("--commit")
    parser.add_argument("--target")
    parser.add_argument("--repo", type=Path)
    parser.add_argument("--rustc-version-file", type=Path)
    args = parser.parse_args()
    if args.mode == "create":
        if not all((args.version, args.commit, args.target, args.repo, args.rustc_version_file)):
            parser.error("create requires version/commit/target/repo/rustc-version-file")
        value = create(args.bundle, args.version, args.commit, args.target,
                       args.rustc_version_file.read_text(encoding="utf-8"), toolchain_config(args.repo))
    else:
        value = verify(args.bundle, args.version, args.commit, args.target)
    print(json.dumps({"status": "verified", "bundle_id": value["bundle_id"], "target": value["runtime_target"]}))


if __name__ == "__main__":
    main()
