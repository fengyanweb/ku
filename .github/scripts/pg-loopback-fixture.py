#!/usr/bin/env python3
"""Assemble an isolated PostgreSQL 17.10 query-test fixture on Windows.

Reuse an existing 17.10 bin/lib installation. Missing share files come from the
fixed, SHA-256-verified official source distribution and its own Perl generators.
This is a minimal test fixture, not a PostgreSQL installation or service.

prepare --installed-root PATH --perl PATH [--source-archive PATH]
verify --fixture PATH --test-binary PATH --ku-binary PATH

No existing installation, connection file, database, or service is modified.
Fixtures are retained for reproducible reruns; this script never deletes trees.
"""

from __future__ import annotations

import argparse
from contextlib import contextmanager
import csv
import hashlib
import importlib.util
import json
import os
from pathlib import Path, PurePosixPath
import re
import secrets
import shutil
import socket
import stat
import subprocess
import sys
import tarfile
import time
import uuid
from typing import Iterator

sys.dont_write_bytecode = True

VERSION = "17.10"
SOURCE_URL = "https://ftp.postgresql.org/pub/source/v17.10/postgresql-17.10.tar.bz2"
SOURCE_SHA256 = "078a03516dcdbdb705fecaf415ea3d13a956c589e46f09fed68a06fb00598c90"
MAX_ARCHIVE_BYTES = 32 * 1024 * 1024
MAX_ARCHIVE_ENTRIES = 16000
MAX_SOURCE_BYTES = 256 * 1024 * 1024
MAX_SELECTED_BYTES = 32 * 1024 * 1024
REPO = Path(__file__).resolve().parents[2]
TARGET = REPO / "target"
PREFIX = "pg-loopback-17.10-"
SECRET = ""
MAX_RECEIPT_BYTES = 8192

_spec = importlib.util.spec_from_file_location(
    "ku_native_bounds", Path(__file__).with_name("verify-native-three-os.py")
)
assert _spec is not None and _spec.loader is not None
BOUNDS = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(BOUNDS)


def present(path: Path) -> bool:
    # A dangling leaf or an inaccessible entry is not evidence of absence.
    try:
        path.lstat()
        return True
    except FileNotFoundError:
        return False


def write_marker(path: Path, text: str, *, mode: str = "x") -> None:
    """Write one bounded owned leaf; preserve the first error and close owner."""
    if len(text.encode("utf-8")) > MAX_RECEIPT_BYTES:
        raise ValueError("PostgreSQL operation marker exceeds its byte bound")
    output = path.open(mode, encoding="utf-8", newline="\n")
    primary = None
    try:
        if output.write(text) != len(text):
            raise OSError("Incomplete PostgreSQL operation marker write")
    except BaseException as error:
        primary = error
    cleanup = BOUNDS.CleanupErrors()
    cleanup.attempt("PostgreSQL marker close", output.close)
    try:
        cleanup.raise_if_any(primary)
    except BaseException as error:
        # Failed close must not lose the actual stream to an exception wrapper.
        error.marker_stream = output
        raise


class FixtureOperation:
    def __init__(self) -> None:
        self.uncertain = False
        self.receipt: dict | None = None


@contextmanager
def operation(root: Path) -> Iterator[FixtureOperation]:
    # Cooperative exclusive create: no PID authority, waiting, stealing, or
    # automatic recovery. A losing caller has no right to touch any other leaf.
    lock = root / "operation.lock"
    write_marker(lock, "owned PostgreSQL operation in progress\n")
    owned = FixtureOperation()
    primary = None
    try:
        yield owned
    except BaseException as error:
        primary = error
    cleanup = BOUNDS.CleanupErrors()
    if not owned.uncertain:
        if primary is None and owned.receipt is not None:
            def commit_receipt() -> None:
                text = json.dumps(owned.receipt, indent=2) + "\n"
                if SECRET and SECRET in text:
                    raise ValueError("Refusing a credential-bearing verification receipt")
                write_marker(lock, text, mode="w")
                # One same-directory rename commits the receipt AND releases
                # admission. Never write success and then separately unlink the
                # lease, or remove a target after an uncertain rename outcome.
                lock.replace(root / "verification.json")

            try:
                commit_receipt()
            except BaseException as error:
                # Keep any failed-close stream owner carried by write_marker;
                # reducing this first failure to a category would lose it.
                primary = error
        else:
            cleanup.attempt("PostgreSQL operation release", lock.unlink)
    elif primary is None:
        cleanup.add("PostgreSQL operation cleanup remains unconfirmed")
    cleanup.raise_if_any(primary)


