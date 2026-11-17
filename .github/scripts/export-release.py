#!/usr/bin/env python3
"""Export a checked Ku bundle without publishing or obtaining credentials.

Only Python's standard library is used. Cargo metadata is supplied by the
caller so this program never downloads a dependency. The command-line release
path runs only the freshly verified host Ku binary and repository examples.
"""

from __future__ import annotations

import argparse
import gzip
import hashlib
import importlib.util
import json
import os
from pathlib import Path, PurePosixPath
import platform
import re
import shutil
import stat
import subprocess
import sys
import tarfile
import tempfile
import time

_RUNTIME_SPEC = importlib.util.spec_from_file_location("ku_runtime_bundle", Path(__file__).with_name("runtime-bundle.py"))
assert _RUNTIME_SPEC and _RUNTIME_SPEC.loader
RUNTIME_BUNDLE = importlib.util.module_from_spec(_RUNTIME_SPEC)
_RUNTIME_SPEC.loader.exec_module(RUNTIME_BUNDLE)

_COMPRESSION_SPEC = importlib.util.spec_from_file_location("ku_native_compression_build", Path(__file__).with_name("build-native-compression.py"))
assert _COMPRESSION_SPEC and _COMPRESSION_SPEC.loader
NATIVE_COMPRESSION = importlib.util.module_from_spec(_COMPRESSION_SPEC)
_COMPRESSION_SPEC.loader.exec_module(NATIVE_COMPRESSION)

_CALENDAR_SPEC = importlib.util.spec_from_file_location("ku_native_calendar_build", Path(__file__).with_name("build-native-calendar.py"))
assert _CALENDAR_SPEC and _CALENDAR_SPEC.loader
NATIVE_CALENDAR = importlib.util.module_from_spec(_CALENDAR_SPEC)
_CALENDAR_SPEC.loader.exec_module(NATIVE_CALENDAR)

_HTTP_CLIENT_SPEC = importlib.util.spec_from_file_location("ku_native_http_client_build", Path(__file__).with_name("build-native-http-client.py"))
assert _HTTP_CLIENT_SPEC and _HTTP_CLIENT_SPEC.loader
NATIVE_HTTP_CLIENT = importlib.util.module_from_spec(_HTTP_CLIENT_SPEC)
_HTTP_CLIENT_SPEC.loader.exec_module(NATIVE_HTTP_CLIENT)

MAX_FILES = 1000
MAX_BYTES = 1024 * 1024 * 1024
MAX_METADATA = 16 * 1024 * 1024
MAX_LICENSE = 32 * 1024 * 1024
MAX_LICENSE_TOTAL = 64 * 1024 * 1024
MAX_TAR_TRAILER = 64 * 1024
TARGETS = {
    "x86_64-pc-windows-msvc",
    "x86_64-pc-windows-gnu",
    "x86_64-unknown-linux-gnu",
    "aarch64-apple-darwin",
}
VERSION = re.compile(r"0\.(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)\Z")
COMMIT = re.compile(r"[0-9a-f]{40}\Z")
PACKAGE = re.compile(r"[A-Za-z0-9][A-Za-z0-9_.+-]{0,119}\Z")
LICENSE_NAME = re.compile(r"(?:LICEN[CS]E|COPYING|NOTICE|COPYRIGHT)(?:[._-].*)?\Z", re.I)
RLIB_NAME = re.compile(r"deps/lib[A-Za-z_][A-Za-z0-9_]*-[0-9a-f]{16}\.rlib\Z")
KU_LICENSE = "Apache-2.0"


class ExportError(ValueError):
    pass


class RuntimeSmokeError(ExportError):
    def __init__(self, message: str, evidence: dict):
        super().__init__(message)
        self.evidence = evidence


def canonical_example(source: bytes) -> bytes:
    """Hash and execute the same UTF-8/LF text on every checkout platform."""
    return source.decode("utf-8").replace("\r\n", "\n").encode("utf-8")


