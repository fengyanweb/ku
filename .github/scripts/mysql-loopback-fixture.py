#!/usr/bin/env python3
"""Opt-in, disposable MySQL/MariaDB fixtures; never a system service.

prepare --installed-root PATH
verify --fixture PATH --test-binary PATH --ku-binary PATH
cleanup --fixture PATH
exercise --product mysql|mariadb --installed-root PATH --test-binary PATH --ku-binary PATH

Windows keeps its exact 8.0.29 snapshot under target/mysql-loopback-8.0.29-UUID.
POSIX uses a private ku-{mysql,mariadb}-live-UUID temporary root, trusted explicit
installed executables/share files, and a bounded matching client SDK copy. Only
newly generated credentials and data are used; no configuration is copied and
this script downloads no software. A same-user administrator replacing files or
processes concurrently is outside the fixture boundary. The Windows server pin
is a compatibility fixture, not a production version recommendation.

MySQL 8.0 still reads .mylogin.cnf with --no-defaults. MYSQL_TEST_LOGIN_FILE is
therefore forced to an absent private path; --no-login-paths needs MySQL 8.2.
See https://dev.mysql.com/doc/refman/8.0/en/option-files.html .
"""

from __future__ import annotations

import argparse
from contextlib import contextmanager
import importlib.util
import json
import os
from pathlib import Path
import re
import secrets
import shutil
import socket
import stat
import signal
import subprocess
import sys
import threading
import time
from typing import Iterator

sys.dont_write_bytecode = True
SPEC = importlib.util.spec_from_file_location(
    "ku_mysql_pg_fixture_helpers", Path(__file__).with_name("pg-loopback-fixture.py")
)
assert SPEC is not None and SPEC.loader is not None
PG = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(PG)
BOUNDS = PG.BOUNDS

REPO = Path(__file__).resolve().parents[2]
TARGET = REPO / "target"
VERSION = "8.0.29"
PREFIX = f"mysql-loopback-{VERSION}-"
LIVE_TEST = "native_mysql_client_live_loopback_roundtrip"
MAX_JSON_BYTES = 8192
MAX_FILES = 2048
MAX_COPY_BYTES = 512 * 1024 * 1024
MAX_FILE_BYTES = 256 * 1024 * 1024
MAX_SERVER_LOG_BYTES = 2 * 1024 * 1024
MAX_CLEANUP_FILES = 8192
START_TIMEOUT = 30
TEST_TIMEOUT = 120
SECRETS: list[str] = []
PRIVATE_LOG_ROOT: Path | None = None


def windows_only() -> None:
    if os.name != "nt":
        raise ValueError("This fixture currently requires Windows and MySQL 8.0.29")


def present(path: Path) -> bool:
    try:
        path.lstat()
        return True
    except FileNotFoundError:
        return False


def plain(path: Path, *, directory: bool = False) -> Path:
    path = path.absolute()
    if str(path).startswith("\\\\"):
        raise ValueError("Network paths are not allowed for an isolated fixture")
    for entry in (path, *path.parents):
        info = entry.lstat()
        if stat.S_ISLNK(info.st_mode) or getattr(info, "st_file_attributes", 0) & 0x400:
            raise ValueError("Symlinks and reparse points are not fixture inputs")
    mode = path.stat().st_mode
    if not (stat.S_ISDIR(mode) if directory else stat.S_ISREG(mode)):
        raise ValueError("Expected a plain fixture directory or file")
    return path


def bounded_bytes(path: Path, limit: int = MAX_JSON_BYTES) -> bytes:
    path = plain(path)
    if path.stat().st_size > limit:
        raise ValueError("Fixture input exceeds its byte bound")
    with path.open("rb") as source:
        data = source.read(limit + 1)
    if len(data) > limit:
        raise ValueError("Fixture input grew beyond its byte bound")
    return data


def no_duplicates(pairs: list[tuple[str, object]]) -> dict[str, object]:
    result: dict[str, object] = {}
    for key, value in pairs:
        if key in result:
            raise ValueError("Duplicate fixture JSON key")
        result[key] = value
    return result


def read_json(path: Path) -> dict:
    try:
        value = json.loads(bounded_bytes(path), object_pairs_hook=no_duplicates)
    except (UnicodeError, json.JSONDecodeError, RecursionError) as error:
        raise ValueError("Invalid fixture JSON") from error
    if not isinstance(value, dict):
        raise ValueError("Fixture JSON must be an object")
    return value


def write_private(path: Path, text: str, *, replace: bool = False) -> None:
    plain(path.parent, directory=True)
    if path.exists() or path.is_symlink():
        plain(path)
        if not replace:
            raise ValueError("Refusing to overwrite a fixture file")
    with path.open("w" if replace else "x", encoding="utf-8", newline="\n") as output:
        if os.name != "nt":
            os.fchmod(output.fileno(), 0o600)
        output.write(text)


def write_json(path: Path, value: dict, *, replace: bool = False) -> None:
    write_private(path, json.dumps(value, indent=2) + "\n", replace=replace)


def run(command: list[str], cwd: Path, label: str, timeout: int = 30) -> bytes:
    previous = BOUNDS.COMMAND_TIMEOUT_SECONDS
    BOUNDS.COMMAND_TIMEOUT_SECONDS = timeout
    try:
        result = BOUNDS.run_bounded(command, cwd, label)
        return result.stdout + result.stderr
    except (SystemExit, OSError, RuntimeError, subprocess.TimeoutExpired) as error:
        # Initializer output may contain an as-yet unknown random root password.
        # Never forward a command, stdout/stderr or an exception carrying it.
        if PRIVATE_LOG_ROOT is not None:
            private = str(error).encode("utf-8", errors="replace")[:MAX_SERVER_LOG_BYTES]
            write_private(PRIVATE_LOG_ROOT / "command-error.log", private.decode("utf-8", errors="ignore"), replace=True)
        raise RuntimeError(f"{label} failed (private diagnostic output suppressed)") from None
    finally:
        BOUNDS.COMMAND_TIMEOUT_SECONDS = previous


@contextmanager
def isolated_environment(root: Path) -> Iterator[None]:
    global PRIVATE_LOG_ROOT
    previous = dict(os.environ)
    previous_log_root = PRIVATE_LOG_ROOT
    allowed = {
        "SYSTEMROOT", "WINDIR", "COMSPEC", "PATHEXT", "PATH", "PROCESSOR_ARCHITECTURE",
        "NUMBER_OF_PROCESSORS", "INCLUDE", "LIB", "LIBPATH", "VSINSTALLDIR",
        "VCINSTALLDIR", "VCTOOLSINSTALLDIR", "WINDOWSSDKDIR", "WINDOWSSDKVERSION",
        "UNIVERSALCRTSDKDIR", "UCRTVERSION",
        # The Ku CLI locates vswhere through these non-secret installation roots
        # when the caller is not already inside a Visual Studio developer shell.
        "PROGRAMFILES", "PROGRAMFILES(X86)", "PROGRAMW6432",
    }
    env = {key: value for key, value in previous.items() if key.upper() in allowed}
    portable = root / "portable"
    env["PATH"] = os.pathsep.join((str(portable / "bin"), str(portable / "lib"), env.get("PATH", "")))
    env["TEMP"] = env["TMP"] = str(root / "tmp")
    env["TZ"] = "UTC"
    # Unlike --no-login-paths, this is supported by the pinned MySQL 8.0.29.
    login = root / "never-created.mylogin.cnf"
    if login.exists() or login.is_symlink():
        raise ValueError("Private disabled login-path file must not exist")
    env["MYSQL_TEST_LOGIN_FILE"] = str(login)
    env["MYSQL_HISTFILE"] = os.devnull
    os.environ.clear()
    os.environ.update(env)
    PRIVATE_LOG_ROOT = root
    try:
        yield
    finally:
        os.environ.clear()
        os.environ.update(previous)
        PRIVATE_LOG_ROOT = previous_log_root


