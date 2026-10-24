#!/usr/bin/env python3
"""Verify PG lifecycles using one current-run installed Ku release archive.

This is an explicit CI acceptance entrypoint, not part of ordinary packaging.
It uses only trusted checkout helpers and the existing private-PG fixture.
"""
from __future__ import annotations

import argparse
from contextlib import contextmanager
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import platform
import re
import shutil
import signal
import socket
import subprocess
import stat
import struct
import sys
import tempfile
import time
import urllib.error
import urllib.parse
import urllib.request
import zipfile

sys.dont_write_bytecode = True
MAX_ZIP = 1024 * 1024 * 1024
MAX_JSON = 2 * 1024 * 1024
# Explicit supported storage domains, not a claim about all future GitHub hosts.
ARTIFACT_STORAGE_SUFFIXES = (".blob.core.windows.net", ".actions.githubusercontent.com")
TARGET_HOSTS = {
    "x86_64-unknown-linux-gnu": ("Linux", {"x86_64", "amd64"}),
    "x86_64-pc-windows-msvc": ("Windows", {"x86_64", "amd64"}),
    "aarch64-apple-darwin": ("Darwin", {"arm64", "aarch64"}),
}


def load_trusted(path, name):
    spec = importlib.util.spec_from_file_location(name, path)
    if spec is None or spec.loader is None:
        raise ValueError("trusted checkout helper is unavailable")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


class NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, req, fp, code, msg, headers, newurl):
        return None


def github_json(endpoint, token):
    # Metadata is obtained only from the fixed GitHub API. Signed artifact
    # download URLs and the token are never included in saved evidence.
    request = urllib.request.Request("https://api.github.com" + endpoint, headers={
        "Authorization": "Bearer " + token,
        "Accept": "application/vnd.github+json",
        "X-GitHub-Api-Version": "2022-11-28",
    })
    # Never forward the API credential through a metadata redirect.
    try:
        response = urllib.request.build_opener(NoRedirect()).open(request, timeout=15)
    except urllib.error.HTTPError as error:
        error.close()
        raise ValueError("GitHub metadata request failed or redirected") from None
    with response:
        body = response.read(MAX_JSON + 1)
    if len(body) > MAX_JSON:
        raise ValueError("GitHub metadata exceeds its bound")
    return json.loads(body)


def download_zip(endpoint, token, destination, expected_size, expected_digest):
    request = urllib.request.Request("https://api.github.com" + endpoint, headers={
        "Authorization": "Bearer " + token,
        "Accept": "application/vnd.github+json",
        "X-GitHub-Api-Version": "2022-11-28",
    })
    opener = urllib.request.build_opener(NoRedirect())
    try:
        response = opener.open(request, timeout=15)
    except urllib.error.HTTPError as error:
        if error.code != 302:
            error.close()
            raise ValueError("GitHub artifact download request failed") from None
        location = error.headers.get("Location", "")
        error.close()
        parsed = urllib.parse.urlsplit(location)
        host = (parsed.hostname or "").lower()
        if (parsed.scheme != "https" or not host or parsed.username or parsed.password
                or parsed.port not in (None, 443)
                or not any(host.endswith(suffix) and len(host) > len(suffix)
                           for suffix in ARTIFACT_STORAGE_SUFFIXES)):
            raise ValueError("GitHub artifact download redirect is outside supported storage hosts")
        # Fresh request omits Authorization. Refuse another redirect, so the
        # explicit storage-host check cannot be bypassed by an intermediate hop.
        try:
            response = opener.open(urllib.request.Request(location), timeout=15)
        except urllib.error.HTTPError as error:
            error.close()
            raise ValueError("artifact storage request failed or redirected") from None
    digest = hashlib.sha256()
    size = 0
    deadline = time.monotonic() + 120
    with response, destination.open("xb") as output:
        while chunk := response.read(1024 * 1024):
            size += len(chunk)
            if size > MAX_ZIP or size > expected_size or time.monotonic() >= deadline:
                raise ValueError("artifact download exceeded its size/time bound")
            output.write(chunk)
            digest.update(chunk)
    if size != expected_size or digest.hexdigest() != expected_digest:
        raise ValueError("GitHub artifact ZIP size/digest mismatch")