def install_command(executable: Path, arguments: list[str], working: Path) -> tuple[dict, str]:
    """Run an installed CLI command with bounded time/output and reap it."""
    with tempfile.TemporaryFile() as stdout, tempfile.TemporaryFile() as stderr:
        process = subprocess.Popen([str(executable), *arguments], cwd=working,
                                   stdin=subprocess.DEVNULL, stdout=stdout, stderr=stderr)
        deadline = time.monotonic() + 30
        try:
            while process.poll() is None:
                if time.monotonic() >= deadline:
                    raise ExportError("installed Ku command exceeded 30 seconds")
                if any(os.fstat(stream.fileno()).st_size > 65536 for stream in (stdout, stderr)):
                    raise ExportError("installed Ku command exceeded its output limit")
                time.sleep(0.02)
            output = []
            for stream in (stdout, stderr):
                stream.seek(0)
                value = stream.read(65537)
                if len(value) > 65536:
                    raise ExportError("installed Ku command exceeded its output limit")
                output.append(value)
            if process.returncode != 0 or output[1]:
                raise ExportError(f"installed Ku command failed (exit {process.returncode}): "
                                  + output[1].decode("utf-8", errors="replace")[:2048])
            receipt = {"exit_code": process.returncode, "stdout_bytes": len(output[0]),
                       "stdout_sha256": hashlib.sha256(output[0]).hexdigest(),
                       "stderr_bytes": len(output[1])}
            return receipt, output[0].decode("utf-8").replace("\r\n", "\n")
        finally:
            if process.poll() is None:
                process.kill()
            process.wait(timeout=10)


def verify_installation(installed: Path, repo: Path, version: str, target: str) -> dict:
    """Use the files extracted from the final archive, outside the checkout."""
    hosts = {("Windows", "amd64"): {"x86_64-pc-windows-msvc", "x86_64-pc-windows-gnu"},
             ("Linux", "x86_64"): {"x86_64-unknown-linux-gnu"},
             ("Darwin", "arm64"): {"aarch64-apple-darwin"}}
    if target not in hosts.get((platform.system(), platform.machine().lower()), set()):
        raise ExportError("installation check requires the release target's native host")
    executable = installed / ("ku.exe" if "-windows-" in target else "ku")
    receipt = {"format": "ku-install-check-v1", "version": version, "target": target,
               "executable_sha256": file_hash(executable), "checks": [], "examples": {}}
    with tempfile.TemporaryDirectory(prefix="ku-install-check-") as temporary:
        working = Path(temporary).resolve()
        for name in ("hello.ku", "json.ku"):
            source = canonical_example(read_bounded(repo / "examples" / name, 65536))
            (working / name).write_bytes(source)
            receipt["examples"][name] = hashlib.sha256(source).hexdigest()
        for name, arguments in (("version", ["--version"]),
                                ("check-hello", ["check", "hello.ku"]),
                                ("run-hello", ["run", "hello.ku"]),
                                ("run-json", ["run", "json.ku"])):
            record, output = install_command(executable, arguments, working)
            if name == "version" and output != f"ku {version}\n":
                raise ExportError("installed Ku version does not match the release")
            if name == "check-hello" and output != "check ok: hello.ku\n":
                raise ExportError("installed Ku did not check the representative program")
            if name == "run-hello" and output != "Hello Ku\n":
                raise ExportError("installed Ku representative program returned unexpected output")
            if name == "run-json":
                lines = output.splitlines()
                if (len(lines) != 4 or lines[1:3] != ["roundtrip ok", "json/parse_error"]
                        or "byte 7" not in lines[3]):
                    raise ExportError("installed Ku JSON example did not complete its checks")
                value = json.loads(lines[0])
                if (value.get("name") != "中文😀" or value.get("count") != 7
                        or value.get("active") is not True or value.get("absent", True) is not None
                        or value.get("embedded_nul") != "left\0right"
                        or value.get("nested") != {"numbers": [[1, 2], [3]]}):
                    raise ExportError("installed Ku JSON result differs from the example")
            receipt["checks"].append({"name": name, **record})
    return receipt