def private_fixture() -> Path:
    TARGET.mkdir(exist_ok=True)
    plain(TARGET, directory=True)
    old_target, old_prefix = PG.TARGET, PG.PREFIX
    PG.TARGET, PG.PREFIX = TARGET, PREFIX
    try:
        return PG.private_fixture()
    finally:
        PG.TARGET, PG.PREFIX = old_target, old_prefix


def fixture_root(path: Path) -> Path:
    path = plain(path, directory=True)
    if path.parent != plain(TARGET, directory=True) or not re.fullmatch(re.escape(PREFIX) + r"[0-9a-f]{32}", path.name):
        raise ValueError("Only a dedicated MySQL fixture under repository target is accepted")
    return path


@contextmanager
def operation(root: Path) -> Iterator[None]:
    lock = root / "operation.lock"
    with lock.open("x", encoding="ascii") as output:
        output.write(str(os.getpid()))
    try:
        yield
    finally:
        # A pid marker left by an unconfirmed shutdown deliberately blocks reuse.
        if not present(root / "server.pid") and not present(root / "server.active"):
            lock.unlink(missing_ok=True)


def copy_installation(installed: Path, root: Path) -> None:
    installed = plain(installed, directory=True)
    portable = root / "portable"
    portable.mkdir()
    count = total = 0
    deadline = time.monotonic() + 60

    def copy(source: Path, destination: Path) -> None:
        nonlocal count, total
        source = plain(source)
        count += 1
        if count > MAX_FILES or source.stat().st_size > MAX_FILE_BYTES:
            raise ValueError("MySQL snapshot exceeds its file bound")
        destination.parent.mkdir(parents=True, exist_ok=True)
        copied = 0
        with source.open("rb") as incoming, destination.open("xb") as outgoing:
            while chunk := incoming.read(65536):
                copied += len(chunk)
                total += len(chunk)
                if copied > MAX_FILE_BYTES or total > MAX_COPY_BYTES or time.monotonic() > deadline:
                    raise ValueError("MySQL snapshot exceeds its byte/time bound")
                outgoing.write(chunk)

    for name in ("mysqld.exe", "mysql.exe", "mysqladmin.exe"):
        copy(installed / "bin" / name, portable / "bin" / name)
    with os.scandir(installed / "bin") as entries:
        for entry in entries:
            count += 1
            if count > MAX_FILES or time.monotonic() > deadline:
                raise ValueError("MySQL snapshot binary directory walk exceeds its bound")
            if entry.name.lower().endswith(".dll"):
                copy(Path(entry.path), portable / "bin" / entry.name)
    for name in ("libmysql.dll", "libmysql.lib"):
        copy(installed / "lib" / name, portable / "lib" / name)
    for directory in ("share", "include"):
        pending = [(plain(installed / directory, directory=True), portable / directory)]
        while pending:
            source, destination = pending.pop()
            destination.mkdir(parents=True, exist_ok=True)
            with os.scandir(source) as entries:
                for entry in entries:
                    count += 1  # Count directories and entries before scheduling them.
                    if count > MAX_FILES or time.monotonic() > deadline:
                        raise ValueError("MySQL snapshot directory walk exceeds its bound")
                    child = Path(entry.path)
                    if entry.is_dir(follow_symlinks=False):
                        pending.append((plain(child, directory=True), destination / entry.name))
                    else:
                        copy(child, destination / entry.name)
    plain(portable / "share" / "english" / "errmsg.sys")
    plain(portable / "include" / "mysql.h")
    (portable / "empty-plugins").mkdir()


def validate_config(config: dict, admin: dict) -> None:
    if set(config) != {"host", "port", "user", "password", "database"}:
        raise ValueError("Unexpected test connection fields")
    if config["host"] != "127.0.0.1" or type(config["port"]) is not int or not 1024 <= config["port"] <= 65535:
        raise ValueError("Test connections must use explicit IPv4 loopback and a private port")
    if set(admin) != {"user", "password"}:
        raise ValueError("Unexpected admin connection fields")
    for value, prefix in ((config["user"], "ku_test_"), (config["database"], "ku_db_"), (admin["user"], "ku_admin_")):
        if not isinstance(value, str) or not re.fullmatch(prefix + r"[0-9a-f]{16}", value):
            raise ValueError("Fixture account/database name is not generated")
    for password in (config["password"], admin["password"]):
        if not isinstance(password, str) or not re.fullmatch(r"[0-9a-f]{64}", password):
            raise ValueError("Fixture password has an invalid generated format")


def prepare(args: argparse.Namespace) -> Path:
    windows_only()
    installed = plain(args.installed_root, directory=True)
    root = private_fixture()
    (root / "tmp").mkdir()
    with operation(root):
        copy_installation(installed, root)
        with isolated_environment(root):
            mysqld = root / "portable" / "bin" / "mysqld.exe"
            version = run([str(mysqld), "--no-defaults", "--version"], root, "check private MySQL version")
            if not re.search(rb"\bVer 8\.0\.29 for Win64\b", version) or b"MariaDB" in version:
                raise ValueError("The isolated fixture requires exactly MySQL 8.0.29 Win64")
            run([str(mysqld), "--no-defaults", "--initialize", "--console",
                 f"--basedir={root / 'portable'}", f"--datadir={root / 'data'}"],
                root, "initialize private MySQL data", 120)
        auto = bounded_bytes(root / "data" / "auto.cnf").decode("ascii")
        identity = re.search(r"(?m)^server-uuid=([0-9a-f-]{36})\s*$", auto)
        if identity is None or not re.fullmatch(r"[0-9a-f]{8}(?:-[0-9a-f]{4}){3}-[0-9a-f]{12}", identity[1]):
            raise ValueError("Initialized MySQL server identity is missing")
        with socket.socket() as reservation:
            reservation.bind(("127.0.0.1", 0))
            port = reservation.getsockname()[1]
        config = {"host": "127.0.0.1", "port": port, "user": "ku_test_" + secrets.token_hex(8),
                  "password": secrets.token_hex(32), "database": "ku_db_" + secrets.token_hex(8)}
        admin = {"user": "ku_admin_" + secrets.token_hex(8), "password": secrets.token_hex(32)}
        validate_config(config, admin)
        write_json(root / "db.json", config)
        write_json(root / "admin.json", admin)
        write_json(root / "fixture.json", {"format": 1, "version": VERSION, "server_uuid": identity[1]})
    print(f"Prepared private MySQL fixture (server not running): {root}", flush=True)
    return root