def current_run_artifact(repository, run_id, commit, target, token):
    run = github_json(f"/repos/{repository}/actions/runs/{run_id}", token)
    if (run.get("id") != run_id or run.get("head_sha") != commit
            or run.get("repository", {}).get("full_name", "").casefold() != repository.casefold()
            or run.get("event") != "push"):
        raise ValueError("GitHub run identity differs from this push")
    listing = github_json(f"/repos/{repository}/actions/runs/{run_id}/artifacts?per_page=100", token)
    rows = listing.get("artifacts", [])
    if type(listing.get("total_count")) is not int or listing["total_count"] != len(rows):
        raise ValueError("artifact list is incomplete or exceeds one bounded page")
    selected = [row for row in rows if row.get("name") == "ku-release-" + target]
    if len(selected) != 1:
        raise ValueError("expected exactly one current-run target artifact")
    artifact = selected[0]
    digest = artifact.get("digest", "")
    workflow = artifact.get("workflow_run", {})
    if (artifact.get("expired") is not False or type(artifact.get("id")) is not int
            or artifact["id"] <= 0 or type(artifact.get("size_in_bytes")) is not int
            or not 0 < artifact["size_in_bytes"] <= MAX_ZIP
            or not re.fullmatch(r"sha256:[0-9a-f]{64}", digest)
            or workflow.get("id") != run_id or workflow.get("head_sha") != commit):
        raise ValueError("artifact digest, size or run identity is invalid")
    return artifact


def extract_artifact_zip(source, destination, expected_names):
    with zipfile.ZipFile(source) as archive:
        members = archive.infolist()
        if len(members) != 2 or {item.filename for item in members} != expected_names:
            raise ValueError("artifact ZIP must contain exactly its archive and manifest")
        total = 0
        for member in members:
            mode = member.external_attr >> 16
            if member.is_dir() or stat.S_ISLNK(mode) or member.flag_bits & 1:
                raise ValueError("artifact ZIP contains a non-plain/encrypted member")
            total += member.file_size
            if member.file_size <= 0 or total > MAX_ZIP:
                raise ValueError("artifact ZIP expanded size exceeds its bound")
            path = destination / member.filename
            limit = MAX_JSON if path.name.endswith(".manifest.json") else MAX_ZIP
            if member.file_size > limit:
                raise ValueError("artifact member exceeds its bound")
            with archive.open(member) as incoming, path.open("xb") as output:
                shutil.copyfileobj(incoming, output, 1024 * 1024)
            if path.stat().st_size != member.file_size:
                raise ValueError("artifact member extraction is incomplete")


def check_executable_architecture(executable, target):
    with executable.open("rb") as incoming:
        header = incoming.read(64)
        if target == "x86_64-unknown-linux-gnu":
            valid = (len(header) == 64 and header[:6] == b"\x7fELF\x02\x01"
                     and struct.unpack_from("<H", header, 18)[0] == 62)
        elif target == "aarch64-apple-darwin":
            valid = (len(header) == 64 and header[:4] == b"\xcf\xfa\xed\xfe"
                     and struct.unpack_from("<I", header, 4)[0] == 0x0100000c)
        else:
            valid = header[:2] == b"MZ" and len(header) == 64
            if valid:
                offset = struct.unpack_from("<I", header, 60)[0]
                valid = 64 <= offset <= 1024 * 1024
                if valid:
                    incoming.seek(offset)
                    pe = incoming.read(26)
                    valid = (pe[:4] == b"PE\x00\x00" and len(pe) == 26
                             and struct.unpack_from("<H", pe, 4)[0] == 0x8664
                             and struct.unpack_from("<H", pe, 24)[0] == 0x20b)
    if not valid:
        raise ValueError("installed executable binary architecture differs from target")


def private_pid_evidence(root):
    """Bounded evidence only; a missing pid file never proves process exit."""
    result = []
    work = root / "fixture-work"
    if not work.is_dir():
        return result
    for index, entry in enumerate(work.iterdir()):
        if index >= 8:
            result.append({"entries_truncated": True})
            break
        if not entry.name.startswith("ku-http-pg-acceptance-") or entry.is_symlink():
            continue
        pid_file = entry / "data" / "postmaster.pid"
        if not pid_file.is_file() or pid_file.is_symlink():
            continue
        with pid_file.open("rb") as stream:
            raw = stream.read(4097)
        lines = raw[:4096].decode("utf-8", errors="replace").splitlines()
        result.append({"path": str(pid_file), "sha256_prefix_4096": hashlib.sha256(raw[:4096]).hexdigest(),
                       "truncated": len(raw) > 4096,
                       "pid": int(lines[0]) if lines and lines[0].isdigit() else None,
                       "start_epoch": int(lines[2]) if len(lines) > 2 and lines[2].isdigit() else None,
                       "port": int(lines[3]) if len(lines) > 3 and lines[3].isdigit() else None})
    return result