def verify_default_native_installation(installed: Path, repo: Path, target: str) -> dict:
    """Use the final archive's omitted-backend build, then remove its sources."""
    executable = installed / ("ku.exe" if "-windows-" in target else "ku")
    receipt = {"format": "ku-installed-default-native-v1", "status": "running", "target": target,
               "executable_sha256": file_hash(executable), "backend": "c", "profile": "release"}
    with tempfile.TemporaryDirectory(prefix="ku-installed-default-c-") as temporary:
        working = Path(temporary).resolve()
        project = working / "project 中文 with spaces"
        project.mkdir()
        source = canonical_example(read_bounded(repo / "examples/hello.ku", 65536))
        (project / "hello.ku").write_bytes(source)
        receipt["source_sha256"] = hashlib.sha256(source).hexdigest()
        program = working / ("hello-default-c.exe" if "-windows-" in target else "hello-default-c")
        build, text = install_command(executable, ["build", "--release", "--verbose", "hello.ku", "-o", str(program)], project)
        if ("build profile: release" not in text or "c compiler command:" not in text
                or "rustc command:" in text or not program.is_file()):
            raise ExportError("installed omitted-backend build did not execute the Release C compiler")
        receipt["build"] = build
        receipt["binary_sha256"] = file_hash(program)
        shutil.rmtree(project)
        receipt["source_tree_removed"] = not project.exists()
        run, text = install_command(program, [], working)
        if text != "Hello Ku\n":
            raise ExportError("installed default C program returned an incorrect result")
        receipt["run"] = run
    receipt.update(status="passed", temporary_tree_removed=not working.exists())
    return receipt


def run_runtime_check(arguments: list[str], repo: Path, working: Path,
                      timeout_seconds: float = 180) -> subprocess.CompletedProcess:
    """Reuse the existing suspended Windows Job/POSIX session owner and bounds."""
    if type(timeout_seconds) not in (int, float) or not 0 < timeout_seconds <= 180:
        raise ExportError("runtime verification may only shorten the existing 180-second execution cap")
    runner_path = Path(__file__).with_name("verify-native-three-os.py")
    spec = importlib.util.spec_from_file_location("ku_release_runtime_runner", runner_path)
    if spec is None or spec.loader is None:
        raise ExportError("cannot load existing native process owner")
    runner = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(runner)
    runner.COMMAND_TIMEOUT_SECONDS = timeout_seconds
    runner.MAX_CAPTURE_BYTES = 1024 * 1024
    # Run the unchanged driver in this same contained Python process after
    # directing all of its TemporaryDirectories into our outer owned root.
    # run_path does not set sys.path[0] for a source file; restore normal Python
    # script sibling imports instead of retaining the -c working-directory entry.
    wrapper = ("import os,runpy,sys; root=sys.argv.pop(1); "
               "os.environ.update({name:root for name in ('TMPDIR','TMP','TEMP')}); "
               "sys.argv=sys.argv[1:]; "
               "sys.path[0]=os.path.dirname(os.path.realpath(sys.argv[0])); "
               "runpy.run_path(sys.argv[0],run_name='__main__')")
    command = [arguments[0], "-B", "-c", wrapper, str(working), *arguments[2:]]
    return runner.run_bounded(command, repo, "installed runtime smoke")