def write_startup_files(root: Path, config: dict, admin: dict) -> None:
    validate_config(config, admin)
    test = f"'{config['user']}'@'127.0.0.1'"
    operator = f"'{admin['user']}'@'127.0.0.1'"
    # MySQL 8.0 treats '_' as a wildcard in database-level GRANT patterns even
    # in quoted identifiers. CREATE DATABASE uses the literal name; only the
    # privilege pattern needs escapes. No percent/backslash passes validation.
    # https://dev.mysql.com/doc/refman/8.0/en/grant.html
    grant_database = config["database"].replace("_", r"\_")
    statements = [
        f"ALTER USER 'root'@'localhost' IDENTIFIED BY '{secrets.token_hex(32)}' ACCOUNT LOCK;",
        f"CREATE DATABASE IF NOT EXISTS `{config['database']}` CHARACTER SET utf8mb4;",
        f"CREATE USER IF NOT EXISTS {test} IDENTIFIED BY '{config['password']}';",
        f"ALTER USER {test} IDENTIFIED BY '{config['password']}';",
        f"GRANT SELECT,INSERT,UPDATE,DELETE,CREATE,DROP,INDEX,ALTER ON `{grant_database}`.* TO {test};",
        f"CREATE USER IF NOT EXISTS {operator} IDENTIFIED BY '{admin['password']}';",
        f"ALTER USER {operator} IDENTIFIED BY '{admin['password']}';",
        f"GRANT SHUTDOWN ON *.* TO {operator};",
    ]
    write_private(root / "init.sql", "\n".join(statements) + "\n", replace=True)
    write_private(root / "admin.cnf", "\n".join((
        "[client]", "protocol=TCP", "host=127.0.0.1", f"port={config['port']}",
        f"user={admin['user']}", f"password={admin['password']}", "connect-timeout=1",
        "ssl-mode=REQUIRED", "",
    )), replace=True)


def server_command(root: Path, port: int) -> list[str]:
    return [str(root / "portable" / "bin" / "mysqld.exe"), "--no-defaults", "--console",
            "--no-monitor", "--mysqlx=OFF", "--skip-name-resolve", "--bind-address=127.0.0.1",
            f"--port={port}", "--port-open-timeout=0", f"--basedir={root / 'portable'}",
            f"--datadir={root / 'data'}", f"--pid-file={root / 'server.pid'}",
            f"--init-file={root / 'init.sql'}", f"--tmpdir={root / 'tmp'}",
            f"--plugin-dir={root / 'portable' / 'empty-plugins'}", "--persisted-globals-load=OFF",
            "--max-connections=10", "--innodb-buffer-pool-size=33554432", "--skip-log-bin",
            "--local-infile=OFF", "--general-log=OFF", "--slow-query-log=OFF",
            "--skip-named-pipe", "--shared-memory=OFF", "--default-time-zone=+00:00"]


class MysqlProcess:
    """One owned, suspended-before-Job server process and its bounded log."""

    def __init__(self, root: Path, command: list[str]) -> None:
        self.root, self.command = root, command
        self.process = None
        self.job = None
        self.reader = None
        self.reader_start_state = "not_attempted"
        self.output = bytearray()
        self.lock = threading.Lock()
        self.failed = threading.Event()
        self.reader_error = ""
        self.cleanup_uncertain = False
        self.stop_complete = False
        self.ready = False

    def start(self) -> None:
        if self.process is not None or self.stop_complete:
            raise RuntimeError("An owned MySQL process cannot be restarted on the same supervisor")
        # A process can fail to terminate before ever writing its pid file.
        # Keep a separate marker until owning-process exit is confirmed.
        write_private(self.root / "server.active", "owned MySQL startup in progress\n")
        try:
            self.process = subprocess.Popen(self.command, cwd=self.root, stdin=subprocess.DEVNULL,
                                            stdout=subprocess.PIPE, stderr=subprocess.STDOUT, bufsize=0,
                                            creationflags=0x08000000 | 0x00000200 | 0x00000004)
            self.job = BOUNDS.WindowsJob.attach(self.process)
            if self.job is None:
                raise RuntimeError("Could not contain the suspended MySQL process")
            # Retain the owner before start: interruption may happen after the
            # thread exists but before Thread.start returns normally.
            self.reader = threading.Thread(target=self.drain, daemon=True)
            self.reader_start_state = "unknown"
            self.reader.start()
            self.reader_start_state = "started"
            BOUNDS.resume_suspended_windows_process(self.process)
        except BaseException as primary:
            if isinstance(primary, BOUNDS.WindowsJobSetupError):
                self.job = primary.job
            cleanup = BOUNDS.CleanupErrors()
            if self.process is None:
                # Popen failed without returning an owned child handle.
                cleanup.attempt("active_marker_unlink", lambda: plain(self.root / "server.active").unlink())
            else:
                cleanup.attempt("server_stop", self.stop)
            cleanup.raise_if_any(primary)

    def drain(self) -> None:
        assert self.process is not None and self.process.stdout is not None
        errors = BOUNDS.CleanupErrors()
        try:
            with (self.root / "server.log").open("xb") as log:
                while chunk := self.process.stdout.read(8192):
                    with self.lock:
                        room = MAX_SERVER_LOG_BYTES - len(self.output)
                        kept = chunk[:room]
                        self.output.extend(kept)
                    log.write(kept)
                    if len(chunk) > room:
                        self.failed.set()
                        errors.add("output_limit")
                        break
        except Exception as error:
            self.failed.set()
            errors.add("reader_read", error)
        finally:
            if self.failed.is_set():
                # A failed terminate must reach the supervisor, never escape
                # only as a daemon-thread traceback containing private output.
                errors.attempt("reader_terminate", lambda: BOUNDS.kill_process_tree(self.process, self.job))
                with self.lock:
                    self.reader_error = errors.summary()

    def wait_ready(self) -> None:
        deadline = time.monotonic() + START_TIMEOUT
        while time.monotonic() < deadline:
            if self.failed.is_set() or self.process.poll() is not None:
                raise RuntimeError("Private MySQL failed before readiness; no connection attempted")
            with self.lock:
                ready = b"ready for connections" in self.output
            if ready and (self.root / "server.pid").exists():
                pid = bounded_bytes(self.root / "server.pid", 32).strip()
                if pid != str(self.process.pid).encode("ascii"):
                    raise RuntimeError("Private MySQL pid does not match its owned process")
                self.ready = True
                return
            time.sleep(0.05)
        raise RuntimeError("Private MySQL readiness exceeded its absolute deadline")

    def stop(self) -> None:
        if self.process is None or self.stop_complete:
            return
        deadline = time.monotonic() + 10
        cleanup = BOUNDS.CleanupErrors()
        primary = None

        def attempt(label, action):
            nonlocal primary
            try:
                return action()
            except BaseException as error:
                if primary is None:
                    primary = error
                else:
                    cleanup.add(label, error)
                return None

        attempt("tree_terminate", lambda: BOUNDS.kill_process_tree(self.process, self.job))
        attempt("root_wait", lambda: self.process.wait(timeout=max(0, deadline - time.monotonic())))
        if self.job is not None:
            if attempt("job_drain", lambda: self.job.wait_empty(deadline)) is not True:
                cleanup.add("job_drain_unconfirmed")
            attempt("job_close", self.job.close)
        reader_finished = self.reader is None
        if self.reader is not None:
            joined = False
            def join_reader():
                self.reader.join(timeout=min(5, max(0, deadline - time.monotonic())))
                return True

            if self.reader_start_state == "not_attempted":
                reader_finished = True
            elif self.reader_start_state == "unknown":
                # Thread.start can fail after OS thread creation but before
                # ident/_started registration. is_alive()==False alone is not
                # proof the thread cannot later read the pipe. Even an ident
                # needs a normally returned join before registration is known.
                ident = attempt("reader_identity", lambda: self.reader.ident)
                if ident is not None and attempt("reader_join", join_reader) is True:
                    self.reader_start_state = "started"
                    joined = True
                else:
                    cleanup.add("reader_start_unknown")
            else:
                joined = attempt("reader_join", join_reader) is True
            if self.reader_start_state == "started" and joined:
                # After any failed join, even is_alive()==False is insufficient
                # to authorize synchronous close of a possibly locked pipe.
                reader_finished = attempt("reader_state", self.reader.is_alive) is False
            if not reader_finished:
                cleanup.add("reader_unfinished")
        if reader_finished and self.process.stdout is not None:
            attempt("stream_close", self.process.stdout.close)
        with self.lock:
            reader_error = self.reader_error
        if self.failed.is_set():
            cleanup.add("reader_failed")
            if reader_error:
                if primary is None:
                    primary = RuntimeError(reader_error)
                else:
                    cleanup.add("reader_failure")
        if self.cleanup_uncertain:
            cleanup.add("previous_cleanup_failure")
        if primary is not None or cleanup.failed:
            self.cleanup_uncertain = True
            cleanup.raise_if_any(primary)
        # Marker removal follows the assigned Job's zero-active observation
        # while open; root exit or pipe EOF cannot replace that observation.
        try:
            pid = self.root / "server.pid"
            if pid.exists():
                plain(pid).unlink()
            marker = self.root / "server.active"
            if present(marker):
                plain(marker).unlink()
        except BaseException:
            self.cleanup_uncertain = True
            raise
        # Startup failure and verify's finally may both stop this same owner.
        # Only a fully successful transaction permits a no-op second stop.
        self.stop_complete = True