@contextmanager
def owned_temporary_root(report):
    root = Path(tempfile.mkdtemp(prefix="ku-installed-pg-")).resolve()
    report["temporary_cleanup_confirmed"] = False
    try:
        yield root
    finally:
        # Preserve owned roots after any unconfirmed driver/database cleanup;
        # filesystem deletion is not a process-reclamation receipt. POSIX
        # foreground PostgreSQL is retained by the explicit owner below.
        pg_started = report.get("pg_fixture_invoked", False)
        fixture_confirmed = report.get("fixture_cleanup_confirmed") is True
        if pg_started and not fixture_confirmed:
            report["cleanup_unconfirmed"] = True
            report["retained_temporary_root"] = str(root)
            try:
                report["private_database_pid_evidence"] = private_pid_evidence(root)
            except OSError as error:
                report["pid_evidence_error_kind"] = type(error).__name__
            report["status"] = "failed"
        else:
            try:
                shutil.rmtree(root)
                report["temporary_cleanup_confirmed"] = True
                report["cleanup_unconfirmed"] = False
            except BaseException:
                report["cleanup_unconfirmed"] = True
                report["retained_temporary_root"] = str(root)
                report["status"] = "failed"
                raise


# Keep a strong reference until all PID/PGID operations finish and the child is
# reaped. No poll/wait/communicate occurs before that point. An exited child
# remains a zombie, so its PID cannot be reused while pg_ctl may still signal it.
HELD_POSTGRES_OWNERS = []


