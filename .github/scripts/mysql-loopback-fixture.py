#!/usr/bin/env python3
"""Opt-in, disposable Windows MySQL 8.0.29 fixture; never a system service.

prepare --installed-root PATH
verify --fixture PATH --test-binary PATH --ku-binary PATH
cleanup --fixture PATH

Only newly generated credentials and data under target/mysql-loopback-8.0.29-UUID
are used. Installation executables/DLLs/share/include are trusted inputs, copied
without configuration or component manifests. No software is downloaded. A
same-user administrator replacing files/processes concurrently is outside this
fixture's isolation boundary. This old server version is NOT a production advice.

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
import socket
import stat
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
        self.output = bytearray()
        self.lock = threading.Lock()
        self.failed = threading.Event()
        self.ready = False

    def start(self) -> None:
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
            self.reader = threading.Thread(target=self.drain, daemon=True)
            self.reader.start()
            BOUNDS.resume_suspended_windows_process(self.process)
        except BaseException:
            if self.process is None:
                # Popen failed without returning an owned child handle.
                plain(self.root / "server.active").unlink()
            else:
                self.stop()
            raise

    def drain(self) -> None:
        assert self.process is not None and self.process.stdout is not None
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
                        BOUNDS.kill_process_tree(self.process, self.job)
                        return
        except (OSError, ValueError):
            self.failed.set()
            BOUNDS.kill_process_tree(self.process, self.job)

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
        if self.process is None:
            return
        try:
            BOUNDS.kill_process_tree(self.process, self.job)
            self.process.wait(timeout=10)
        finally:
            if self.job is not None:
                self.job.close()
        if self.reader is not None:
            self.reader.join(timeout=5)
            if self.reader.is_alive():
                raise RuntimeError("Private MySQL output did not close after termination")
        if self.process.stdout is not None:
            self.process.stdout.close()
        # Process exit, not a TCP ping or pid-file disappearance, proves shutdown.
        pid = self.root / "server.pid"
        if pid.exists():
            plain(pid).unlink()
        marker = self.root / "server.active"
        if present(marker):
            plain(marker).unlink()


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
            graceful = False
            try:
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
            finally:
                server.stop()
                for name in ("init.sql", "admin.cnf"):
                    path = root / name
                    if path.exists():
                        plain(path).unlink()
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


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)
    prepare_parser = commands.add_parser("prepare")
    prepare_parser.add_argument("--installed-root", type=Path, required=True)
    verify_parser = commands.add_parser("verify")
    verify_parser.add_argument("--fixture", type=Path, required=True)
    verify_parser.add_argument("--test-binary", type=Path, required=True)
    verify_parser.add_argument("--ku-binary", type=Path, required=True)
    cleanup_parser = commands.add_parser("cleanup")
    cleanup_parser.add_argument("--fixture", type=Path, required=True)
    args = parser.parse_args()
    try:
        {"prepare": prepare, "verify": verify, "cleanup": cleanup}[args.command](args)
    except (OSError, RuntimeError, ValueError, subprocess.TimeoutExpired):
        # JSON/parser/OS exceptions can contain sensitive data. Keep diagnostics
        # categorical, never include raw command output or credential documents.
        raise SystemExit("MySQL fixture failed; no success recorded. Inspect the private fixture without publishing credentials.") from None


if __name__ == "__main__":
    main()
