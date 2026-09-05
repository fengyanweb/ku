#!/usr/bin/env python3
"""Export a checked Ku bundle without publishing or obtaining credentials.

Only Python's standard library is used. Cargo metadata is supplied by the
caller so this program never downloads a dependency or executes package code.
"""

from __future__ import annotations

import argparse
import gzip
import hashlib
import json
import os
from pathlib import Path, PurePosixPath
import re
import shutil
import stat
import sys
import tarfile
import tempfile

MAX_FILES = 1000
MAX_BYTES = 1024 * 1024 * 1024
MAX_METADATA = 16 * 1024 * 1024
MAX_LICENSE = 32 * 1024 * 1024
MAX_LICENSE_TOTAL = 64 * 1024 * 1024
MAX_TAR_TRAILER = 64 * 1024
TARGETS = {
    "x86_64-pc-windows-msvc",
    "x86_64-unknown-linux-gnu",
    "aarch64-apple-darwin",
}
VERSION = re.compile(r"0\.(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)\Z")
COMMIT = re.compile(r"[0-9a-f]{40}\Z")
PACKAGE = re.compile(r"[A-Za-z0-9][A-Za-z0-9_.+-]{0,119}\Z")
LICENSE_NAME = re.compile(r"(?:LICEN[CS]E|COPYING|NOTICE|COPYRIGHT)(?:[._-].*)?\Z", re.I)
RLIB_NAME = re.compile(r"deps/lib[A-Za-z_][A-Za-z0-9_]*-[0-9a-f]{16}\.rlib\Z")


class ExportError(ValueError):
    pass


class PlainTarInfo(tarfile.TarInfo):
    """Reject extension headers before tarfile can parse their declared payload.

The exporter writes USTAR only. In particular, a forged PAX/GNU long-name
header must never cause tarfile to read an attacker-sized metadata buffer.
"""

    @classmethod
    def frombuf(cls, buf, encoding, errors):
        member = super().frombuf(buf, encoding, errors)
        if member.type not in (tarfile.REGTYPE, tarfile.AREGTYPE):
            raise ExportError("archive contains a non-plain USTAR file header")
        if buf[257:265] != b"ustar\x0000":
            raise ExportError("archive header is not canonical USTAR")
        if member.size < 0 or member.size > MAX_BYTES:
            raise ExportError("archive header size exceeds limit")
        return member


def dependency_name(name: str, target: str) -> bool:
    if RLIB_NAME.fullmatch(name):
        return True
    # These are Cargo-emitted Rust procedural-macro libraries, not arbitrary
    # third-party database DLLs or operating-system SDK libraries.
    if target == "x86_64-pc-windows-msvc":
        pattern = r"deps/[A-Za-z_][A-Za-z0-9_]*-[0-9a-f]{16}\.dll\Z"
    elif target == "x86_64-unknown-linux-gnu":
        pattern = r"deps/lib[A-Za-z_][A-Za-z0-9_]*-[0-9a-f]{16}\.so\Z"
    elif target == "aarch64-apple-darwin":
        pattern = r"deps/lib[A-Za-z_][A-Za-z0-9_]*-[0-9a-f]{16}\.dylib\Z"
    else:
        return False
    return re.fullmatch(pattern, name) is not None


def checked_relative(name: str) -> str:
    """The same archive names must be safe on all three supported systems."""
    if not isinstance(name, str) or not name or len(name) > 500:
        raise ExportError("invalid archive path length")
    parts = name.split("/")
    reserved = {"CON", "PRN", "AUX", "NUL"} | {
        f"{prefix}{number}" for prefix in ("COM", "LPT") for number in range(1, 10)
    }
    if (
        PurePosixPath(name).is_absolute()
        or len(parts) > 12
        or any(c in name for c in "\\:\x00\r\n<>\"|?*")
        or any(ord(c) < 32 for c in name)
        or any(
            not part or part in (".", "..") or len(part) > 120
            or part.endswith((".", " ")) or part.split(".")[0].upper() in reserved
            for part in parts
        )
    ):
        raise ExportError(f"unsafe archive path: {name!r}")
    return name