def client_command(root: Path, name: str, *arguments: str) -> list[str]:
    return [str(root / "portable" / "bin" / name), f"--defaults-file={root / 'admin.cnf'}", *arguments]


def require_live_pass(output: bytes) -> str:
    text = output.decode("utf-8", errors="replace")
    if "skip:" in text.lower() or not re.search(r"(?m)^test result: ok\. 1 passed; 0 failed; 0 ignored;", text):
        raise RuntimeError("MySQL acceptance did not execute exactly one passing live test")
    for secret in SECRETS:
        text = text.replace(secret, "<redacted>")
    return text


def verify(args: argparse.Namespace) -> None:
    windows_only()
    root = fixture_root(args.fixture)
    with operation(root):
        record = root / "verification.json"
        if record.exists() or record.is_symlink():
            plain(record).unlink()
        if present(root / "server.pid") or present(root / "server.active"):
            raise ValueError("A MySQL active/pid marker already exists; refusing to reuse the instance")
        manifest = read_json(root / "fixture.json")
        if set(manifest) != {"format", "version", "server_uuid"} or type(manifest["format"]) is not int or manifest["format"] != 1 or manifest["version"] != VERSION:
            raise ValueError("Unexpected MySQL fixture identity")
        if not isinstance(manifest["server_uuid"], str) or not re.fullmatch(r"[0-9a-f]{8}(?:-[0-9a-f]{4}){3}-[0-9a-f]{12}", manifest["server_uuid"]):
            raise ValueError("Unexpected MySQL server UUID")
        config, admin = read_json(root / "db.json"), read_json(root / "admin.json")
        validate_config(config, admin)
        SECRETS[:] = [config["password"], admin["password"]]
        test_binary, ku_binary = plain(args.test_binary), plain(args.ku_binary)
        for directory in (root / "data", root / "tmp", root / "portable" / "empty-plugins"):
            plain(directory, directory=True)
        # No connection/health probe is used to discover or claim a port.
        with socket.socket() as reservation:
            reservation.setsockopt(socket.SOL_SOCKET, socket.SO_EXCLUSIVEADDRUSE, 1)
            reservation.bind(("127.0.0.1", config["port"]))
        server = None
        graceful = False
        primary = None
        cleanup_errors = BOUNDS.CleanupErrors()
        try:
            # Include partial startup-file creation and environment setup in
            # the same secret cleanup transaction as the running server.
            write_startup_files(root, config, admin)
            log = root / "server.log"
            if log.exists() or log.is_symlink():
                plain(log).unlink()
            with isolated_environment(root):
                os.environ["KU_MYSQL_TEST_CONFIG_FILE"] = str(root / "db.json")
                os.environ["KU_BIN"] = str(ku_binary)
                os.environ["KU_MYSQL_LIB"] = str(root / "portable" / "lib")
                os.environ["KU_MYSQL_INCLUDE"] = str(root / "portable" / "include")
                server = MysqlProcess(root, server_command(root, config["port"]))
                server.start()
                server.wait_ready()
                identity = run(client_command(root, "mysql.exe", "--batch", "--raw", "--skip-column-names", "--default-character-set=utf8mb4",
                                             "--execute=SELECT @@server_uuid, @@datadir, @@port"),
                               root, "verify private MySQL identity", 5).decode("utf-8").strip().split("\t")
                expected_dir = str(root / "data").replace("\\", "/").rstrip("/").casefold()
                if len(identity) != 3 or identity[0] != manifest["server_uuid"] or identity[1].replace("\\", "/").rstrip("/").casefold() != expected_dir or identity[2] != str(config["port"]):
                    raise RuntimeError("MySQL identity is not the newly created private instance")
                output = run([str(test_binary), "--exact", LIVE_TEST, "--ignored", "--nocapture", "--test-threads=1"],
                             REPO, "live MySQL query acceptance", TEST_TIMEOUT)
                text = require_live_pass(output)
                if server.failed.is_set() or server.process.poll() is not None:
                    raise RuntimeError("Private MySQL did not remain healthy during acceptance")
                run(client_command(root, "mysqladmin.exe", "shutdown"), root, "stop private MySQL", 15)
                server.process.wait(timeout=15)
                graceful = True
        except BaseException as error:
            primary = error
        finally:
            if server is not None:
                cleanup_errors.attempt("server_stop", server.stop)
            for name in ("init.sql", "admin.cnf"):
                def remove_secret(name=name):
                    path = root / name
                    if present(path):
                        plain(path).unlink()

                cleanup_errors.attempt(name.replace(".", "_") + "_unlink", remove_secret)
        cleanup_errors.raise_if_any(primary)
        if not graceful:
            raise RuntimeError("Private MySQL shutdown was not confirmed")
        write_private(root / "live-test.log", text, replace=True)
        write_json(record, {"format": 1, "version": VERSION, "server_uuid": manifest["server_uuid"],
                            "test_binary": str(test_binary), "ku_binary": str(ku_binary),
                            "live_test_passed": True, "server_stopped": True,
                            "verified_at_utc": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime())})
    print("Live MySQL test passed; owned server stopped. Run cleanup to remove the private fixture.", flush=True)


def cleanup(args: argparse.Namespace) -> None:
    windows_only()
    root = fixture_root(args.fixture)
    if any(present(root / name) for name in ("operation.lock", "server.pid", "server.active")):
        raise ValueError("Fixture operation/shutdown is not confirmed; refusing deletion")
    with operation(root):
        cleanup_locked(root)