def verify_runtime_installation(installed: Path, repo: Path, target: str) -> dict:
    """Run the existing three-second fixture with the extracted CLI, not Cargo."""
    repo = repo.resolve()
    executable = installed / ("ku.exe" if "-windows-" in target else "ku")
    driver = repo / "tests/acceptance/multiloop_load.py"
    fixture = repo / "tests/fixtures/multiloop_service.ku"
    receipt = {"format": "ku-installed-runtime-smoke-v1", "status": "running", "target": target,
               "executable_sha256": file_hash(executable), "driver_sha256": file_hash(driver),
               "fixture_sha256": file_hash(fixture), "duration_seconds": 3, "arrival_rps": 1,
               "process_owner": {"path": ".github/scripts/verify-native-three-os.py",
                                 "sha256": file_hash(Path(__file__).with_name("verify-native-three-os.py")),
                                 "execution_timeout_seconds": 180, "combined_capture_bytes": 1024 * 1024,
                                 "cleanup_budget_seconds": 10, "windows_setup_seconds": 5},
               "selected_loops": 2, "scope": "installed native HTTP/Task/timer smoke, not capacity or sustained acceptance"}
    failure = None
    failure_error = None
    with tempfile.TemporaryDirectory(prefix="ku-installed-runtime-") as temporary:
        working = Path(temporary).resolve()
        report_path = working / "runtime.json"
        arguments = [sys.executable, "-B", str(driver), "--ku", str(executable), "--loops", "2",
                     "--duration", "3", "--rps", "1", "--profile", "release", "--output", str(report_path)]
        result = None
        try:
            result = run_runtime_check(arguments, repo, working)
            receipt["driver_exit_code"] = result.returncode
            receipt["process_tree_cleanup_confirmed"] = True
            if result.returncode != 0:
                raise ExportError(f"installed runtime check failed (exit {result.returncode})")
            report = json.loads(read_bounded(report_path, 1024 * 1024))
            if (report.get("status") != "passed" or report.get("cli_sha256") != receipt["executable_sha256"]
                    or report.get("fixture_sha256") != receipt["fixture_sha256"]
                    or len(report.get("runs", [])) != 1):
                raise ExportError("installed runtime check report identity/status differs")
            run = report["runs"][0]
            phases = run.get("phases", [])
            if (run.get("loops") != 2 or run.get("exit_code") != 0 or len(phases) != 1
                    or phases[0].get("name") != "short"):
                raise ExportError("installed runtime check did not complete two-loop run and normal drain")
            phase = phases[0]
            if (phase.get("arrivals") != 3 or phase.get("completed") != 3 or phase.get("client_shed") != 0
                    or phase.get("counts") != {"success": 3} or phase.get("first_errors")):
                raise ExportError("installed runtime check did not return all three correct bodies")
            metrics = run["final_metrics"]
            loops, tasks, outcomes = metrics["loops"], metrics["tasks"], metrics["outcomes"]
            if (len(loops) != 2 or len(tasks) != 2 or len(outcomes) != 2
                    or any(type(row[key]) is not int or row[key] != 0 for row in loops
                           for key in ("queued", "running", "resident", "pending"))
                    or any(type(row[key]) is not int or row[key] != 0 for row in tasks
                           for key in ("ready", "waiting", "running", "resident", "timers"))
                    or any(type(row["queued_bytes"]) is not int or row["queued_bytes"] != 0 for row in outcomes)):
                raise ExportError("installed runtime check left Task or HTTP queue records/bytes")
            if file_hash(executable) != receipt["executable_sha256"]:
                raise ExportError("installed Ku changed during its runtime check")
            receipt.update(requests={"planned": 3, "completed": 3, "correct": 3,
                                     "paths_in_order": ["/ok", "/cpu", "/timer"]},
                           observed_loops=len(loops), task_records=0, http_queue_records=0,
                           http_queue_bytes=0, drain_exit_code=run["exit_code"], status="passed")
        except (Exception, SystemExit) as error:
            failure_error = error
            failure = str(error)[-8192:]
            receipt.update(status="failed", error=failure, error_truncated=len(str(error)) > 8192)
            if result is None:
                # The reused owner reports cleanup uncertainty as an error;
                # never infer a clean tree just because its launcher exited.
                receipt["process_tree_cleanup_confirmed"] = False
            if report_path.is_file():
                with report_path.open("rb") as stream:
                    stream.seek(max(0, os.fstat(stream.fileno()).st_size - 8192))
                    receipt["report_tail"] = stream.read(8192).decode("utf-8", errors="replace")
            if result is not None:
                for name, data in (("stdout_tail", result.stdout), ("stderr_tail", result.stderr)):
                    receipt[name] = data[-8192:].decode("utf-8", errors="replace")
    receipt["temporary_tree_removed"] = not working.exists()
    if failure:
        raise RuntimeSmokeError(failure, receipt) from failure_error
    return receipt