class PosixPrivatePostgres:
    def __init__(self, root, tools, lane, run_tool):
        self.root, self.tools, self.lane = root, tools, lane
        self.owner_pid = os.getpid()
        self.run_tool, self.process, self.log = run_tool, None, None
        self.signal_closed = False
        self.receipt = {"lane": lane, "cleanup_confirmed": False,
                        "pid_reuse_guard": "direct child held unreaped until the final signal boundary",
                        "database_retry": False, "snapshots": []}

    def default_reaper(self):
        if signal.getsignal(signal.SIGCHLD) != signal.SIG_DFL:
            raise ValueError("private postgres owner requires default SIGCHLD and no external child reaper")

    def start(self, execution_deadline=None):
        self.default_reaper()
        if execution_deadline is None:
            execution_deadline = time.monotonic() + 180
        def remaining():
            value = execution_deadline - time.monotonic()
            if value <= 0:
                raise TimeoutError("private PG shared execution deadline expired before launch")
            return value
        remaining()
        self.root.mkdir(mode=0o700)
        self.root_identity = (self.root.stat().st_dev, self.root.stat().st_ino)
        self.data = self.root / "data"
        self.run_tool([str(self.tools / "initdb"), "-D", str(self.data), "-U", "ku_accept",
                       "--auth-local=trust", "--auth-host=trust", "--encoding=UTF8", "--locale=C", "--no-instructions"], min(30, remaining()))
        self.data_identity = (self.data.stat().st_dev, self.data.stat().st_ino)
        self.tool_identities = {}
        for name in ("postgres", "pg_ctl"):
            path = self.tools / name
            info = path.stat()
            self.tool_identities[name] = (info.st_dev, info.st_ino, info.st_size, info.st_mtime_ns)
            with path.open("rb") as incoming:
                self.receipt[name + "_sha256"] = hashlib.file_digest(incoming, "sha256").hexdigest()
        with socket.socket() as listener:
            listener.bind(("127.0.0.1", 0))
            self.port = listener.getsockname()[1]
        remaining()
        self.log = (self.root / "postgres.log").open("xb")
        self.process = subprocess.Popen([str(self.tools / "postgres"), "-D", str(self.data),
            "-h", "127.0.0.1", "-p", str(self.port), "-k", "",
            "-c", "max_connections=24", "-c", "shared_buffers=16MB"],
            stdin=subprocess.DEVNULL, stdout=self.log, stderr=self.log,
            start_new_session=True, cwd=self.root)
        HELD_POSTGRES_OWNERS.append(self)
        self.receipt.update(pid=self.process.pid, parent_pid=os.getpid(), root=str(self.root), data=str(self.data))
        self.anchor()
        deadline = min(time.monotonic() + 20, execution_deadline)
        for _ in range(401):
            if time.monotonic() >= deadline:
                break
            if self.pid_record(ready=True):
                info = self.root.stat()
                spec = {"format": "ku-installed-private-pg-owner-v1", "lane": self.lane,
                        "root": str(self.root), "data": str(self.data), "tools": str(self.tools),
                        "port": self.port, "pid": self.process.pid, "owner_pid": os.getpid(),
                        "root_device": info.st_dev, "root_inode": info.st_ino}
                path = self.root / "owner.json"
                path.write_text(json.dumps(spec) + "\n")
                self.receipt["ready"] = True
                return path
            time.sleep(min(0.05, max(0, deadline - time.monotonic())))
        raise TimeoutError("direct private postgres did not become ready within 20 seconds")

    def anchor(self, allow_unqueryable=False):
        self.default_reaper()
        if os.getpid() != self.owner_pid:
            raise ValueError("private postgres owner must remain in its original parent process")
        if self.signal_closed or self.process is None or self.process.returncode is not None:
            raise ValueError("private postgres signal anchor is unavailable or was reaped")
        for path, identity in ((self.root, self.root_identity), (self.data, self.data_identity)):
            info = path.stat()
            if path.is_symlink() or (info.st_dev, info.st_ino) != identity:
                raise ValueError("private postgres root/data identity changed")
        for name, expected in self.tool_identities.items():
            path = self.tools / name
            info = path.stat()
            if path.is_symlink() or (info.st_dev, info.st_ino, info.st_size, info.st_mtime_ns) != expected:
                raise ValueError("private postgres executable identity changed")
        # Darwin returns ESRCH for an unreaped zombie even while ps still
        # exposes its original parent/group. That exception alone never proves
        # ownership or authorizes a signal/reap; snapshot must prove the held
        # direct child and its zombie state independently.
        try:
            group, session = os.getpgid(self.process.pid), os.getsid(self.process.pid)
        except ProcessLookupError:
            if allow_unqueryable:
                return False
            raise
        if group != self.process.pid or session != self.process.pid:
            raise ValueError("private postgres moved outside its owned session/group")
        return True

    def pid_record(self, ready=False):
        path = self.data / "postmaster.pid"
        if not path.exists():
            return False
        if path.is_symlink() or self.data.is_symlink():
            raise ValueError("private postgres pid/data path changed")
        with path.open("rb") as incoming:
            raw = incoming.read(8193)
        if len(raw) > 8192:
            raise ValueError("private postgres pid record exceeds bound")
        fields = raw.decode("utf-8").splitlines()
        if (len(fields) < 4 or fields[0] != str(self.process.pid)
                or fields[1] != str(self.data) or fields[3] != str(self.port)):
            raise ValueError("private postgres pid record differs from retained child")
        return not ready or (len(fields) >= 8 and fields[7].strip() == "ready")

    def snapshot(self, timeout):
        self.anchor(allow_unqueryable=True)
        result = self.run_tool(["ps", "-axo", "pid=,ppid=,pgid=,uid=,state="], timeout)
        raw = result.stdout
        if len(raw) > 65536:
            raise ValueError("private postgres process snapshot exceeds bound")
        rows = raw.decode("ascii").splitlines()
        if len(rows) > 4096:
            raise ValueError("private postgres process snapshot count exceeds bound")
        parsed = []
        for row in rows:
            fields = row.split()
            if len(fields) != 5:
                raise ValueError("private postgres process snapshot is malformed")
            pid, parent, group, uid = map(int, fields[:4])
            if uid == os.getuid():
                parsed.append((pid, parent, group, fields[4]))
        roots = [row for row in parsed if row[0] == self.process.pid]
        if (len(roots) != 1 or roots[0][1] != self.owner_pid
                or roots[0][2] != self.process.pid):
            raise ValueError("private postgres is no longer the held direct child/group")
        root_zombie = roots[0][3].startswith("Z")
        root_live = not root_zombie and self.anchor(allow_unqueryable=True)
        uncertain = not root_zombie and not root_live
        members = []
        for pid, parent, group, state in parsed:
            zombie = state.startswith("Z")
            if pid == self.process.pid and zombie:
                # The retained Popen/parent/group fixes the PID until final
                # wait. Do not claim a successful getsid on Darwin zombies.
                session = None
            else:
                try:
                    session = os.getsid(pid)
                except ProcessLookupError:
                    if group != self.process.pid:
                        continue
                    session = None
                    uncertain = uncertain or not zombie
            if group == self.process.pid or session == self.process.pid:
                if session is not None and group == self.process.pid and session != self.process.pid:
                    raise ValueError("private postgres group member has another session")
                members.append({"pid": pid, "parent": parent, "group": group,
                                "session": session, "state": state})
                if len(members) > 64:
                    raise ValueError("private postgres owned process count exceeds bound")
        self.receipt["last_root_state"] = roots[0][3]
        self.receipt["owned_signal_members_confirmed"] = (root_live and not uncertain
            and all(row["group"] == self.process.pid and not row["state"].startswith("Z") for row in members))
        if len(self.receipt["snapshots"]) < 8:
            self.receipt["snapshots"].append(members)
        return len(members) == 1 and root_zombie

    def cleanup(self):
        if self.process is None:
            self.receipt["cleanup_confirmed"] = True
            if self.log is not None:
                self.log.close()
            return
        # One shutdown budget, independent of unchanged HTTP/query/recovery
        # deadlines. The existing tool owner also has its own bounded cleanup.
        deadline = time.monotonic() + 30
        def remaining():
            value = deadline - time.monotonic()
            if value <= 0:
                raise TimeoutError("private postgres cleanup budget expired")
            return value
        try:
            # Independently observe state before invoking a signaling tool:
            # an already-dead root needs only the final ownership-checked reap.
            empty = self.snapshot(min(2, remaining()))
            has_record = self.pid_record()
            if not empty and self.receipt["owned_signal_members_confirmed"] and has_record:
                try:
                    self.run_tool([str(self.tools / "pg_ctl"), "-D", str(self.data),
                                   "-w", "-t", "15", "stop", "-m", "immediate"], min(15, remaining()))
                    self.receipt["pg_ctl_stop"] = "passed"
                except BaseException as error:
                    self.receipt["pg_ctl_stop"] = type(error).__name__
            # Do not reap the direct child even after pg_ctl says success.
            # Verify its complete session/group independently while PID stays
            # anchored; the only permitted remaining member is that zombie.
            for index in range(80):
                if empty or self.snapshot(min(2, remaining())):
                    self.signal_closed = True
                    code = self.process.wait(timeout=remaining())
                    self.receipt.update(cleanup_confirmed=True, postmaster_exit_code=code,
                                        group_members_before_reap=0, retained_root_zombie=True)
                    self.log.close()
                    HELD_POSTGRES_OWNERS.remove(self)
                    return
                if (index in (0, 20) and self.receipt["owned_signal_members_confirmed"]
                        and self.anchor(allow_unqueryable=True)):
                    signal_number = signal.SIGQUIT if index == 0 else signal.SIGKILL
                    os.killpg(self.process.pid, signal_number)
                    self.receipt.setdefault("owned_group_signals", []).append(int(signal_number))
                time.sleep(min(0.05, remaining()))
            raise TimeoutError("private postgres session/group did not empty before final reap")
        except BaseException as error:
            self.receipt.update(cleanup_confirmed=False, failure_kind=type(error).__name__)
            # Do not close the signal anchor or erase the private data directory.
            raise