def run(
    command: list[str], cwd: Path, label: str, timeout: int = 45, *, include_stderr: bool = False
) -> bytes:
    previous = BOUNDS.COMMAND_TIMEOUT_SECONDS
    BOUNDS.COMMAND_TIMEOUT_SECONDS = timeout
    try:
        completed = BOUNDS.run_bounded(command, cwd, label)
        return completed.stdout + completed.stderr if include_stderr else completed.stdout
    except (SystemExit, RuntimeError, OSError) as error:
        message = str(error)
        if SECRET:
            message = message.replace(SECRET, "<redacted>")
        raise RuntimeError(message) from None
    finally:
        BOUNDS.COMMAND_TIMEOUT_SECONDS = previous


def digest(path: Path) -> str:
    result = hashlib.sha256()
    deadline = time.monotonic() + 45
    total = 0
    with path.open("rb") as stream:
        for chunk in iter(lambda: stream.read(65536), b""):
            total += len(chunk)
            if total > MAX_ARCHIVE_BYTES or time.monotonic() >= deadline:
                raise ValueError("PostgreSQL source archive exceeded verification bounds")
            result.update(chunk)
    return result.hexdigest()


def validate_archive(path: Path) -> Path:
    path = path.resolve(strict=True)
    if not path.is_file() or path.stat().st_size > MAX_ARCHIVE_BYTES:
        raise ValueError("PostgreSQL source archive exceeds the 32 MiB bound")
    if digest(path) != SOURCE_SHA256:
        raise ValueError("PostgreSQL 17.10 source SHA-256 mismatch; refusing extraction")
    return path


def selected_source(name: str) -> bool:
    return (
        name.startswith("src/include/")
        or name.startswith("src/backend/catalog/")
        or name.startswith("src/backend/snowball/")
        or name.startswith("src/timezone/tznames/")
        or name in {
            "src/backend/utils/misc/postgresql.conf.sample",
            "src/backend/libpq/pg_hba.conf.sample",
            "src/backend/libpq/pg_ident.conf.sample",
            "src/pl/plpgsql/src/plpgsql.control",
            "src/pl/plpgsql/src/plpgsql--1.0.sql",
        }
    )


def extract_source(archive: Path, destination: Path) -> None:
    deadline = time.monotonic() + 45
    count = expanded = selected = 0
    seen: set[str] = set()
    with tarfile.open(archive, "r|bz2") as source:
        for entry in source:
            if entry.size < 0:
                raise ValueError("Negative PostgreSQL source entry size")
            count += 1
            expanded += entry.size
            if count > MAX_ARCHIVE_ENTRIES or expanded > MAX_SOURCE_BYTES:
                raise ValueError("PostgreSQL source archive exceeds extraction bounds")
            if time.monotonic() >= deadline:
                raise TimeoutError("PostgreSQL source extraction exceeded 45 seconds")
            parts = PurePosixPath(entry.name).parts
            if not parts or parts[0] != f"postgresql-{VERSION}" or ".." in parts:
                raise ValueError("Unexpected PostgreSQL source path")
            name = "/".join(parts[1:])
            if not selected_source(name) or entry.isdir():
                continue
            if not entry.isfile() or "\\" in name or ":" in name or name.casefold() in seen:
                raise ValueError("Non-regular or duplicate selected PostgreSQL source entry")
            seen.add(name.casefold())
            selected += entry.size
            if entry.size > 4 * 1024 * 1024 or selected > MAX_SELECTED_BYTES:
                raise ValueError("Selected PostgreSQL sources exceed 32 MiB")
            target = destination.joinpath(*parts[1:])
            target.parent.mkdir(parents=True, exist_ok=True)
            incoming = source.extractfile(entry)
            assert incoming is not None
            with incoming, target.open("xb") as output:
                while chunk := incoming.read(65536):
                    if time.monotonic() >= deadline:
                        raise TimeoutError("PostgreSQL source extraction deadline elapsed")
                    output.write(chunk)
    print(f"Verified source: {SOURCE_SHA256}; extracted {selected} bytes", flush=True)