class PlainTarInfo(tarfile.TarInfo):
    """Reject extension headers before tarfile can parse their declared payload.

    The exporter writes USTAR only. In particular, a forged PAX/GNU long-name
    header must never cause tarfile to read an attacker-sized metadata buffer.
    """

    @classmethod
    def fromtarfile(cls, archive):
        # Security-patched CPython versions route their inherited fromtarfile
        # through private _frombuf, bypassing a frombuf-only override. Own this
        # public entry point: consume exactly one header, validate it, and never
        # dispatch into tarfile's PAX/GNU/sparse extension processors.
        buf = archive.fileobj.read(tarfile.BLOCKSIZE)
        member = cls.frombuf(buf, archive.encoding, archive.errors)
        member.offset_data = archive.fileobj.tell()
        member.offset = member.offset_data - tarfile.BLOCKSIZE
        blocks = (member.size + tarfile.BLOCKSIZE - 1) // tarfile.BLOCKSIZE
        archive.offset = member.offset_data + blocks * tarfile.BLOCKSIZE
        return member

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
    if target in ("x86_64-pc-windows-msvc", "x86_64-pc-windows-gnu"):
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


def collect_licenses(stage: Stager, metadata_path: Path, repo: Path, sysroot: Path, publisher_toolchain: str | None = None) -> dict:
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

    ku = [package for package in packages if package["id"] in workspace
          and package["name"] == "ku"
          and Path(package["manifest_path"]).parent.resolve() == repo]
    if len(ku) != 1 or ku[0].get("license") != KU_LICENSE:
        raise ExportError("Ku Cargo metadata must declare Apache-2.0")
    for name in ("LICENSE", "NOTICE"):
        add_license(repo / name, name)

    for package in sorted(packages, key=lambda p: (p["name"], p["version"], p["id"])):
        name, version = package["name"], package["version"]
        if not PACKAGE.fullmatch(name) or not PACKAGE.fullmatch(version):
            raise ExportError("unsafe Cargo package identity")
        manifest = Path(package["manifest_path"])
        plain(manifest)
        root = manifest.parent
        if package["id"] in workspace:
            # Ku's own original license and notice are included at archive root.
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
        "ku_license": KU_LICENSE,
        "ku_materials": ["LICENSE", "NOTICE"],
        "cargo": records,
        "rust": {"required_toolchain": publisher_toolchain or RUNTIME_BUNDLE.toolchain_config(Path(__file__).resolve().parents[2]), "materials": rust_paths},
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


def add_native_compression(stage: Stager, root: Path, target: str) -> list[dict]:
    """Export only verified optional pack payload, never its build/cache tree."""
    if not root.is_absolute():
        raise ExportError("--native-compression-root must be an absolute private pack root")
    plain(root, directory=True)
    records = []
    for codec in ("zstd", "zlib"):
        relative = f"native-compression/v1/{target}/{codec}"
        leaf = root / relative
        plain(leaf, directory=True)
        # Check every allowed input's directory components before even reading
        # a manifest; no symlink/junction can redirect verification elsewhere.
        archive = NATIVE_COMPRESSION.archive_name(codec, target)
        names = ["manifest.json", "LICENSE", "lib/" + archive,
                 *["include/" + name for name in NATIVE_COMPRESSION.HEADER_NAMES[codec]]]
        for name in names:
            plain(leaf / name)
        try:
            manifest = NATIVE_COMPRESSION.validate_pack(leaf, codec, target)
        except (OSError, ValueError, KeyError, TypeError, AttributeError) as error:
            raise ExportError("native compression pack validation failed: " + str(error)) from error
        for name in names:
            stage.add(relative + "/" + name, source=leaf / name)
        # Validate the copied snapshot too. A source changing between initial
        # verification and copying must not create a mixed final SDK.
        copied = NATIVE_COMPRESSION.validate_pack(stage.root / relative, codec, target)
        if copied != manifest:
            raise ExportError("native compression pack changed during export")
        records.append({"name": codec, "version": copied["version"],
                        "license": copied["license"]["spdx"],
                        "source": copied["source"],
                        "materials": [relative + "/LICENSE"],
                        "manifest": relative + "/manifest.json"})
    return records