def run_pg_lifecycles(exporter, prepare, repo, installed, root, lifecycle, report):
    driver = repo / "tests" / "acceptance" / "http_pg_lifecycle.py"
    executable = installed / ("ku.exe" if os.name == "nt" else "ku")
    working = root / "fixture-work"
    working.mkdir()
    command = [sys.executable, "-B", str(driver), "--ku", str(executable),
               "--loops", "4", "--profile", "debug"]
    report["pg_fixture_invoked"] = True
    if os.name == "nt":
        try:
            process = exporter.run_runtime_check([*command, "--lane", "both", "--output", str(lifecycle)], repo, working)
        except BaseException as error:
            cleanup = getattr(error, "process_owner_cleanup", {})
            report["runtime_process_owner_cleanup"] = cleanup
            # Only the original suspended Job's successful drain can prove
            # cleanup of a Windows pg_ctl child after a failed driver.
            report["fixture_cleanup_confirmed"] = (cleanup.get("process_tree_cleanup_confirmed") is True
                                                    and cleanup.get("windows_job_drained") is True)
            raise
        report["runtime_process_owner_cleanup"] = getattr(process, "process_owner_cleanup", {})
        return process
    execution_deadline = time.monotonic() + 180
    report["fixture_execution_budget_seconds"] = 180
    report["fixture_execution_budget_scope"] = "one absolute budget across tools/startup and both independent lanes; mandatory reclamation has a separate bounded cleanup phase"
    def execution_remaining():
        value = execution_deadline - time.monotonic()
        if value <= 0:
            raise TimeoutError("installed PostgreSQL shared execution deadline expired")
        return value
    runner = load_trusted(repo / ".github" / "scripts" / "verify-native-three-os.py", "ku_installed_pg_tool_owner")
    runner.MAX_CAPTURE_BYTES = 65536
    def tool(arguments, timeout):
        runner.COMMAND_TIMEOUT_SECONDS = timeout
        runner.PROCESS_CLEANUP_TIMEOUT_SECONDS = min(10, timeout)
        return runner.run_bounded(arguments, repo, "private installed postgres owner tool")
    config = shutil.which("pg_config")
    if not config:
        raise ValueError("private postgres pg_config is unavailable")
    tools = Path(tool([config, "--bindir"], min(15, execution_remaining())).stdout.decode().strip()).resolve()
    for name in ("initdb", "postgres", "pg_ctl"):
        exporter.plain(tools / name)
    combined = {"status": "running", "required_lanes": ["sync", "async"], "lanes": {}}
    report["private_database_owners"] = {}
    process, first_failure = None, None
    for lane in ("sync", "async"):
        owner = PosixPrivatePostgres(root / ("private-database-" + lane), tools, lane, tool)
        report["private_database_owners"][lane] = owner.receipt
        parent_cleanup, error = False, None
        lane_output = lifecycle.with_name(lifecycle.stem + "." + lane + lifecycle.suffix)
        save_report(lane_output, {"status": "running", "lanes": {}})
        try:
            owner_config = owner.start(execution_deadline)
            try:
                process = exporter.run_runtime_check([*command, "--lane", lane,
                    "--database-owner-config", str(owner_config), "--output", str(lane_output)], repo, working,
                    timeout_seconds=min(180, execution_remaining()))
            except BaseException as failure:
                parent_cleanup = getattr(failure, "process_owner_cleanup", {}).get("process_tree_cleanup_confirmed") is True
                raise
            parent_cleanup = getattr(process, "process_owner_cleanup", {}).get("process_tree_cleanup_confirmed") is True
            if process.returncode != 0:
                raise ValueError("private database fixture process did not succeed")
            data = prepare.json_object(prepare.read_file(lane_output, prepare.MAX_JSON))
            if data.get("required_lanes") != [lane] or set(data.get("lanes", {})) != {lane}:
                raise ValueError("private database fixture returned another lane")
            raw = data["lanes"][lane].get("report", {})
            if (raw.get("database_process_owner") != "outer installed-archive verifier"
                    or raw.get("database_owner_pid") != owner.process.pid):
                raise ValueError("fixture did not use its assigned parent-owned database")
            combined["lanes"][lane] = data["lanes"][lane]
        except BaseException as failure:
            error = failure
            owner.receipt["fixture_or_start_failure_kind"] = type(failure).__name__
        finally:
            try:
                owner.cleanup()
            except BaseException as failure:
                error = error or failure
            if error is None and owner.receipt.get("cleanup_confirmed") is True and owner.receipt.get("postmaster_exit_code") != 0:
                error = ValueError("private postgres exited abnormally after an otherwise successful lane")
            owner.receipt["fixture_process_owner_cleanup_confirmed"] = parent_cleanup
            report["fixture_cleanup_confirmed"] = (parent_cleanup and all(
                row.get("cleanup_confirmed") is True for row in report["private_database_owners"].values()))
            if error is not None:
                # Preserve the last bounded independent lane checkpoint even
                # after a forced outer timeout; never replace it with passed.
                try:
                    failed_data = prepare.json_object(prepare.read_file(lane_output, prepare.MAX_JSON))
                    combined["lanes"][lane] = failed_data.get("lanes", {}).get(lane, {})
                except (ValueError, OSError, KeyError, TypeError):
                    combined["lanes"][lane] = {}
                combined["lanes"][lane].update(status="failed", owner_failure_kind=type(error).__name__)
            if owner.receipt.get("fixture_or_start_failure_kind") or not owner.receipt.get("cleanup_confirmed"):
                log_path = owner.root / "postgres.log"
                if log_path.is_file() and not log_path.is_symlink():
                    with log_path.open("rb") as incoming:
                        incoming.seek(max(0, log_path.stat().st_size - 8192))
                        owner.receipt["postgres_log_tail"] = incoming.read(8192).decode("utf-8", errors="replace")
            combined["status"] = "passed" if all(combined["lanes"].get(name, {}).get("status") == "passed" for name in ("sync", "async")) else "failed"
            save_report(lifecycle, combined)
        if not report["fixture_cleanup_confirmed"] or isinstance(error, KeyboardInterrupt):
            raise error or ValueError("private fixture or postgres owner cleanup is unconfirmed")
        # Match the existing lane policy: an independently failed lane does not
        # prevent the other lane once all of its actual owners are reclaimed.
        if error is not None:
            first_failure = first_failure or error
    if first_failure is not None:
        raise first_failure
    return process