def copy_regular(source: Path, target: Path) -> None:
    metadata = source.lstat()
    if not stat.S_ISREG(metadata.st_mode) or source.is_symlink():
        raise ValueError(f"Expected a regular PostgreSQL fixture input: {source.name}")
    if getattr(metadata, "st_file_attributes", 0) & getattr(stat, "FILE_ATTRIBUTE_REPARSE_POINT", 0):
        raise ValueError("Refusing a reparse-point PostgreSQL fixture input")
    if target.exists():
        raise ValueError("Refusing to overwrite PostgreSQL fixture output")
    target.parent.mkdir(parents=True, exist_ok=True)
    shutil.copyfile(source, target)


def private_fixture() -> Path:
    TARGET.mkdir(exist_ok=True)
    root = TARGET / f"{PREFIX}{uuid.uuid4().hex}"
    root.mkdir(mode=0o700)
    identity = run(["whoami.exe", "/user", "/fo", "csv", "/nh"], root, "query fixture owner")
    records = list(csv.reader(identity.decode("utf-8", errors="replace").splitlines()))
    if len(records) != 1 or len(records[0]) != 2 or not re.fullmatch(r"S-1-[0-9-]+", records[0][1]):
        raise ValueError("Cannot identify the current Windows owner SID")
    run(
        ["icacls.exe", str(root), "/inheritance:r", "/grant:r", f"*{records[0][1]}:(OI)(CI)F"],
        root, "restrict PostgreSQL fixture ACL",
    )
    return root


def fixture_environment(portable: Path) -> None:
    for key in list(os.environ):
        if key.upper().startswith("PG") or key in {"KU_PG_LIB", "KU_PG_LIBDIR", "KU_PG_INCLUDE"}:
            os.environ.pop(key, None)
    os.environ["PATH"] = str(portable / "bin") + os.pathsep + os.environ.get("PATH", "")
    os.environ["TZ"] = "GMT"


def prepare(args: argparse.Namespace) -> Path:
    if os.name != "nt":
        raise ValueError("This minimal assembler reuses Windows PostgreSQL 17.10 binaries")
    installed = args.installed_root.resolve(strict=True)
    perl = args.perl.resolve(strict=True)
    version = run([str(installed / "bin" / "postgres.exe"), "--version"], REPO, "check PostgreSQL version")
    if version.decode("ascii").strip() != f"postgres (PostgreSQL) {VERSION}":
        raise ValueError("Installed PostgreSQL must exactly match the verified 17.10 sources")
    root = private_fixture()
    with operation(root) as owned:
        # Preparation has no reuse/cleanup mode. On any assembly failure retain
        # the new private directory and lease, including unknown initdb cleanup.
        owned.uncertain = True
        prepare_owned(args, root, installed, perl)
        owned.uncertain = False
    print(f"Prepared (server not started): {root}", flush=True)
    return root