def plain(path: Path, directory: bool = False) -> os.stat_result:
    """Reject symlinks and Windows junctions, including parent components."""
    absolute = Path(os.path.abspath(path))
    for entry in (absolute, *absolute.parents):
        info = entry.lstat()
        if stat.S_ISLNK(info.st_mode) or getattr(info, "st_file_attributes", 0) & 0x400:
            raise ExportError(f"symlink/reparse point is not allowed: {entry}")
    info = absolute.stat()
    expected = stat.S_ISDIR if directory else stat.S_ISREG
    if not expected(info.st_mode):
        raise ExportError(f"not a plain {'directory' if directory else 'file'}: {path}")
    return info


def read_bounded(path: Path, limit: int) -> bytes:
    if plain(path).st_size > limit:
        raise ExportError(f"file exceeds size limit: {path.name}")
    with path.open("rb") as source:
        value = source.read(limit + 1)
    if len(value) > limit:
        raise ExportError(f"file grew beyond size limit: {path.name}")
    return value


def file_hash(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as source:
        while chunk := source.read(1024 * 1024):
            digest.update(chunk)
    return digest.hexdigest()


def walk_plain(root: Path):
    plain(root, directory=True)
    pending = [(root, 0)]
    count = 0
    while pending:
        directory, depth = pending.pop()
        if depth > 12:
            raise ExportError("directory nesting exceeds limit")
        with os.scandir(directory) as scan:
            entries = []
            for entry in scan:
                count += 1
                if count > MAX_FILES * 2:
                    raise ExportError("directory entry count exceeds limit")
                entries.append(Path(entry.path))
        for entry in sorted(entries):
            info = entry.lstat()
            if stat.S_ISLNK(info.st_mode) or getattr(info, "st_file_attributes", 0) & 0x400:
                raise ExportError(f"symlink/reparse point is not allowed: {entry}")
            if stat.S_ISDIR(info.st_mode):
                pending.append((entry, depth + 1))
            elif stat.S_ISREG(info.st_mode):
                yield entry
            else:
                raise ExportError(f"special file is not allowed: {entry}")


class Stager:
    def __init__(self, root: Path):
        self.root = root
        self.files: dict[str, dict] = {}
        self.folded: set[str] = set()
        self.total = 0

    def add(self, name: str, *, source: Path | None = None, data: bytes | None = None):
        checked_relative(name)
        if name.casefold() in self.folded:
            raise ExportError(f"duplicate/case-colliding archive path: {name}")
        if len(self.files) >= MAX_FILES:
            raise ExportError("archive file count exceeds limit")
        if (source is None) == (data is None):
            raise ExportError("exactly one file source is required")
        info = plain(source) if source is not None else None
        size = info.st_size if info is not None else len(data)
        if size + self.total > MAX_BYTES:
            raise ExportError("archive expanded size exceeds limit")
        destination = self.root / name
        destination.parent.mkdir(parents=True, exist_ok=True)
        digest = hashlib.sha256()
        written = 0
        with destination.open("xb") as output:
            if source is not None:
                with source.open("rb") as incoming:
                    while chunk := incoming.read(1024 * 1024):
                        written += len(chunk)
                        if written > size:
                            raise ExportError(f"file changed while staging: {name}")
                        digest.update(chunk)
                        output.write(chunk)
            else:
                output.write(data)
                digest.update(data)
                written = len(data)
        if written != size:
            raise ExportError(f"file changed while staging: {name}")
        mode = 0o755 if info is not None and info.st_mode & 0o111 else 0o644
        self.files[name] = {"sha256": digest.hexdigest(), "size": size, "mode": mode}
        self.folded.add(name.casefold())
        self.total += size


def json_bytes(value) -> bytes:
    return (json.dumps(value, ensure_ascii=False, indent=2, sort_keys=True) + "\n").encode("utf-8")


def production_packages(metadata: dict) -> list[dict]:
    """Use a caller-supplied --filter-platform resolve graph; exclude dev edges."""
    packages = metadata.get("packages")
    resolve = metadata.get("resolve")
    roots = metadata.get("workspace_members")
    if not isinstance(packages, list) or not packages or len(packages) > 512:
        raise ExportError("Cargo metadata packages are missing or exceed limit")
    if not isinstance(resolve, dict) or not isinstance(resolve.get("nodes"), list):
        raise ExportError("Cargo metadata requires a complete resolve graph")
    if not isinstance(roots, list) or not roots or len(roots) > 32:
        raise ExportError("Cargo metadata workspace roots are missing or exceed limit")
    by_id = {package["id"]: package for package in packages}
    nodes = {node["id"]: node for node in resolve["nodes"]}
    if len(by_id) != len(packages) or len(nodes) != len(resolve["nodes"]) or len(nodes) > 512:
        raise ExportError("Cargo metadata contains duplicate/too many graph identities")
    pending = list(roots)
    seen = set()
    while pending:
        identity = pending.pop()
        if identity in seen:
            continue
        if identity not in by_id or identity not in nodes:
            raise ExportError("Cargo metadata resolve graph is incomplete")
        seen.add(identity)
        deps = nodes[identity].get("deps")
        if not isinstance(deps, list) or len(deps) > 512:
            raise ExportError("Cargo metadata has invalid dependency edges")
        for dependency in deps:
            kinds = dependency.get("dep_kinds")
            if not isinstance(kinds, list) or not kinds:
                raise ExportError("Cargo metadata dependency kind is missing")
            if any(kind.get("kind") not in (None, "build", "dev") for kind in kinds):
                raise ExportError("Cargo metadata has an unknown dependency kind")
            if any(kind.get("kind") != "dev" for kind in kinds):
                pending.append(dependency["pkg"])
    return [by_id[identity] for identity in seen]


def collect_licenses(stage: Stager, metadata_path: Path, repo: Path, sysroot: Path) -> dict:
    metadata = json.loads(read_bounded(metadata_path, MAX_METADATA))
    packages = production_packages(metadata)
    plain(repo, directory=True)
    repo = repo.resolve()
    workspace = set(metadata["workspace_members"])
    if any(not Path(package["manifest_path"]).parent.resolve().is_relative_to(repo)
           for package in packages if package["id"] in workspace):
        raise ExportError("Cargo workspace roots do not belong to the supplied repository")
    records = []
    license_bytes = 0
    seen = set()

    def add_license(source: Path, name: str):
        nonlocal license_bytes
        value = read_bounded(source, MAX_LICENSE)
        if not value.strip():
            raise ExportError(f"empty license material: {source.name}")
        # Preserve the original bytes, including HTML copyright notices.
        license_bytes += len(value)
        if license_bytes > MAX_LICENSE_TOTAL:
            raise ExportError("license material total exceeds limit")
        stage.add(name, data=value)

    for package in sorted(packages, key=lambda p: (p["name"], p["version"], p["id"])):
        name, version = package["name"], package["version"]
        if not PACKAGE.fullmatch(name) or not PACKAGE.fullmatch(version):
            raise ExportError("unsafe Cargo package identity")
        manifest = Path(package["manifest_path"])
        plain(manifest)
        root = manifest.parent
        if package["id"] in workspace:
            # No source-license grant is inferred for Ku's workspace packages.
            continue
        identity = f"{name}-{version}"
        if identity.casefold() in seen:
            raise ExportError(f"ambiguous Cargo package identity: {identity}")
        seen.add(identity.casefold())
        selected = {}
        with os.scandir(root) as entries:
            for index, entry in enumerate(entries):
                if index >= 2000:
                    raise ExportError("crate root entry count exceeds limit")
                if LICENSE_NAME.fullmatch(entry.name):
                    path = Path(entry.path)
                    if entry.is_dir(follow_symlinks=False):
                        for child in walk_plain(path):
                            selected[child.relative_to(root).as_posix()] = child
                    else:
                        selected[entry.name] = path
        explicit = package.get("license_file")
        if explicit:
            license_path = Path(explicit)
            if not license_path.is_absolute():
                license_path = root / license_path
            plain(license_path)
            if not license_path.resolve().is_relative_to(root.resolve()):
                raise ExportError(f"license_file escapes crate: {identity}")
            selected[license_path.relative_to(root).as_posix()] = license_path
        if not selected or not any(
            relative.upper().startswith(("LICENSE", "LICENCE", "COPYING"))
            or (explicit and source == license_path)
            for relative, source in selected.items()
        ):
            raise ExportError(f"missing original license material for Cargo package {identity}")
        paths = []
        for relative, source in sorted(selected.items()):
            destination = f"THIRD_PARTY/cargo/{identity}/{relative}"
            add_license(source, destination)
            paths.append(destination)
        records.append({"name": name, "version": version, "license": package.get("license"),
                        "source": package.get("source"), "materials": paths})

    rust_doc = sysroot / "share" / "doc" / "rust"
    rust_paths = []
    licenses = rust_doc / "licenses"
    for source in walk_plain(licenses):
        destination = "THIRD_PARTY/rust/licenses/" + source.relative_to(licenses).as_posix()
        add_license(source, destination)
        rust_paths.append(destination)
    if not rust_paths:
        raise ExportError("Rust sysroot license directory is empty")
    copyrights = [rust_doc / "COPYRIGHT-library.html", rust_doc / "COPYRIGHT", rust_doc / "COPYRIGHT.html"]
    copyright_file = next((path for path in copyrights if path.exists()), None)
    if copyright_file is None:
        raise ExportError("Rust sysroot copyright attribution is missing")
    destination = "THIRD_PARTY/rust/" + copyright_file.name
    add_license(copyright_file, destination)
    rust_paths.append(destination)
    return {
        "format": "ku-third-party-v1",
        "scope": "Non-dev dependency closure of workspace roots from caller-supplied cargo metadata --filter-platform TARGET. Includes build dependencies as a conservative superset, not a minimal linked-dependency list.",
        "ku_license": "No open-source license grant for Ku's own code is included in this release.",
        "cargo": records,
        "rust": {"required_toolchain": "1.89.0", "materials": rust_paths},
    }


def verify_archive(archive: Path, expected: dict, destination: Path):
    """Extract one bounded plain member at a time, then verify complete contents."""
    if plain(archive).st_size > MAX_BYTES + 32 * 1024 * 1024:
        raise ExportError("compressed archive size exceeds limit")
    plain(destination, directory=True)
    if any(destination.iterdir()):
        raise ExportError("verification destination must be empty")
    if not isinstance(expected, dict) or not expected or len(expected) > MAX_FILES:
        raise ExportError("invalid expected archive file count")
    for name in expected:
        checked_relative(name)
    seen = set()
    total = 0
    # gzip's reader validates its trailer/CRC; tarfile's direct r|gz reader does
    # not promise that it will consume the gzip trailer after the tar EOF.
    with gzip.open(archive, "rb") as decompressed, tarfile.open(
        fileobj=decompressed, mode="r|", tarinfo=PlainTarInfo
    ) as incoming:
        for member in incoming:
            name = checked_relative(member.name)
            if not member.isfile() or member.linkname or member.sparse is not None:
                raise ExportError(f"archive contains a non-plain file: {name}")
            if name in seen or name not in expected:
                raise ExportError(f"archive contains duplicate or extra file: {name}")
            seen.add(name)
            spec = expected[name]
            total += member.size
            if member.size < 0 or member.size != spec["size"] or total > MAX_BYTES:
                raise ExportError(f"archive member size mismatch/limit: {name}")
            if member.mode != spec["mode"] or member.mode not in (0o644, 0o755):
                raise ExportError(f"archive member mode mismatch: {name}")
            path = destination / name
            path.parent.mkdir(parents=True, exist_ok=True)
            digest = hashlib.sha256()
            copied = 0
            extracted = incoming.extractfile(member)
            if extracted is None:
                raise ExportError(f"archive member cannot be read: {name}")
            with extracted, path.open("xb") as output:
                while chunk := extracted.read(1024 * 1024):
                    copied += len(chunk)
                    if copied > member.size:
                        raise ExportError(f"archive member expanded beyond size: {name}")
                    output.write(chunk)
                    digest.update(chunk)
            if copied != member.size or digest.hexdigest() != spec["sha256"]:
                raise ExportError(f"archive member hash mismatch: {name}")
            path.chmod(member.mode)
        trailer_size = 0
        while trailer := incoming.fileobj.read(16 * 1024):
            trailer_size += len(trailer)
            if trailer_size > MAX_TAR_TRAILER or any(trailer):
                raise ExportError("archive has nonzero or excessive trailing data")
    if seen != set(expected):
        raise ExportError("archive is missing expected files")
    if {path.relative_to(destination).as_posix() for path in walk_plain(destination)} != seen:
        raise ExportError("extracted file set differs from manifest")


def export_release(bundle: Path, output: Path, repo: Path, version: str,
                   target: str, commit: str, metadata: Path, rust_sysroot: Path) -> dict:
    if not VERSION.fullmatch(version):
        raise ExportError("version must be a canonical 0.x.y version without v")
    if target not in TARGETS:
        raise ExportError("unsupported release target")
    if not COMMIT.fullmatch(commit):
        raise ExportError("commit must be a lowercase 40-character Git SHA")
    plain(bundle, directory=True)
    if output.resolve().is_relative_to(bundle.resolve()):
        raise ExportError("output must be outside the input bundle")
    output.mkdir(parents=True, exist_ok=True)
    plain(output, directory=True)
    asset_name = f"ku-v{version}-{target}.tar.gz"
    asset_path = output / asset_name
    manifest_path = output / f"{asset_name}.manifest.json"
    if os.path.lexists(asset_path) or os.path.lexists(manifest_path):
        raise ExportError("release output already exists; refusing overwrite")
    release = {"format": "ku-release-v1", "version": version, "target": target, "commit": commit,
               "rust_toolchain": "1.89.0", "status": "experimental",
               "ku_license": "UNLICENSED; no open-source license grant is included for Ku's own code."}
    with tempfile.TemporaryDirectory(prefix="ku-export-") as temporary:
        temporary = Path(temporary).resolve()
        staging = temporary / "staging"
        staging.mkdir()
        stage = Stager(staging)
        for path in walk_plain(bundle):
            stage.add(path.relative_to(bundle).as_posix(), source=path)
        executable = "ku.exe" if target.endswith("windows-msvc") else "ku"
        archive = "ku_native_tls.lib" if target.endswith("windows-msvc") else "libku_native_tls.a"
        required = {executable, "libku.rlib", f"ku-language-{version}.vsix",
                    f"native-tls/v1/{target}/manifest.kutls",
                    f"native-tls/v1/{target}/include/ku_native_tls.h",
                    f"native-tls/v1/{target}/lib/{archive}"}
        if not required.issubset(stage.files) or not any(RLIB_NAME.fullmatch(name) for name in stage.files):
            raise ExportError("bundle is missing required compiler, runner dependencies, VSIX or TLS pack")
        allowed = required | ({"ku.pdb"} if target.endswith("windows-msvc") else set())
        if any(name not in allowed and not dependency_name(name, target) for name in stage.files):
            raise ExportError("bundle contains an unexpected file")
        if executable == "ku" and stage.files[executable]["mode"] != 0o755:
            raise ExportError("Unix ku executable does not have its executable mode")
        stage.add("THIRD_PARTY.json", data=json_bytes(collect_licenses(stage, metadata, repo, rust_sysroot)))
        stage.add("RELEASE.json", data=json_bytes(release))
        instructions = f"""# Ku {version} ({target})

This is an experimental, target-specific build from commit {commit}.
Extract the whole archive into an empty directory and keep its layout intact.
Run ./{executable} --version (PowerShell: .\\{executable} --version).
Optionally add that directory to PATH. Install ku-language-{version}.vsix in VS Code.

The default runner backend requires Rust 1.89.0 for this target and its native
linker. libku.rlib and deps/ must stay beside ku. deps/ contains matching Rust
RLIBs and Cargo-emitted procedural-macro libraries for that exact toolchain and
platform; do not replace them with another build. The native C backend needs a
working target C compiler/linker. Keep native-tls/ beside ku for automatic
matching TLS pack discovery; no extra setting is needed in this bundle layout.
External database client libraries are not bundled; database programs need the
matching dynamic libraries. This is not a universal executable for all OSes.

The default runner embeds the entry source only. Imported modules are loaded
from their original source locations at runtime, so those source locations must
remain available. For self-contained deployment of the complete local import
graph, use ku build --native with the target C compiler/linker.

SHA256SUMS covers every payload file except SHA256SUMS itself. The separate
asset manifest also covers SHA256SUMS and the complete compressed archive.
THIRD_PARTY.json records original Cargo/Rust license and copyright materials.
The VSIX uses Node/VS Code APIs; its npm build tools are not distributed.

Ku's own code remains UNLICENSED. Publishing source or binaries here does not
grant an open-source license. Third-party licenses apply only to their respective
components. No additional license grant for Ku is implied by this archive.
"""
        stage.add("README-INSTALL.md", data=instructions.encode("utf-8"))
        sums = "".join(f"{spec['sha256']}  {name}\n" for name, spec in sorted(stage.files.items()))
        stage.add("SHA256SUMS", data=sums.encode("utf-8"))
        compressed = temporary / asset_name
        with compressed.open("xb") as raw, gzip.GzipFile(filename="", fileobj=raw, mode="wb", mtime=0) as zipped:
            with tarfile.open(fileobj=zipped, mode="w|", format=tarfile.USTAR_FORMAT) as archive_out:
                for name, spec in sorted(stage.files.items()):
                    member = tarfile.TarInfo(name)
                    member.size, member.mode = spec["size"], spec["mode"]
                    with (staging / name).open("rb") as incoming:
                        archive_out.addfile(member, incoming)
        verified = temporary / "verified"
        verified.mkdir()
        verify_archive(compressed, stage.files, verified)
        manifest = {**release, "asset": asset_name, "sha256": file_hash(compressed),
                    "size": compressed.stat().st_size, "files": stage.files}
        # Exclusive creation also closes the race with another publisher.
        with asset_path.open("xb") as destination, compressed.open("rb") as source:
            shutil.copyfileobj(source, destination, length=1024 * 1024)
        with manifest_path.open("xb") as destination:
            destination.write(json_bytes(manifest))
    return {"asset_path": str(asset_path.resolve()), "manifest_path": str(manifest_path.resolve()),
            "sha256": manifest["sha256"], "size": manifest["size"]}


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    for argument in ("bundle", "output", "repo", "metadata", "rust-sysroot"):
        parser.add_argument(f"--{argument}", required=True, type=Path)
    for argument in ("version", "target", "commit"):
        parser.add_argument(f"--{argument}", required=True)
    arguments = parser.parse_args()
    try:
        result = export_release(**vars(arguments))
    except (ExportError, OSError, ValueError, KeyError, TypeError, AttributeError,
            RecursionError, tarfile.TarError, EOFError) as error:
        print(f"release export failed: {error}", file=sys.stderr)
        return 1
    print(json.dumps(result, sort_keys=True))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