def save_report(output, report):
    output.parent.mkdir(parents=True, exist_ok=True)
    output.write_text(json.dumps(report, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")


def verify(args):
    repo = args.repo.resolve()
    helpers = repo / ".github" / "scripts"
    prepare = load_trusted(helpers / "prepare-release.py", "ku_installed_pg_prepare")
    exporter = prepare.EXPORTER
    repository = prepare.repo_version(repo, args.version)
    if not prepare.COMMIT.fullmatch(args.commit):
        raise ValueError("expected full lowercase source commit")
    system, machines = TARGET_HOSTS[args.target]
    if platform.system() != system or platform.machine().lower() not in machines:
        raise ValueError("selected target does not match this physical runner")
    output = args.output.resolve()
    report = {"format": "ku-installed-pg-check-v1", "status": "running",
              "version": args.version, "target": args.target, "commit": args.commit,
              "database_retry": False, "loops": 4, "profile": "debug",
              "scope": "actual installed CLI -> native sync/async PG lifecycle; not a capacity result",
              "temporary_files": "owned root; independently confirmed runtime and PG cleanup removes it even after a failed check; unconfirmed cleanup retains pid evidence"}
    save_report(output, report)
    try:
        with owned_temporary_root(report) as root:
            archive_name = f"ku-v{args.version}-{args.target}.tar.gz"
            manifest_name = archive_name + ".manifest.json"
            assets = root / "assets"
            assets.mkdir()
            if args.assets is not None:
                # Offline/local follow-up uses already independently downloaded
                # archive+manifest. It makes no outer-ZIP/API identity claim.
                source = args.assets.resolve()
                for name in (archive_name, manifest_name):
                    exporter.plain(source / name)
                    shutil.copyfile(source / name, assets / name)
                report["download_policy"] = "local inputs; verify original ZIP/API receipt separately"
            else:
                if (os.environ.get("GITHUB_REPOSITORY", "").casefold() != repository.casefold()
                        or os.environ.get("GITHUB_SHA") != args.commit):
                    raise ValueError("workflow repository/commit environment differs")
                run_text = os.environ.get("GITHUB_RUN_ID", "")
                if not re.fullmatch(r"[1-9][0-9]{0,19}", run_text):
                    raise ValueError("invalid current GitHub run ID")
                token = os.environ.get("GH_TOKEN", "")
                if not token:
                    raise ValueError("read-only current-run artifact token is unavailable")
                artifact = current_run_artifact(repository, int(run_text), args.commit, args.target, token)
                report["artifact"] = {key: artifact[key] for key in
                                      ("id", "name", "size_in_bytes", "digest", "expired")}
                report["run_id"] = int(run_text)
                save_report(output, report)
                zipped = root / "artifact.zip"
                download_zip(f"/repos/{repository}/actions/artifacts/{artifact['id']}/zip", token,
                             zipped, artifact["size_in_bytes"], artifact["digest"][7:])
                extract_artifact_zip(zipped, assets, {archive_name, manifest_name})
                report["download_policy"] = "current push run; exact target artifact; verified outer ZIP digest"
            manifest_bytes = prepare.read_file(assets / manifest_name, prepare.MAX_JSON)
            manifest = prepare.json_object(manifest_bytes)
            prepare.validate_manifest(manifest, args.version, args.target, args.commit, archive_name)
            report["manifest_sha256"] = hashlib.sha256(manifest_bytes).hexdigest()
            report["archive_sha256"] = manifest["sha256"]
            report["archive_size"] = manifest["size"]
            snapshot = root / "snapshot.tar.gz"
            prepare.copy_verified(assets / archive_name, snapshot, manifest,
                                  prepare.identity(exporter.plain(assets / archive_name)))
            installed = root / "installed"
            installed.mkdir()
            exporter.verify_archive(snapshot, manifest["files"], installed)
            release = prepare.json_object(prepare.read_file(installed / "RELEASE.json", prepare.MAX_JSON))
            if any(release.get(key) != expected for key, expected in
                   (("version", args.version), ("target", args.target), ("commit", args.commit))):
                raise ValueError("installed RELEASE.json identity differs")
            executable = installed / ("ku.exe" if os.name == "nt" else "ku")
            check_executable_architecture(executable, args.target)
            expected_cli = manifest["files"][executable.name]["sha256"]
            report["executable_sha256"] = expected_cli
            report["binary_architecture_check"] = "passed"
            # Do not propagate the artifact API token into the compiler, HTTP
            # service or disposable database process environment.
            os.environ.pop("GH_TOKEN", None)
            os.environ.pop("GITHUB_TOKEN", None)
            report["installation_check"] = exporter.verify_installation(
                installed, repo, args.version, args.target)
            lifecycle = output.with_name(output.stem + ".lifecycle.json")
            # A zero-exit miswired driver must never reuse an earlier receipt.
            save_report(lifecycle, {"status": "running", "required_lanes": ["sync", "async"],
                                    "lanes": {}})
            save_report(output, report)
            process = run_pg_lifecycles(exporter, prepare, repo, installed, root, lifecycle, report)
            if process.returncode != 0:
                raise ValueError("installed PG lifecycle process did not succeed")
            data = prepare.json_object(prepare.read_file(lifecycle, prepare.MAX_JSON))
            if (data.get("status") != "passed" or data.get("required_lanes") != ["sync", "async"]
                    or set(data.get("lanes", {})) != {"sync", "async"}):
                raise ValueError("installed PG lifecycle lanes did not both pass")
            for lane in ("sync", "async"):
                entry = data["lanes"][lane]
                raw = entry.get("report", {})
                fixture_name = "http_pg_lifecycle" + ("_async" if lane == "async" else "") + ".ku"
                fixture_sha = exporter.file_hash(repo / "tests" / "fixtures" / fixture_name)
                if (entry.get("status") != "passed" or raw.get("status") != "passed"
                        or raw.get("cleanup_errors") != [] or raw.get("cli_sha256") != expected_cli
                        or raw.get("event_loops") != 4 or raw.get("database_retry") is not False
                        or raw.get("parameters", {}).get("native_c_profile") != "debug"
                        or raw.get("lane") != lane or raw.get("fixture") != fixture_name
                        or raw.get("fixture_sha256") != fixture_sha):
                    raise ValueError("PG lifecycle result, CLI identity or cleanup differs")
                report.setdefault("lanes", {})[lane] = {
                    "status": "passed", "cli_sha256": raw["cli_sha256"],
                    "fixture_sha256": raw["fixture_sha256"],
                    "cleanup_errors": raw["cleanup_errors"],
                    "database_retry": raw["database_retry"],
                }
            if exporter.file_hash(executable) != expected_cli:
                raise ValueError("installed executable changed during acceptance")
            report["lifecycle_report_sha256"] = exporter.file_hash(lifecycle)
            if os.name == "nt":
                cleanup = report.get("runtime_process_owner_cleanup", {})
                if (cleanup.get("process_tree_cleanup_confirmed") is not True
                        or cleanup.get("windows_job_drained") is not True):
                    raise ValueError("Windows fixture Job cleanup is unconfirmed")
                report["fixture_cleanup_confirmed"] = True
            if report.get("fixture_cleanup_confirmed") is not True:
                raise ValueError("fixture and database cleanup is unconfirmed")
            report["cleanup_unconfirmed"] = False
            report["status"] = "passed"
    except BaseException as error:
        # Raw lifecycle JSON retains bounded categorical/request diagnostics.
        # Avoid serializing download URLs or token-bearing exception strings.
        report["status"] = "failed"
        report["failure_kind"] = type(error).__name__
        raise
    finally:
        save_report(output, report)
    return report


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--repo", required=True, type=Path)
    parser.add_argument("--version", required=True)
    parser.add_argument("--target", required=True, choices=sorted(TARGET_HOSTS))
    parser.add_argument("--commit", required=True)
    parser.add_argument("--output", required=True, type=Path)
    parser.add_argument("--assets", type=Path, help="offline exact archive+manifest directory; no outer ZIP claim")
    args = parser.parse_args()
    try:
        result = verify(args)
    except BaseException as error:
        print(f"installed PG verification failed ({type(error).__name__}); see bounded JSON evidence", file=sys.stderr)
        return 1
    print(json.dumps({"status": result["status"], "target": result["target"]}))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