def prepare_owned(args: argparse.Namespace, root: Path, installed: Path, perl: Path) -> None:
    global SECRET
    print(f"Preparing private fixture: {root}", flush=True)
    if args.source_archive:
        archive = validate_archive(args.source_archive)
    else:
        archive = root / f"postgresql-{VERSION}.tar.bz2"
        run([
            "curl.exe", "--proto", "=https", "--fail", "--silent", "--show-error",
            "--connect-timeout", "10", "--max-time", "90", "--max-filesize", str(MAX_ARCHIVE_BYTES),
            "--output", str(archive), SOURCE_URL,
        ], root, "download verified PostgreSQL source", 100)
        validate_archive(archive)
    source = root / "source"
    source.mkdir()
    extract_source(archive, source)
    portable = root / "portable"
    bindir, libdir, share = portable / "bin", portable / "lib", portable / "share"
    for directory in (bindir, libdir, share, share / "extension", share / "timezone"):
        directory.mkdir(parents=True, exist_ok=True)
    for name in ["postgres.exe", "initdb.exe", "pg_ctl.exe", "pg_config.exe", "pg_isready.exe", "psql.exe"]:
        copy_regular(installed / "bin" / name, bindir / name)
    dlls = sorted((installed / "bin").glob("*.dll"))
    if len(dlls) > 128 or sum(path.stat().st_size for path in dlls) > 128 * 1024 * 1024:
        raise ValueError("Installed PostgreSQL DLL set exceeds fixture bounds")
    for path in dlls:
        copy_regular(path, bindir / path.name)
    for name in ["plpgsql.dll", "dict_snowball.dll", "libpq.lib"]:
        copy_regular(installed / "lib" / name, libdir / name)
    fixture_environment(portable)
    catalog = source / "src" / "include" / "catalog"
    makefile = (catalog / "Makefile").read_text(encoding="utf-8")
    ordered = makefile.split("CATALOG_HEADERS :=", 1)[1].split("GENERATED_HEADERS", 1)[0]
    headers = re.findall(r"\bpg_[a-z0-9_]+\.h\b", ordered)
    if not 1 < len(headers) <= 128 or len(set(headers)) != len(headers):
        raise ValueError("Invalid catalog header order in verified upstream Makefile")
    run([
        str(perl), (source / "src" / "backend" / "catalog" / "genbki.pl").as_posix(),
        f"--include-path={(source / 'src' / 'include').as_posix()}", "--set-version=17",
        *[(catalog / header).as_posix() for header in headers],
    ], share, "generate upstream PostgreSQL catalog", 45)
    snowball = source / "src" / "backend" / "snowball"
    run([str(perl), (snowball / "snowball_create.pl").as_posix(), "--input", snowball.as_posix(), "--outdir", share.as_posix()],
        root, "generate upstream Snowball catalog", 45)
    mappings = {
        "src/backend/utils/misc/postgresql.conf.sample": "postgresql.conf.sample",
        "src/backend/libpq/pg_hba.conf.sample": "pg_hba.conf.sample",
        "src/backend/libpq/pg_ident.conf.sample": "pg_ident.conf.sample",
        "src/backend/catalog/system_functions.sql": "system_functions.sql",
        "src/backend/catalog/system_views.sql": "system_views.sql",
        "src/backend/catalog/information_schema.sql": "information_schema.sql",
        "src/backend/catalog/sql_features.txt": "sql_features.txt",
        "src/pl/plpgsql/src/plpgsql.control": "extension/plpgsql.control",
        "src/pl/plpgsql/src/plpgsql--1.0.sql": "extension/plpgsql--1.0.sql",
    }
    for original, output in mappings.items():
        copy_regular(source / original, share / output)
    for path in (source / "src" / "timezone" / "tznames").iterdir():
        if path.is_file() and path.name not in {"Makefile", "README", "meson.build"}:
            copy_regular(path, share / "timezonesets" / path.name)
    for path in (snowball / "stopwords").glob("*.stop"):
        copy_regular(path, share / "tsearch_data" / path.name)
    with socket.socket() as reservation:
        reservation.bind(("127.0.0.1", 0))
        port = reservation.getsockname()[1]
    SECRET = secrets.token_hex(32)
    password = root / "password.txt"
    with password.open("x", encoding="ascii", newline="\n") as output:
        output.write(SECRET + "\n")
    with (root / "db.conn").open("x", encoding="ascii", newline="\n") as output:
        output.write(f"hostaddr=127.0.0.1 host=127.0.0.1 port={port} dbname=postgres user=ku_native_test password={SECRET} sslmode=disable\n")
    run([
        str(bindir / "initdb.exe"), "-D", str(root / "data"), "-U", "ku_native_test",
        "--auth=scram-sha-256", f"--pwfile={password}", "--encoding=UTF8", "--no-locale",
        "--no-instructions", "-c", "timezone=GMT", "-c", "log_timezone=GMT",
        "-c", "shared_buffers=16MB", "-c", "max_connections=10",
    ], root, "initialize isolated SCRAM PostgreSQL cluster", 60)
    manifest = {"format": 1, "version": VERSION, "source_sha256": SOURCE_SHA256, "source_url": SOURCE_URL, "port": port}
    with (root / "fixture.json").open("x", encoding="utf-8", newline="\n") as output:
        json.dump(manifest, output, indent=2)
        output.write("\n")