def cleanup_locked(root: Path) -> None:
    manifest = read_json(root / "fixture.json")
    if type(manifest.get("format")) is not int or manifest.get("format") != 1 or manifest.get("version") != VERSION:
        raise ValueError("Refusing cleanup of an unrecognized fixture")
    # Enumerate and validate the entire bounded tree before deleting any entry.
    pending, files, directories = [root], [], []
    deadline = time.monotonic() + 15
    while pending:
        directory = pending.pop()
        directories.append(plain(directory, directory=True))
        with os.scandir(directory) as entries:
            for entry in entries:
                if len(files) + len(directories) + len(pending) >= MAX_CLEANUP_FILES or time.monotonic() > deadline:
                    raise ValueError("Fixture cleanup scan exceeds its bound")
                path = Path(entry.path)
                if entry.is_dir(follow_symlinks=False):
                    pending.append(plain(path, directory=True))
                else:
                    files.append(plain(path))
    for path in files:
        if path == root / "operation.lock":
            continue
        if time.monotonic() > deadline:
            raise ValueError("Fixture cleanup exceeded its deadline; partial tree remains")
        path.unlink()
    for path in reversed(directories[1:]):
        path.rmdir()
    (root / "operation.lock").unlink()
    root.rmdir()
    print("Removed the stopped private MySQL fixture and its generated credentials (not recoverable).", flush=True)


# POSIX uses trusted installed executables in place and copies only its explicit
# matching client SDK. The Windows 8.0.29 path above keeps its original contract.
POSIX_FORMAT = 2
MAX_DATA_BYTES = 768 * 1024 * 1024
POSIX_LIVE_TESTS = {
    "darwin": "native_mysql_client_live_macos_loopback_roundtrip",
    "linux": "native_mysql_client_live_linux_loopback_roundtrip",
}


def posix_only() -> None:
    if os.name != "posix" or sys.platform not in POSIX_LIVE_TESTS:
        raise ValueError("POSIX MySQL fixture requires macOS or Linux")
    if os.geteuid() == 0:
        raise ValueError("Run the disposable database as a non-root user")


def posix_temporary_root() -> Path:
    # macOS per-user TMPDIR can exceed the Unix socket path bound. This short
    # system temporary parent still receives an exclusive 0700 random root.
    return plain(Path("/tmp").resolve(), directory=True)


def posix_fixture_root(path: Path) -> Path:
    root = plain(path, directory=True)
    temporary = posix_temporary_root()
    if root.parent != temporary or not re.fullmatch(r"ku-(?:mysql|mariadb)-live-[0-9a-f]{32}", root.name):
        raise ValueError("Only a dedicated POSIX temporary fixture is accepted")
    info = root.stat()
    if info.st_uid != os.geteuid() or stat.S_IMODE(info.st_mode) != 0o700:
        raise ValueError("POSIX fixture must be private to the current user")
    return root


def posix_manifest(root: Path) -> dict:
    value = read_json(root / "fixture.json")
    fields = {"format", "product", "version", "server", "basedir", "client", "admin", "initializer",
              "client_library_source", "client_include_source", "client_library_family",
              "client_version", "client_version_id"}
    if set(value) != fields or type(value["format"]) is not int or value["format"] != POSIX_FORMAT:
        raise ValueError("Unexpected POSIX MySQL fixture metadata")
    product, version = value["product"], value["version"]
    if product not in ("mysql", "mariadb") or not isinstance(version, str) or not re.fullmatch(r"[A-Za-z0-9._-]{1,128}", version):
        raise ValueError("Unexpected POSIX database identity")
    if ("MariaDB" in version) != (product == "mariadb") or not root.name.startswith(f"ku-{product}-live-"):
        raise ValueError("POSIX fixture product and version disagree")
    for key in ("server", "client", "admin", "initializer"):
        path = Path(value[key])
        plain(path)
        if not os.access(path, os.X_OK):
            raise ValueError("Fixture executable is not executable")
    plain(Path(value["basedir"]), directory=True)
    plain(Path(value["client_include_source"]), directory=True)
    plain(Path(value["client_library_source"]))
    if value["client_library_family"] not in ("mysqlclient", "mariadb") or not isinstance(value["client_version"], str) or not re.fullmatch(r"[A-Za-z0-9._-]{1,128}", value["client_version"]):
        raise ValueError("Unexpected actual client library identity")
    if type(value["client_version_id"]) is not int or value["client_version_id"] <= 0:
        raise ValueError("Invalid actual client version ID")
    return value


def posix_executable(installed: Path, *names: str) -> Path:
    for name in names:
        candidate = installed / name
        if candidate.exists():
            resolved = plain(candidate.resolve())
            if os.access(resolved, os.X_OK):
                return resolved
    raise ValueError("Required database executable is absent from the specified installation")


def check_posix_tree(root: Path, *, volatile: bool = False) -> tuple[int, int]:
    count = total = 0
    pending = [root]
    deadline = time.monotonic() + 2
    while pending:
        candidate = pending.pop()
        try:
            directory = plain(candidate, directory=True)
            iterator = os.scandir(directory)
        except FileNotFoundError:
            if volatile and candidate != root:
                continue
            raise
        with iterator as entries:
            for entry in entries:
                count += 1
                if count > MAX_CLEANUP_FILES or time.monotonic() > deadline:
                    raise ValueError("POSIX fixture file scan exceeds its bound")
                path = Path(entry.path)
                try:
                    info = path.lstat()
                    # The private local socket is the sole non-regular leaf.
                    if path == root / "mysql.sock" and stat.S_ISSOCK(info.st_mode):
                        continue
                    if stat.S_ISDIR(info.st_mode):
                        pending.append(plain(path, directory=True))
                    else:
                        plain(path)
                        total += info.st_size
                        if total > MAX_DATA_BYTES:
                            raise ValueError("POSIX fixture data exceeds its byte bound")
                except FileNotFoundError:
                    # A running initializer/server can remove its own temporary
                    # files between enumeration and stat. Stopped cleanup uses
                    # the strict default; links and other errors never pass.
                    if not volatile:
                        raise
    return count, total


def copy_posix_sdk(root: Path, include: Path, library: Path) -> None:
    include = plain(include.resolve(), directory=True)
    library = plain(library.resolve())
    if not re.fullmatch(r"lib(?:mysqlclient|mariadb)(?:\.so(?:\.[0-9]+)*|(?:\.[0-9]+)*\.dylib)", library.name):
        raise ValueError("Explicit SDK library must identify a shared MySQL or MariaDB client")
    if not (include / "mysql.h").is_file():
        raise ValueError("Explicit SDK include directory has no mysql.h")
    sdk = root / "sdk"
    (sdk / "include").mkdir(parents=True)
    (sdk / "lib").mkdir()
    count = total = 0
    deadline = time.monotonic() + 30
    pending = [(include, sdk / "include")]

    def copy(source: Path, destination: Path) -> None:
        nonlocal count, total
        source = plain(source)
        count += 1
        if count > MAX_FILES or source.stat().st_size > MAX_FILE_BYTES:
            raise ValueError("POSIX SDK copy exceeds its file bound")
        with source.open("rb") as incoming, destination.open("xb") as outgoing:
            while chunk := incoming.read(65536):
                total += len(chunk)
                if total > MAX_COPY_BYTES or time.monotonic() > deadline:
                    raise ValueError("POSIX SDK copy exceeds its byte/time bound")
                outgoing.write(chunk)
    while pending:
        source, destination = pending.pop()
        with os.scandir(source) as entries:
            for entry in entries:
                count += 1
                if count > MAX_FILES or time.monotonic() > deadline:
                    raise ValueError("POSIX SDK walk exceeds its bound")
                child = Path(entry.path)
                if entry.is_dir(follow_symlinks=False):
                    plain(child, directory=True)
                    target = destination / entry.name
                    target.mkdir()
                    pending.append((child, target))
                else:
                    copy(child, destination / entry.name)
    copy(library, sdk / "lib" / library.name)


