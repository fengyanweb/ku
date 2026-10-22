#!/usr/bin/env python3
"""Prepare/stop only ROOT/database's private benchmark PostgreSQL 16 cluster.

Run as root on the disposable Linux guest. Every database process is attached
to its independent CPU7/512MiB cgroup before dropping to the fixture user and
exec. This never operates a system cluster, production address or remote DB.
"""
from __future__ import annotations

import argparse
import fcntl
import hashlib
import json
import os
from pathlib import Path
import platform
import secrets
import signal
import subprocess
import time

from linux_harness import Harness, Domain, write_json

FORMAT = "ku-cross-stack-private-pg-v1"
PORT = 19192
PG = Path("/usr/lib/postgresql/16/bin")


def sha(data):
    return hashlib.sha256(data).hexdigest()


class Database:
    def __init__(self, root, uid=501, gid=1000):
        if platform.system() != "Linux" or os.geteuid() != 0:
            raise RuntimeError("database helper requires root on the isolated Linux benchmark guest")
        self.root = Path(root).resolve(strict=True)
        self.directory = self.root / "database"
        if self.directory.is_symlink():
            raise RuntimeError("refuse a linked database directory")
        self.directory.mkdir(mode=0o755, exist_ok=True)
        self.marker = self.directory / "OWNER.json"
        self.state_path = self.directory / "state.json"
        self.uid, self.gid = uid, gid
        expected = dict(format=FORMAT, root=str(self.directory), uid=uid, gid=gid,
                        listen_address="127.0.0.1", port=PORT)
        if self.marker.exists():
            owner = json.loads(self.marker.read_text())
            if any(owner.get(key) != value for key, value in expected.items()):
                raise RuntimeError("private fixture ownership marker differs")
        else:
            if list(self.directory.iterdir()):
                raise RuntimeError("refuse existing unowned database directory")
            expected["owner_token"] = secrets.token_hex(16)
            write_json(self.marker, expected)
            self.marker.chmod(0o600)
        self.owner = json.loads(self.marker.read_text())
        self.data, self.socket = self.directory / "data", self.directory / "socket"
        for path in (self.data, self.socket):
            if path.is_symlink():
                raise RuntimeError("refuse linked database payload/socket directory")
            path.mkdir(mode=0o700, exist_ok=True)
            path.chmod(0o700)
            os.chown(path, uid, gid)
        self.manifest_path = self.root / "manifest.json"
        if self.manifest_path.exists():
            manifest = json.loads(self.manifest_path.read_text())
            if manifest.get("uid", uid) != uid or manifest.get("gid", gid) != gid:
                raise RuntimeError("manifest user does not match database fixture user")
        else:
            manifest = dict(uid=uid, gid=gid, stacks={}, protocol=str(self.root / "src/tests/benchmarks/cross_stack/protocol.json"),
                            cgroup_root="/sys/fs/cgroup/ku-cross-stack")
            self.save_manifest(manifest)
        self.harness = Harness(self.root, self.directory / "evidence")
        self.environment = dict(os.environ, LC_ALL="C.UTF-8", LANG="C.UTF-8", PGCLIENTENCODING="UTF8")
        self.commands = []

    def save_manifest(self, manifest):
        with (self.root / ".manifest.lock").open("a") as lock:
            os.chown(self.root / ".manifest.lock", self.uid, self.gid)
            fcntl.flock(lock, fcntl.LOCK_EX)
            latest = json.loads(self.manifest_path.read_text()) if self.manifest_path.exists() else {}
            merged = dict(manifest, **latest)
            for key in ("uid", "gid", "db_url", "db_cgroup", "database"):
                if key in manifest:
                    merged[key] = manifest[key]
            write_json(self.manifest_path, merged)
            # A later user-owned build step may update the same manifest.
            os.chown(self.manifest_path, self.uid, self.gid)
            self.manifest_path.chmod(0o600)

    def state(self):
        if not self.state_path.exists():
            return None
        value = json.loads(self.state_path.read_text())
        if value.get("owner_token") != self.owner["owner_token"] or value.get("data") != str(self.data):
            raise RuntimeError("database state does not belong to this fixture")
        return value

    def bounded(self, label, command, domain, timeout=30):
        path = self.directory / "evidence" / (label + "-" + str(time.time_ns()) + ".log")
        path.parent.mkdir(exist_ok=True)
        began = time.monotonic()
        process = self.harness.launch(list(map(str, command)), self.directory,
                                      self.environment, domain, path)
        try:
            deadline = began + timeout
            while process.poll() is None:
                if time.monotonic() >= deadline or path.stat().st_size > 1048576:
                    os.killpg(process.pid, signal.SIGKILL)
                    process.wait(timeout=3)
                    raise RuntimeError(f"database preparation command exceeded bound: {label}")
                time.sleep(.02)
            output = path.read_bytes()
            self.commands.append(dict(label=label, command=list(map(str, command)),
                                      exit_code=process.returncode, seconds=time.monotonic()-began,
                                      output_bytes=len(output), output_sha256=sha(output), log=str(path)))
            if process.returncode:
                raise RuntimeError(f"private database command failed: {label}; inspect {path}")
            return output
        finally:
            if process.poll() is None:
                os.killpg(process.pid, signal.SIGKILL)
                process.wait(timeout=3)

    def psql(self, database, domain, label, sql):
        return self.bounded(label, [PG / "psql", "-X", "-v", "ON_ERROR_STOP=1", "-h", "127.0.0.1",
                                   "-p", PORT, "-U", "bench", "-d", database, "-A", "-t", "-F", "\t",
                                   "-c", sql], domain)

    def validated_domain(self, state):
        path = Path(state["cgroup"])
        if path.parent != self.harness.cgroup_root or not path.name.startswith("database-"):
            raise RuntimeError("refuse a database state outside the owned cgroup parent")
        domain = object.__new__(Domain)
        domain.harness, domain.label, domain.path, domain.frozen = self.harness, "database", path, False
        if not path.exists():
            return domain, False
        return domain, True

    def validate_postmaster(self, state):
        pid = int(state["pid"])
        proc = Path("/proc") / str(pid)
        if not proc.exists():
            return False
        # Verify the actual executable and private data in argv before issuing
        # any stop command; a recycled PID must never authorize a signal.
        if (proc / "exe").resolve() != (PG / "postgres").resolve():
            raise RuntimeError("recorded PID belongs to another executable")
        argv = (proc / "cmdline").read_bytes().split(b"\0")
        if str(self.data).encode() not in argv:
            raise RuntimeError("recorded postmaster does not name the owned data directory")
        domain, exists = self.validated_domain(state)
        if not exists or str(pid) not in (domain.path / "cgroup.procs").read_text().split():
            raise RuntimeError("private postmaster is outside its recorded resource domain")
        return True

    def start(self):
        previous = self.state()
        if previous and previous.get("status") == "running":
            if self.validate_postmaster(previous):
                raise RuntimeError("owned database already runs; reuse manifest or explicitly stop first")
        if previous:
            old, exists = self.validated_domain(previous)
            if exists and (old.path / "cgroup.procs").read_text().strip():
                raise RuntimeError("old fixture domain still has processes; use stop before restart")
            if exists and not old.remove():
                raise RuntimeError("old empty database domain could not be removed")
        if (self.data / "postmaster.pid").exists():
            raise RuntimeError("private data has postmaster.pid without a validated running fixture; inspect state")
        domain = Domain(self.harness, "database", 1, 512*1048576, "7")
        state = dict(owner_token=self.owner["owner_token"], data=str(self.data), status="preparing",
                     cgroup=str(domain.path), pid=None, port=PORT, address="127.0.0.1", db_name="bench",
                     started_wall_ns=time.time_ns(), limits=domain.settings)
        write_json(self.state_path, state)
        process = None
        try:
            version = self.bounded("version", [PG / "postgres", "--version"], domain).decode().strip()
            if "PostgreSQL) 16." not in version:
                raise RuntimeError("requires pinned major PostgreSQL16")
            if not (self.data / "PG_VERSION").exists():
                self.bounded("initdb", [PG / "initdb", "-D", self.data, "-U", "bench",
                                        "--encoding=UTF8", "--no-locale", "-A", "trust", "--no-instructions"], domain, 90)
            if (self.data / "PG_VERSION").read_text().strip() != "16":
                raise RuntimeError("existing owned data has another PostgreSQL major")
            command = [PG / "postgres", "-D", self.data, "-h", "127.0.0.1", "-p", PORT,
                       "-k", self.socket, "-c", "shared_buffers=64MB", "-c", "max_connections=80",
                       "-c", "fsync=on", "-c", "synchronous_commit=on", "-c", "full_page_writes=on",
                       "-c", "log_statement=none", "-c", "log_min_duration_statement=-1"]
            process = self.harness.launch(list(map(str, command)), self.directory,
                                          self.environment, domain, self.directory / "postgres.log")
            state.update(pid=process.pid, command=list(map(str, command)), postgres_version=version)
            write_json(self.state_path, state)
            deadline = time.monotonic() + 15
            ready = False
            while time.monotonic() < deadline:
                if process.poll() is not None:
                    raise RuntimeError(f"owned PostgreSQL exited {process.returncode}; see postgres.log")
                try:
                    ready = self.psql("postgres", domain, "ready", "SELECT 1").strip() == b"1"
                    if ready:
                        break
                except RuntimeError:
                    pass
                time.sleep(.1)
            if not ready:
                raise RuntimeError("owned database did not become ready in15s")
            names = self.psql("postgres", domain, "database-list", "SELECT datname FROM pg_database WHERE datname='bench'").strip()
            if not names:
                self.bounded("createdb", [PG / "createdb", "-h", "127.0.0.1", "-p", PORT,
                                          "-U", "bench", "--encoding=UTF8", "bench"], domain)
            sql = ("CREATE TABLE IF NOT EXISTS cross_stack_items(id integer PRIMARY KEY,label text NOT NULL,score integer NOT NULL);\n"
                   "INSERT INTO cross_stack_items SELECT id,'item-'||id||'-中文🙂',id*17 FROM generate_series(1,1024) AS id ON CONFLICT(id) DO NOTHING;\n")
            sql_path = self.directory / "fixture.sql"
            sql_path.write_text(sql, encoding="utf-8")
            sql_path.chmod(0o600)
            os.chown(sql_path, self.uid, self.gid)
            self.bounded("fixture", [PG / "psql", "-X", "-v", "ON_ERROR_STOP=1", "-h", "127.0.0.1",
                                     "-p", PORT, "-U", "bench", "-d", "bench", "-f", sql_path], domain)
            rows = self.psql("bench", domain, "verify-rows", "SELECT id,label,score FROM cross_stack_items ORDER BY id")
            expected = "".join(f"{i}\titem-{i}-中文🙂\t{i*17}\n" for i in range(1,1025)).encode()
            if rows != expected:
                raise RuntimeError("private fixture rows differ from the frozen1024-row golden set")
            schema = self.psql("bench", domain, "verify-schema",
                               "SELECT column_name,data_type,is_nullable FROM information_schema.columns WHERE table_schema='public' AND table_name='cross_stack_items' ORDER BY ordinal_position")
            if schema != b"id\tinteger\tNO\nlabel\ttext\tNO\nscore\tinteger\tNO\n":
                raise RuntimeError("private fixture schema differs")
            key = self.psql("bench", domain, "verify-key",
                            "SELECT pg_get_constraintdef(oid) FROM pg_constraint WHERE conrelid='cross_stack_items'::regclass AND contype='p'").strip()
            if key != b"PRIMARY KEY (id)":
                raise RuntimeError("fixture primary-key constraint differs")
            state.update(status="running", ready_wall_ns=time.time_ns(), rows=1024,
                         rows_sha256=sha(rows), schema_sha256=sha(schema), fixture_sql_sha256=sha(sql.encode()),
                         commands=self.commands, resources=domain.snapshot(),
                         semantics="private loopback fixture; normal durable commit; no production cluster; no statement replay")
            self.validate_postmaster(state)
            write_json(self.state_path, state)
            manifest = json.loads(self.manifest_path.read_text())
            manifest.update(db_url=f"postgresql://bench@127.0.0.1:{PORT}/bench?sslmode=disable", db_cgroup=str(domain.path),
                            database=dict(status="running", state=str(self.state_path), postgres_version=version,
                                          rows=1024, rows_sha256=sha(rows), fixture_sql_sha256=sha(sql.encode()),
                                          cpu_set="7", memory_bytes=512*1048576, shared_buffers="64MB", max_connections=80))
            self.save_manifest(manifest)
            return dict(status="running", pid=process.pid, cgroup=str(domain.path), state=str(self.state_path), rows_sha256=sha(rows))
        except BaseException as error:
            domain.kill()
            if process is not None:
                process.wait(timeout=5)
            after = domain.snapshot()
            removed = domain.remove()
            state.update(status="failed", error=repr(error)[:512], commands=self.commands,
                         cleanup=dict(after=after, cgroup_removed=removed))
            write_json(self.state_path, state)
            raise

    def stop(self):
        state = self.state()
        if not state:
            return dict(status="not_started", note="no owned fixture state, no action performed")
        domain, exists = self.validated_domain(state)
        running = self.validate_postmaster(state) if state.get("pid") else False
        forced, stop_error = False, None
        if running:
            try:
                self.bounded("stop-fast", [PG / "pg_ctl", "-D", self.data, "-m", "fast", "-w", "-t", "10", "stop"], domain, 12)
            except Exception as error:
                forced, stop_error = True, repr(error)[:256]
        if exists:
            if (domain.path / "cgroup.procs").read_text().strip():
                domain.kill()
                forced = True
            deadline = time.monotonic() + 3
            while (domain.path / "cgroup.procs").read_text().strip() and time.monotonic() < deadline:
                time.sleep(.02)
            after = domain.snapshot()
            removed = domain.remove()
        else:
            after, removed = dict(process_count=0, note="recorded owned domain already absent"), True
        cleanup = dict(forced=forced, stop_error=stop_error, after=after, cgroup_removed=removed,
                       private_data_preserved=True, stopped_wall_ns=time.time_ns(), commands=self.commands)
        state.update(status="stopped" if removed and after.get("process_count") == 0 else "cleanup_failed", cleanup=cleanup)
        write_json(self.state_path, state)
        manifest = json.loads(self.manifest_path.read_text())
        manifest["db_cgroup"] = None
        manifest["database"] = dict(manifest.get("database", {}), status=state["status"], cleanup=cleanup)
        self.save_manifest(manifest)
        if state["status"] != "stopped":
            raise RuntimeError("private PostgreSQL cleanup incomplete; see owned state")
        return cleanup


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", type=Path, default=Path("/opt/ku-bench"))
    parser.add_argument("--uid", type=int, default=501)
    parser.add_argument("--gid", type=int, default=1000)
    parser.add_argument("action", choices=("start", "stop", "status"))
    args = parser.parse_args()
    db = Database(args.root, args.uid, args.gid)
    result = db.start() if args.action == "start" else db.stop() if args.action == "stop" else db.state()
    print(json.dumps(result, ensure_ascii=False))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