def require_live_pass(output: bytes) -> str:
    decoded = output.decode("utf-8", errors="replace")
    summary = re.search(r"(?m)^test result: ok\. 1 passed; 0 failed; 0 ignored;", decoded)
    if "skip:" in decoded.lower() or summary is None:
        raise RuntimeError("The live PostgreSQL test skipped instead of running")
    return decoded.replace(SECRET, "<redacted>") if SECRET else decoded


def verify(args: argparse.Namespace) -> None:
    if os.name != "nt":
        raise ValueError("This isolated PostgreSQL fixture requires Windows")
    root = args.fixture.resolve(strict=True)
    if root.parent != TARGET.resolve() or not re.fullmatch(re.escape(PREFIX) + r"[0-9a-f]{32}", root.name):
        raise ValueError("Verification only accepts a dedicated fixture created under this repository target")
    with operation(root) as owned:
        # Preserve any previous receipt when admission is refused. Presence of
        # either fence is sufficient; never interpret it as ownership authority.
        owned.uncertain = True
        if present(root / "server.active") or present(root / "data" / "postmaster.pid"):
            raise ValueError("PostgreSQL fixture is active or already-running; preserve it for investigation")
        owned.uncertain = False
        verify_owned(args, root, owned)


def verify_owned(args: argparse.Namespace, root: Path, owned: FixtureOperation) -> None:
    global SECRET
    # A failed rerun must not leave the previous success marker. Only unlink
    # this leaf after checking the fixture root; do not resolve it or delete trees.
    (root / "verification.json").unlink(missing_ok=True)
    manifest = json.loads((root / "fixture.json").read_text(encoding="utf-8"))
    if manifest.get("format") != 1 or manifest.get("version") != VERSION or manifest.get("source_sha256") != SOURCE_SHA256:
        raise ValueError("Unexpected PostgreSQL fixture manifest")
    port = manifest.get("port")
    if not isinstance(port, int) or not 1024 <= port <= 65535:
        raise ValueError("Invalid fixture port")
    test_binary = args.test_binary.resolve(strict=True)
    ku_binary = args.ku_binary.resolve(strict=True)
    portable = root / "portable"
    fixture_environment(portable)
    SECRET = (root / "password.txt").read_text(encoding="ascii").strip()
    os.environ["KU_PG_TEST_CONNINFO_FILE"] = str(root / "db.conn")
    os.environ["KU_BIN"] = str(ku_binary)
    ctl = str(portable / "bin" / "pg_ctl.exe")
    start = [ctl, "start", "-D", str(root / "data"), "-l", str(root / "server.log"), "-w", "-t", "20",
             "-o", f"-h 127.0.0.1 -p {port} -c shared_buffers=16MB -c max_connections=10 -c timezone=GMT -c log_timezone=GMT -c logging_collector=off"]
    windows_job = None
    process = None
    launch_attempted = False
    started_by_attempt = False
    succeeded = False
    primary = None
    owned.uncertain = True
    write_marker(root / "server.active", "owned PostgreSQL startup in progress\n")
    try:
        with (root / "startup.log").open("ab", buffering=0) as log:
            # Contain pg_ctl before it can launch CMD/postgres descendants.
            launch_attempted = True
            process = subprocess.Popen(start, cwd=root, stdin=subprocess.DEVNULL, stdout=log, stderr=log,
                                       creationflags=subprocess.CREATE_NEW_PROCESS_GROUP | subprocess.CREATE_NO_WINDOW | 0x00000004)
            try:
                windows_job = BOUNDS.WindowsJob.attach(process)
            except BOUNDS.WindowsJobSetupError as error:
                # Failed setup may still own a Job whose CloseHandle failed.
                # Keep that exact owner; the child has not been resumed.
                windows_job = error.job
                raise
            if windows_job is None:
                raise RuntimeError("Could not contain the temporary PostgreSQL server in a Windows Job")
            BOUNDS.resume_suspended_windows_process(process)
            if process.wait(timeout=25) != 0:
                raise RuntimeError("Temporary PostgreSQL startup failed; inspect its private startup/server log")
            started_by_attempt = True
        print(f"Started isolated PostgreSQL 17.10 on 127.0.0.1:{port}", flush=True)
        output = run([str(test_binary), "--exact", "native_pg_query_poller_live_loopback_roundtrip",
                      "--ignored", "--nocapture", "--test-threads=1"], REPO,
                     "live PostgreSQL query acceptance", 90, include_stderr=True)
        decoded = require_live_pass(output)
        with (root / "live-test.log").open("a", encoding="utf-8", newline="\n") as log:
            log.write(decoded)
        print(decoded, end="", flush=True)
        succeeded = True
    except BaseException as error:
        primary = error
    finally:
        cleanup = BOUNDS.CleanupErrors()
        stop_failed = False
        stop_stage = "PostgreSQL stop PID check"
        try:
            if started_by_attempt and present(root / "data" / "postmaster.pid"):
                stop_stage = "PostgreSQL stop"
                run([ctl, "stop", "-D", str(root / "data"), "-m", "immediate", "-w", "-t", "20"],
                    root, "stop isolated PostgreSQL cluster", 25)
        except BaseException as error:
            stop_failed = True
            if primary is None:
                primary = error
            else:
                cleanup.add(stop_stage, error)
        # Preserve the existing 25-second pg_ctl bound plus a single five-second
        # fallback budget. Failure of one owner operation cannot skip another.
        cleanup_deadline = time.monotonic() + 5
        if process is not None:
            cleanup.attempt("PostgreSQL process termination", lambda: BOUNDS.kill_process_tree(process, windows_job))
        if windows_job is not None:
            drained = cleanup.attempt("PostgreSQL Job drain", lambda: windows_job.wait_empty(cleanup_deadline))
            if drained is not True:
                cleanup.add("PostgreSQL Job drain unconfirmed")
            cleanup.attempt("PostgreSQL Job close", windows_job.close)
        if process is not None:
            cleanup.attempt("PostgreSQL root wait", lambda: process.wait(
                timeout=max(0.0, cleanup_deadline - time.monotonic()),
            ))
        if cleanup.attempt("PostgreSQL final PID check", lambda: present(root / "data" / "postmaster.pid")):
            cleanup.add("PostgreSQL shutdown was not confirmed; preserve fixture for investigation")
        if launch_attempted and process is None:
            # A constructor exception need not prove that no process was ever
            # created. Without its actual returned handle, retain both fences;
            # never discover/adopt an owner by PID or infer safety from absence.
            cleanup.add("PostgreSQL startup ownership was not confirmed")
        if not cleanup.failed and not stop_failed:
            cleanup.attempt("PostgreSQL active marker release", (root / "server.active").unlink)
            if not cleanup.failed:
                owned.uncertain = False
        try:
            cleanup.raise_if_any(primary)
        except BaseException as error:
            if owned.uncertain:
                error.job = windows_job
                error.process = process
                error.fixture_root = root
            raise
        # The contained Job was observed empty before close; this does not prove
        # containment of processes outside the owned Job.
        print("PostgreSQL fixture cleanup checks passed; fixture retained for reproducible reruns", flush=True)
    if succeeded:
        owned.receipt = {
            "version": VERSION,
            "source_sha256": SOURCE_SHA256,
            "test_binary": str(test_binary),
            "ku_binary": str(ku_binary),
            "verified_at_utc": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
            "live_test_passed": True,
            "server_stopped": True,
        }


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    subcommands = parser.add_subparsers(dest="command", required=True)
    preparation = subcommands.add_parser("prepare")
    preparation.add_argument("--installed-root", type=Path, required=True)
    preparation.add_argument("--perl", type=Path, required=True)
    preparation.add_argument("--source-archive", type=Path)
    verification = subcommands.add_parser("verify")
    verification.add_argument("--fixture", type=Path, required=True)
    verification.add_argument("--test-binary", type=Path, required=True)
    verification.add_argument("--ku-binary", type=Path, required=True)
    args = parser.parse_args()
    try:
        if args.command == "prepare":
            prepare(args)
        else:
            verify(args)
    except (OSError, RuntimeError, ValueError, subprocess.TimeoutExpired) as error:
        message = str(error)
        if SECRET:
            message = message.replace(SECRET, "<redacted>")
        raise SystemExit(f"PG loopback fixture failed: {message}") from None


if __name__ == "__main__":
    main()