def posix_sdk_identity(root: Path, include: Path, library: Path) -> dict:
    include, library = plain(include.resolve(), directory=True), plain(library.resolve())
    family = "mysqlclient" if library.name.startswith("libmysqlclient") else "mariadb"
    header = root / "sdk" / "include" / ("mysql_version.h" if family == "mysqlclient" else "mariadb_version.h")
    text = bounded_bytes(header, 65536).decode("utf-8")
    macro = "LIBMYSQL_VERSION_ID" if family == "mysqlclient" else "MARIADB_PACKAGE_VERSION_ID"
    match = re.search(r"(?m)^#define\s+" + macro + r"\s+([0-9]{1,9})\s*$", text)
    if match is None:
        raise ValueError("Explicit SDK header does not identify its client ABI version")
    # Query the actual bounded library copy in a child with an absolute limit.
    # Loading the public client ABI and reading static version strings performs
    # no connection, initialization or service discovery.
    probe = ("import ctypes,json,sys; lib=ctypes.CDLL(sys.argv[1]); "
             "lib.mysql_get_client_info.restype=ctypes.c_char_p; "
             "lib.mysql_get_client_version.restype=ctypes.c_ulong; "
             "print(json.dumps({'version':lib.mysql_get_client_info().decode('ascii'),"
             "'version_id':lib.mysql_get_client_version()}))")
    output = run([sys.executable, "-B", "-I", "-c", probe, str(root / "sdk" / "lib" / library.name)],
                 root, "read explicit client SDK version", 5)
    if len(output) > 1024:
        raise ValueError("SDK version output exceeds its byte bound")
    identity = json.loads(output, object_pairs_hook=no_duplicates)
    if not isinstance(identity, dict) or set(identity) != {"version", "version_id"} or type(identity["version_id"]) is not int or identity["version_id"] != int(match[1]):
        raise ValueError("Explicit SDK library and header ABI versions disagree")
    if not isinstance(identity["version"], str) or not re.fullmatch(r"[A-Za-z0-9._-]{1,128}", identity["version"]):
        raise ValueError("Unexpected actual client version")
    return {"client_library_source": str(library), "client_include_source": str(include),
            "client_library_family": family, "client_version": identity["version"],
            "client_version_id": identity["version_id"]}


@contextmanager
def posix_environment(root: Path) -> Iterator[None]:
    global PRIVATE_LOG_ROOT
    previous, previous_log_root = dict(os.environ), PRIVATE_LOG_ROOT
    allowed = {"PATH", "DEVELOPER_DIR", "SDKROOT", "MACOSX_DEPLOYMENT_TARGET"}
    env = {key: value for key, value in previous.items() if key in allowed}
    env.update({"HOME": str(root), "TMPDIR": str(root.parent), "TMP": str(root / "tmp"),
                "TEMP": str(root / "tmp"), "LANG": "C", "LC_ALL": "C", "TZ": "UTC",
                "MYSQL_TEST_LOGIN_FILE": str(root / "never-created.mylogin.cnf"), "MYSQL_HISTFILE": os.devnull})
    if sys.platform == "linux":
        # Only this fixture's bounded, explicit SDK copy enters the loader path.
        env["LD_LIBRARY_PATH"] = str(root / "sdk" / "lib")
    os.environ.clear()
    os.environ.update(env)
    PRIVATE_LOG_ROOT = root
    previous_umask = os.umask(0o077)
    try:
        yield
    finally:
        os.umask(previous_umask)
        os.environ.clear()
        os.environ.update(previous)
        PRIVATE_LOG_ROOT = previous_log_root


class PosixMysqlProcess(MysqlProcess):
    """A directly owned session leader; no service manager or PID discovery."""

    def __init__(self, root: Path, command: list[str]) -> None:
        super().__init__(root, command)
        self.monitor_stop = threading.Event()
        self.monitor = None

    def start(self) -> None:
        if self.process is not None or self.stop_complete:
            raise RuntimeError("An owned POSIX database cannot be restarted")
        write_private(self.root / "server.active", "startup pending\n")
        try:
            self.process = subprocess.Popen(self.command, cwd=self.root, stdin=subprocess.DEVNULL,
                                            stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
                                            bufsize=0, start_new_session=True)
            write_private(self.root / "server.active", f"{self.process.pid}\n", replace=True)
            self.reader = threading.Thread(target=self.drain, daemon=True)
            self.reader_start_state = "unknown"
            self.reader.start()
            self.reader_start_state = "started"
            self.monitor = threading.Thread(target=self.watch_tree, daemon=True)
            self.monitor.start()
        except BaseException as primary:
            cleanup = BOUNDS.CleanupErrors()
            if self.process is None:
                cleanup.attempt("active_marker_unlink", lambda: (self.root / "server.active").unlink())
            else:
                cleanup.attempt("server_stop", self.stop)
            cleanup.raise_if_any(primary)

    def watch_tree(self) -> None:
        try:
            while not self.monitor_stop.wait(0.25):
                check_posix_tree(self.root, volatile=True)
        except BaseException as error:
            self.reader_error = "fixture_monitor_" + type(error).__name__
            self.failed.set()
            try:
                BOUNDS.kill_process_tree(self.process, None)
            except BaseException:
                self.cleanup_uncertain = True

    def stop(self) -> None:
        self.monitor_stop.set()
        if self.monitor is not None and self.monitor.ident is not None:
            self.monitor.join(timeout=3)
            if self.monitor.is_alive():
                self.cleanup_uncertain = True
        super().stop()


def initialize_posix(root: Path, command: list[str]) -> None:
    owner = PosixMysqlProcess(root, command)
    primary = None
    cleanup = BOUNDS.CleanupErrors()
    try:
        owner.start()
        if owner.process.wait(timeout=120) != 0 or owner.failed.is_set():
            raise RuntimeError("Private POSIX initialization failed; inspect its bounded private log")
    except BaseException as error:
        primary = error
    finally:
        cleanup.attempt("initializer_stop", owner.stop)
    cleanup.raise_if_any(primary)


def posix_server_resources(root: Path, installed: Path) -> list[str]:
    share = plain((installed / "share" / "mysql").resolve(), directory=True)
    plain(share / "english" / "errmsg.sys")
    charsets = plain(share / "charsets", directory=True)
    return [f"--plugin-dir={root / 'empty-plugins'}", f"--lc-messages-dir={share}",
            f"--character-sets-dir={charsets}"]


def posix_server_command(root: Path, manifest: dict, port: int) -> list[str]:
    command = [manifest["server"], "--no-defaults", "--skip-name-resolve", "--bind-address=127.0.0.1",
               f"--port={port}", f"--basedir={manifest['basedir']}", f"--datadir={root / 'data'}",
               f"--pid-file={root / 'server.pid'}", f"--socket={root / 'mysql.sock'}",
               f"--init-file={root / 'init.sql'}", f"--tmpdir={root / 'tmp'}",
               "--max-connections=10", "--innodb-buffer-pool-size=33554432", "--skip-log-bin",
               "--local-infile=OFF", "--general-log=OFF", "--slow-query-log=OFF", "--default-time-zone=+00:00"]
    command.extend(posix_server_resources(root, Path(manifest["basedir"])))
    if manifest["product"] == "mysql":
        command.extend(("--mysqlx=OFF", "--port-open-timeout=0", "--persisted-globals-load=OFF",
                        "--innodb-redo-log-capacity=33554432"))
    else:
        command.extend(("--innodb-log-file-size=8388608", "--skip-symbolic-links"))
    return command