def export_release(bundle: Path, output: Path, repo: Path, version: str,
                   target: str, commit: str, metadata: Path, rust_sysroot: Path,
                   verify_install: bool = False,
                   native_compression_root: Path | None = None) -> dict:
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
               "rust_toolchain": "pending-runtime-validation", "status": "experimental",
               "ku_license": KU_LICENSE}
    with tempfile.TemporaryDirectory(prefix="ku-export-") as temporary:
        temporary = Path(temporary).resolve()
        staging = temporary / "staging"
        staging.mkdir()
        stage = Stager(staging)
        for path in walk_plain(bundle):
            stage.add(path.relative_to(bundle).as_posix(), source=path)
        executable = "ku.exe" if "-windows-" in target else "ku"
        archive = "ku_native_tls.lib" if target.endswith("windows-msvc") else "libku_native_tls.a"
        required = {executable, "libku.rlib", "ku-runtime.json", "ku-runtime-deps.json", f"ku-language-{version}.vsix",
                    f"native-tls/v1/{target}/manifest.kutls",
                    f"native-tls/v1/{target}/include/ku_native_tls.h",
                    f"native-tls/v1/{target}/lib/{archive}"}
        if not required.issubset(stage.files) or not any(RLIB_NAME.fullmatch(name) for name in stage.files):
            raise ExportError("bundle is missing required compiler, runner dependencies, VSIX or TLS pack")
        allowed = required | ({"ku.pdb"} if target.endswith("windows-msvc") else set())
        calendar_relative = f"native-calendar/v1/{target}"
        calendar_manifest = None
        if os.path.lexists(bundle / 'native-calendar'):
            leaf = bundle / calendar_relative
            calendar_names = ['manifest.json', 'LICENSE.ku', 'LICENSE.chrono',
                              'include/ku_native_calendar.h', 'lib/' + NATIVE_CALENDAR.archive_name(target)]
            try:
                plain(leaf, directory=True)
                for name in calendar_names:
                    plain(leaf / name)
                expected_source = NATIVE_CALENDAR.source_hash()
                calendar_manifest = NATIVE_CALENDAR.validate_pack(leaf, target, expected_source)
                copied = NATIVE_CALENDAR.validate_pack(stage.root / calendar_relative, target, expected_source)
                if copied != calendar_manifest:
                    raise ValueError('calendar pack changed during export')
            except (ValueError, OSError, KeyError, TypeError, AttributeError) as error:
                raise ExportError('native calendar pack validation failed: ' + str(error)) from error
            allowed |= {calendar_relative + '/' + name for name in calendar_names}
        http_client_relative = f"native-http/v1/{target}"
        http_client_manifest = None
        if os.path.lexists(bundle / 'native-http'):
            leaf = bundle / http_client_relative
            http_client_names = ['manifest.json', 'LICENSE.ku', 'LICENSE.dependencies',
                              'include/ku_native_http_client.h', 'lib/' + NATIVE_HTTP_CLIENT.archive_name(target)]
            try:
                plain(leaf, directory=True)
                for name in http_client_names:
                    plain(leaf / name)
                expected_source = NATIVE_HTTP_CLIENT.source_hash()
                http_client_manifest = NATIVE_HTTP_CLIENT.validate_pack(leaf, target, expected_source)
                copied = NATIVE_HTTP_CLIENT.validate_pack(stage.root / http_client_relative, target, expected_source)
                if copied != http_client_manifest:
                    raise ValueError('http_client pack changed during export')
            except (ValueError, OSError, KeyError, TypeError, AttributeError) as error:
                raise ExportError('native http_client pack validation failed: ' + str(error)) from error
            allowed |= {http_client_relative + '/' + name for name in http_client_names}
        if any(name not in allowed and not dependency_name(name, target) for name in stage.files):
            raise ExportError("bundle contains an unexpected file")
        try:
            runtime = RUNTIME_BUNDLE.verify(bundle, version, commit, target)
            closure = RUNTIME_BUNDLE.read_json(bundle / runtime['dependency_manifest'])
            listed = {row['path'] for row in closure['files']}
            if {name for name in stage.files if name.startswith('deps/')} != {name for name in listed if name.startswith('deps/')}:
                raise ValueError('exported dependencies differ from same-build runtime closure')
        except (ValueError, OSError, KeyError, TypeError) as error:
            raise ExportError('runtime bundle validation failed: ' + str(error)) from error
        release['rust_toolchain'] = runtime['rustc']['release']
        if executable == "ku" and stage.files[executable]["mode"] != 0o755:
            raise ExportError("Unix ku executable does not have its executable mode")
        attribution = collect_licenses(stage, metadata, repo, rust_sysroot, runtime["rustc"]["toolchain"])
        if calendar_manifest is not None:
            attribution['native_calendar'] = {
                'version': calendar_manifest['crate_version'],
                'chrono_version': calendar_manifest['chrono_version'],
                'source_sha256': calendar_manifest['source_sha256'],
                'manifest': calendar_relative + '/manifest.json',
                'materials': [calendar_relative + '/LICENSE.ku', calendar_relative + '/LICENSE.chrono'],
            }
        if http_client_manifest is not None:
            attribution['native_http_client'] = {
                'version': http_client_manifest['crate_version'],
                'ureq_version': http_client_manifest['ureq_version'],
                'source_sha256': http_client_manifest['source_sha256'],
                'manifest': http_client_relative + '/manifest.json',
                'materials': [http_client_relative + '/LICENSE.ku', http_client_relative + '/LICENSE.dependencies'],
            }
        if native_compression_root is not None:
            attribution["native_compression"] = add_native_compression(stage, native_compression_root, target)
        stage.add("THIRD_PARTY.json", data=json_bytes(attribution))
        stage.add("RELEASE.json", data=json_bytes(release))
        instructions = f"""# Ku {version} ({target})

This is an experimental, target-specific build from commit {commit}.
Extract the whole archive into an empty directory and keep its layout intact.
Run ./{executable} --version (PowerShell: .\\{executable} --version).
Optionally add that directory to PATH. Install ku-language-{version}.vsix in VS Code.

Running/checking Ku source does not require Rust. This source build defaults
to native C: ku build --release main.ku needs the target C compiler/linker,
does not use libku.rlib or rustc, and does not fall back to Runner on failure.
Use ku doctor --backend c to inspect the C toolchain. This does not change the
default of previously published packages; follow their own installation notes.

The interpreter wrapper remains available with ku build --backend runner.
It needs the exact publisher compiler recorded in ku-runtime.json
(Rust {runtime['rustc']['release']} for this bundle) plus the target native linker.
Ku selects a matching installed compiler without changing your Rust default,
project override or toolchain file. Diagnose the explicit Runner backend with:
  ku doctor --backend runner
If that exact compiler is missing, explicitly install it with:
  ku doctor --install-runner-toolchain
KU_RUNNER_RUSTC may explicitly select an absolute compiler path; a mismatched
explicit compiler is rejected. The complete ku-runtime.json,
ku-runtime-deps.json, libku.rlib and deps/ must stay beside ku. deps/ contains matching Rust
RLIBs and Cargo-emitted procedural-macro libraries for that exact toolchain and
platform; do not replace them with another build. The native C backend needs a
working target C compiler/linker. Keep native-tls/ beside ku for automatic
matching TLS pack discovery; no extra setting is needed in this bundle layout.
External database client libraries are not bundled; database programs need the
matching dynamic libraries. This is not a universal executable for all OSes.

The explicit Runner backend embeds the entry source only. Imported modules are loaded
from their original source locations at runtime, so those source locations must
remain available. For self-contained deployment of the complete local import
graph, use ku build --backend c --release with the target C compiler/linker.
The historical --native form without -o only emits C; it is not this command.

中文：运行/检查源码不需要 Rust；本源码构建的默认 build 使用原生 C，需目标 C
编译器，失败不回退。旧发布包的默认值以其安装说明为准。保留整个安装包。
显式 ku build --backend runner 仍需精确 Rust 工具链；可执行
ku doctor --backend runner，缺少时显式 ku doctor --install-runner-toolchain。
该操作不会修改全局 Rust 默认版本。
完整说明 / Details: https://github.com/fengyanweb/ku/blob/{commit}/docs/cli-runtime-bundle.md
English: https://github.com/fengyanweb/ku/blob/{commit}/docs/cli-runtime-bundle.en.md

SHA256SUMS covers every payload file except SHA256SUMS itself. The separate
asset manifest also covers SHA256SUMS and the complete compressed archive.
THIRD_PARTY.json records original Cargo/Rust license and copyright materials.
The VSIX uses Node/VS Code APIs; its npm build tools are not distributed.

Ku's own code is licensed under Apache-2.0. The original LICENSE and NOTICE are
included at the archive root. Third-party components retain their respective
licenses and copyright notices in THIRD_PARTY/ and THIRD_PARTY.json.
"""
        if calendar_manifest is not None:
            instructions += """
Keep native-calendar/v1/ beside ku when compiling synchronous date, datetime,
format, parse, parts or weekday calls. The matching calendar pack is discovered
automatically and linked statically; the generated program does not need that
pack, rustc, a Ku SDK or Ku source files at runtime. System runtime libraries
and the operating system's local-time configuration still apply.
中文：编译同步日期与格式化接口时，请保留 ku 旁的 native-calendar/v1/；
链接后的程序不再需要该静态包、rustc、Ku SDK 或源码。
"""
        if http_client_manifest is not None:
            instructions += "\nKeep native-http/v1/ beside ku when compiling http.get/post/request in synchronous code or supported native Tasks. The matching static HTTP client pack is linked only when used. The resulting program needs no Ku SDK, source or Rust compiler.\n中文：原生 HTTP 客户端构建需匹配的 native-http/v1/ 静态包；生成程序运行不依赖 SDK 或 Rust。\n"
        if native_compression_root is not None:
            instructions += """
Optional native compression packs (Zstandard 1.5.7 and zlib 1.4) are included
under native-compression/v1/ for automatic installed-CLI discovery. Keep their
layout and original LICENSE files intact. Programs using std.zstd, std.gzip or
std.deflate require the native C backend and a matching C compiler; the codec
archives are linked statically and need no codec shared-library sidecar when
deployed. System C runtime requirements still apply. Unused codecs are not
linked. Running/checking or Runner builds do not implement these bytes APIs.
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
        if verify_install:
            manifest["installation_check"] = verify_installation(verified, repo, version, target)
            default_native_check = verify_default_native_installation(verified, repo, target)
            runtime_check = verify_runtime_installation(verified, repo, target)
        # Exclusive creation also closes the race with another publisher.
        with asset_path.open("xb") as destination, compressed.open("rb") as source:
            shutil.copyfileobj(source, destination, length=1024 * 1024)
        with manifest_path.open("xb") as destination:
            destination.write(json_bytes(manifest))
    result = {"asset_path": str(asset_path.resolve()), "manifest_path": str(manifest_path.resolve()),
              "sha256": manifest["sha256"], "size": manifest["size"]}
    if verify_install:
        # Side-channel evidence stays in the single CLI JSON result. The exact
        # six-file release asset set and strict installation manifest stay intact.
        result["runtime_installation"] = runtime_check
        result["default_native_installation"] = default_native_check
    return result


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    for argument in ("bundle", "output", "repo", "metadata", "rust-sysroot"):
        parser.add_argument(f"--{argument}", required=True, type=Path)
    for argument in ("version", "target", "commit"):
        parser.add_argument(f"--{argument}", required=True)
    parser.add_argument("--native-compression-root", type=Path,
                        help="explicit optional private build root containing both verified codec packs; no cache/source material is exported")
    arguments = parser.parse_args()
    try:
        result = export_release(**vars(arguments), verify_install=True)
    except (ExportError, OSError, ValueError, KeyError, TypeError, AttributeError,
            RecursionError, tarfile.TarError, EOFError) as error:
        if isinstance(error, RuntimeSmokeError):
            print(json.dumps({"runtime_installation": error.evidence}, sort_keys=True), file=sys.stderr)
        print(f"release export failed: {error}", file=sys.stderr)
        return 1
    print(json.dumps(result, sort_keys=True))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