def write_posix_startup(root: Path, config: dict, admin: dict, product: str) -> None:
    write_startup_files(root, config, admin)
    # Oracle's ssl-mode is not a MariaDB command-line option. Encryption settings
    # are intentionally outside this local disposable connection fixture.
    text = (root / "admin.cnf").read_text(encoding="utf-8")
    text = text.replace("ssl-mode=REQUIRED\n", "")
    write_private(root / "admin.cnf", text, replace=True)


def posix_failure_summary(phase: str, error: BaseException, root: Path) -> None:
    # Never format error text, command output or log lines: initialization can
    # contain a root password not present in SECRETS. Only fixed labels and a
    # bounded number of structural MySQL error codes may reach the CI console.
    phase = phase if phase in ("prepare", "verify") else "unknown"
    kind = type(error).__name__
    if kind not in {"RuntimeError", "ValueError", "OSError", "PermissionError", "FileNotFoundError",
                    "FileExistsError", "TimeoutExpired", "TimeoutError", "InterruptedError",
                    "KeyboardInterrupt", "SystemExit"}:
        kind = "unknown"
    codes: list[str] = []
    for name in ("server.log", "command-error.log"):
        try:
            output = bounded_bytes(root / name, MAX_SERVER_LOG_BYTES)
        except (OSError, ValueError):
            continue
        for match in re.finditer(rb"\[ERROR\]\s+\[(MY-[0-9]{6})\]", output):
            code = match[1].decode("ascii")
            if code not in codes:
                codes.append(code)
            if len(codes) == 6:
                break
        if len(codes) == 6:
            break
    summary = f"POSIX database fixture failure: phase={phase} error={kind} mysql_errors={','.join(codes) or 'unknown'}"
    try:
        print(summary, file=sys.stderr, flush=True)
    except (OSError, RuntimeError, ValueError):
        # Diagnostics must not replace the primary failure or prevent cleanup.
        pass


def posix_prepare(args: argparse.Namespace) -> Path:
    posix_only()
    installed = plain(args.installed_root.resolve(), directory=True)
    client_root = plain((args.client_root or args.installed_root).resolve(), directory=True)
    product = args.product
    if product not in ("mysql", "mariadb"):
        raise ValueError("Select mysql or mariadb explicitly on POSIX")
    server = posix_executable(installed, *(('bin/mysqld', 'sbin/mysqld') if product == 'mysql' else ('bin/mariadbd', 'sbin/mariadbd')))
    client = posix_executable(client_root, "bin/mysql", "bin/mariadb")
    admin = posix_executable(client_root, "bin/mysqladmin", "bin/mariadb-admin")
    initializer = server if product == "mysql" else posix_executable(installed, "bin/mariadb-install-db", "scripts/mariadb-install-db", "bin/mysql_install_db")
    include = args.sdk_include or client_root / "include" / "mysql"
    if not include.exists() and args.sdk_include is None:
        include = client_root / "include"
    library = args.sdk_library
    if library is None:
        family = "mysqlclient" if (client_root / "lib" / "libmysqlclient.dylib").exists() else "mariadb"
        library = client_root / "lib" / f"lib{family}.dylib"
    root = posix_temporary_root() / f"ku-{product}-live-{secrets.token_hex(16)}"
    root.mkdir(mode=0o700)
    try:
        (root / "tmp").mkdir(mode=0o700)
        (root / "empty-plugins").mkdir(mode=0o700)
        with posix_environment(root):
            output = run([str(server), "--no-defaults", "--version"], root, "check installed POSIX server version", 5)
            match = re.search(rb"\bVer ([A-Za-z0-9._-]{1,128})\b", output)
            if match is None or (b"MariaDB" in match[1]) != (product == "mariadb"):
                raise ValueError("Installed server version does not match the selected product")
            manifest = {"format": POSIX_FORMAT, "product": product, "version": match[1].decode("ascii"),
                        "server": str(server), "basedir": str(installed), "client": str(client),
                        "admin": str(admin), "initializer": str(initializer)}
            copy_posix_sdk(root, include, library)
            manifest.update(posix_sdk_identity(root, include, library))
            write_json(root / "fixture.json", manifest)
            receipt = getattr(args, "receipt_file", None)
            if receipt is not None:
                write_private(receipt.absolute(), str(root) + "\n")
            write_private(root / "fixture.txt", f"{product}\n{manifest['version']}\n{root / 'sdk'}\n")
            with operation(root):
                if product == "mysql":
                    command = [str(server), "--no-defaults", "--initialize", f"--basedir={installed}",
                               f"--datadir={root / 'data'}", "--innodb-buffer-pool-size=33554432", "--innodb-redo-log-capacity=33554432"]
                else:
                    command = [str(initializer), "--no-defaults", f"--basedir={installed}", f"--datadir={root / 'data'}",
                               "--auth-root-authentication-method=socket", "--skip-test-db", "--skip-name-resolve",
                               "--innodb-buffer-pool-size=33554432", "--innodb-log-file-size=8388608"]
                command.extend(posix_server_resources(root, installed))
                initialize_posix(root, command)
                check_posix_tree(root)
                with socket.socket() as reservation:
                    reservation.bind(("127.0.0.1", 0))
                    port = reservation.getsockname()[1]
                config = {"host": "127.0.0.1", "port": port, "user": "ku_test_" + secrets.token_hex(8),
                          "password": secrets.token_hex(32), "database": "ku_db_" + secrets.token_hex(8)}
                operator = {"user": "ku_admin_" + secrets.token_hex(8), "password": secrets.token_hex(32)}
                validate_config(config, operator)
                write_json(root / "db.json", config)
                write_json(root / "admin.json", operator)
    except BaseException as error:
        posix_failure_summary("prepare", error, root)
        # Only paths under the new exclusive root are removed. No process was
        # started by prepare after the bounded initializer returned.
        if not any(present(root / name) for name in ("server.active", "server.pid", "operation.lock")):
            shutil.rmtree(root)
        raise
    print(f"Prepared private {product} {manifest['version']} fixture (server not running): {root}", flush=True)
    print(f"Explicit client SDK: {manifest['client_library_family']} {manifest['client_version']}; header/library ABI {manifest['client_version_id']}.", flush=True)
    return root


def posix_verify(args: argparse.Namespace) -> None:
    posix_only()
    root = posix_fixture_root(args.fixture)
    manifest = posix_manifest(root)
    config, admin = read_json(root / "db.json"), read_json(root / "admin.json")
    validate_config(config, admin)
    SECRETS[:] = [config["password"], admin["password"]]
    test, ku = plain(args.test_binary), plain(args.ku_binary)
    with operation(root):
        record = root / "verification.json"
        record.unlink(missing_ok=True)
        if present(root / "server.pid") or present(root / "server.active"):
            raise ValueError("POSIX database markers already exist; refusing reuse")
        with socket.socket() as reservation:
            reservation.bind(("127.0.0.1", config["port"]))
        owner = None
        primary = None
        cleanup = BOUNDS.CleanupErrors()
        text = ""
        try:
            with posix_environment(root):
                write_posix_startup(root, config, admin, manifest["product"])
                for name in ("server.log",):
                    (root / name).unlink(missing_ok=True)
                owner = PosixMysqlProcess(root, posix_server_command(root, manifest, config["port"]))
                owner.start()
                owner.wait_ready()
                client = [manifest["client"], f"--defaults-file={root / 'admin.cnf'}", "--batch", "--raw", "--skip-column-names"]
                identity = run(client + ["--execute=SELECT VERSION(), @@datadir, @@port, @@basedir, @@plugin_dir, @@lc_messages_dir, @@character_sets_dir"], root, "verify private POSIX server identity", 5).decode("utf-8").strip().split("\t")
                expected_paths = [root / "data", Path(manifest["basedir"]), root / "empty-plugins",
                                  Path(manifest["basedir"]) / "share" / "mysql",
                                  Path(manifest["basedir"]) / "share" / "mysql" / "charsets"]
                if len(identity) != 7 or identity[0] != manifest["version"] or identity[2] != str(config["port"]):
                    raise RuntimeError("POSIX server identity does not match its newly created fixture")
                if any(Path(actual).resolve() != expected.resolve() for actual, expected in zip([identity[1], *identity[3:]], expected_paths)):
                    raise RuntimeError("POSIX server resources do not match the explicit private installation")
                os.environ.update({"KU_MYSQL_TEST_CONFIG_FILE": str(root / "db.json"), "KU_BIN": str(ku),
                                   "KU_MYSQL_LIB": str(root / "sdk" / "lib"), "KU_MYSQL_INCLUDE": str(root / "sdk" / "include")})
                output = run([str(test), "--exact", POSIX_LIVE_TESTS[sys.platform], "--ignored", "--nocapture", "--test-threads=1"], REPO, "live POSIX MySQL query acceptance", TEST_TIMEOUT)
                text = require_live_pass(output)
                if owner.failed.is_set() or owner.process.poll() is not None:
                    raise RuntimeError("POSIX server did not remain healthy during acceptance")
                run([manifest["admin"], f"--defaults-file={root / 'admin.cnf'}", "shutdown"], root, "stop private POSIX server", 15)
                owner.process.wait(timeout=15)
        except BaseException as error:
            primary = error
        finally:
            if owner is not None:
                cleanup.attempt("server_stop", owner.stop)
            for name in ("init.sql", "admin.cnf"):
                cleanup.attempt(f"{name}_unlink", lambda name=name: (root / name).unlink(missing_ok=True))
        cleanup.raise_if_any(primary)
        write_private(root / "live-test.log", text, replace=True)
        write_json(record, {"format": POSIX_FORMAT, "product": manifest["product"], "version": manifest["version"],
                            "live_test_passed": True, "server_stopped": True,
                            **{key: manifest[key] for key in ("client_library_source", "client_include_source", "client_library_family", "client_version", "client_version_id")},
                            "verified_at_utc": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime())})
    print(f"Live {manifest['product']} {manifest['version']} test passed; owned POSIX server stopped.", flush=True)
    print(text.rstrip(), flush=True)


def posix_cleanup(args: argparse.Namespace) -> None:
    posix_only()
    receipt = getattr(args, "receipt_file", None)
    if receipt is not None:
        receipt = plain(receipt.absolute())
        locator = bounded_bytes(receipt, 4096).decode("utf-8")
        if not locator.endswith("\n") or "\n" in locator[:-1]:
            raise ValueError("Invalid private fixture receipt")
        candidate = Path(locator[:-1])
        if not present(candidate):
            receipt.unlink()
            print("Private POSIX fixture already removed.", flush=True)
            return
    else:
        candidate = args.fixture
    root = posix_fixture_root(candidate)
    posix_manifest(root)
    if any(present(root / name) for name in ("operation.lock", "server.pid", "server.active")):
        raise ValueError("POSIX database shutdown is not confirmed; refusing deletion")
    with operation(root):
        check_posix_tree(root)
        # Socket leaves are validated above and belong only to this stopped
        # private root. Unlike a recursive service cleanup, no process is found
        # or killed from a path, PID marker, port or installation location.
        shutil.rmtree(root)
    if receipt is not None:
        receipt.unlink()
    print("Removed the stopped private POSIX database fixture and generated credentials.", flush=True)


def posix_exercise(args: argparse.Namespace) -> None:
    root = posix_prepare(args)
    primary = None
    cleanup = BOUNDS.CleanupErrors()
    try:
        posix_verify(argparse.Namespace(fixture=root, test_binary=args.test_binary, ku_binary=args.ku_binary))
    except BaseException as error:
        posix_failure_summary("verify", error, root)
        primary = error
    finally:
        cleanup.attempt("fixture_cleanup", lambda: posix_cleanup(argparse.Namespace(fixture=root, receipt_file=getattr(args, "receipt_file", None))))
    cleanup.raise_if_any(primary)


def main() -> None:
    if os.name == "posix":
        def interrupted(signum, frame):
            raise KeyboardInterrupt
        signal.signal(signal.SIGTERM, interrupted)
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)
    prepare_parser = commands.add_parser("prepare")
    prepare_parser.add_argument("--installed-root", type=Path, required=True)
    prepare_parser.add_argument("--product", choices=("mysql", "mariadb"))
    prepare_parser.add_argument("--client-root", type=Path)
    prepare_parser.add_argument("--sdk-include", type=Path)
    prepare_parser.add_argument("--sdk-library", type=Path)
    prepare_parser.add_argument("--receipt-file", type=Path)
    verify_parser = commands.add_parser("verify")
    verify_parser.add_argument("--fixture", type=Path, required=True)
    verify_parser.add_argument("--test-binary", type=Path, required=True)
    verify_parser.add_argument("--ku-binary", type=Path, required=True)
    exercise_parser = commands.add_parser("exercise")
    exercise_parser.add_argument("--product", choices=("mysql", "mariadb"), required=True)
    exercise_parser.add_argument("--installed-root", type=Path, required=True)
    exercise_parser.add_argument("--client-root", type=Path)
    exercise_parser.add_argument("--sdk-include", type=Path)
    exercise_parser.add_argument("--sdk-library", type=Path)
    exercise_parser.add_argument("--test-binary", type=Path, required=True)
    exercise_parser.add_argument("--ku-binary", type=Path, required=True)
    exercise_parser.add_argument("--receipt-file", type=Path)
    cleanup_parser = commands.add_parser("cleanup")
    cleanup_root = cleanup_parser.add_mutually_exclusive_group(required=True)
    cleanup_root.add_argument("--fixture", type=Path)
    cleanup_root.add_argument("--receipt-file", type=Path)
    args = parser.parse_args()
    try:
        operations = ({"prepare": prepare, "verify": verify, "cleanup": cleanup} if os.name == "nt" else
                      {"prepare": posix_prepare, "verify": posix_verify, "cleanup": posix_cleanup, "exercise": posix_exercise})
        if args.command not in operations or (os.name == "nt" and getattr(args, "receipt_file", None) is not None):
            raise ValueError("The selected fixture operation requires POSIX")
        operations[args.command](args)
    except (OSError, RuntimeError, ValueError, subprocess.TimeoutExpired, KeyboardInterrupt):
        # JSON/parser/OS exceptions can contain sensitive data. Keep diagnostics
        # categorical, never include raw command output or credential documents.
        raise SystemExit("MySQL fixture failed; no success recorded. Inspect the private fixture without publishing credentials.") from None


if __name__ == "__main__":
    main()
