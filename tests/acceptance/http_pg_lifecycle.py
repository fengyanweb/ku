#!/usr/bin/env python3
"""Real native HTTP -> PostgreSQL acceptance in a private disposable cluster.

No existing db.conn, credentials or database are read. Requires PostgreSQL server
tools and libpq development files. Proxy faults affect only this fixture. Output
is evidence, never a claim that an unknown write was rolled back. By default the
synchronous and named async handlers run sequentially in separate private
clusters. The requested output embeds both independent lane reports, including
the current phase, so the existing CI artifact retains either lane's failure.
"""

import argparse
import concurrent.futures
import hashlib
import http.client
import json
import os
from pathlib import Path
import select
import shutil
import socket
import struct
import subprocess
import sys
import tempfile
import threading
import time
import traceback

sys.dont_write_bytecode = True
from multiloop_load import process_sample


def deadline_remaining(deadline):
    remaining = deadline - time.monotonic()
    if remaining <= 0:
        raise TimeoutError("HTTP absolute recovery deadline exceeded")
    return remaining


class DeadlineSocket(socket.socket):
    """One absolute budget, including every buffered HTTP receive and send."""

    def __init__(self, deadline):
        self.deadline = deadline
        super().__init__(socket.AF_INET, socket.SOCK_STREAM)

    def connect(self, address):
        self.settimeout(deadline_remaining(self.deadline))
        return super().connect(address)

    def sendall(self, data, flags=0):
        remaining = memoryview(data)
        while remaining:
            self.settimeout(deadline_remaining(self.deadline))
            sent = super().send(remaining, flags)
            if not sent:
                raise ConnectionError("HTTP request write made no progress")
            remaining = remaining[sent:]

    def recv_into(self, buffer, nbytes=0, flags=0):
        # socket.makefile -> SocketIO -> recv_into is the path used by
        # http.client's buffered status/header/body readers. A socket timeout
        # set only at connection creation would renew on every such read.
        self.settimeout(deadline_remaining(self.deadline))
        return super().recv_into(buffer, nbytes, flags)


class DeadlineHTTPConnection(http.client.HTTPConnection):
    """Private literal-IPv4 fixture connection; no DNS, tunnel or retry path."""

    def __init__(self, host, port, deadline):
        if host != "127.0.0.1":
            raise ValueError("absolute-deadline fixture requires literal loopback IPv4")
        super().__init__(host, port)
        self.deadline = deadline

    def connect(self):
        deadline_remaining(self.deadline)  # Do not even open a socket after expiry.
        connection = DeadlineSocket(self.deadline)
        try:
            connection.connect((self.host, self.port))
            connection.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
        except BaseException:
            connection.close()
            raise
        self.sock = connection


def recover_new_reads(request, observe, expected, recovery, *, began, deadline_seconds,
                      max_new_reads, intermediate_errors, start_event, progress):
    """Observe bounded fresh reads after a fault; never replay the failed operation."""
    deadline = began + deadline_seconds
    recovery.update(observations=[], recovered=False, failed_request_replay=False,
                    started_monotonic=began, absolute_deadline_monotonic=deadline,
                    start_event=start_event,
                    max_new_reads=max_new_reads,
                    probe_schedule="earliest start at began + index * window / max_new_reads; overdue probes start immediately",
                    stop_reason="request_or_validation_error",
                    deadline_semantics=f"one absolute {deadline_seconds:g}s window; each new request has min(4s, remaining window), including connect/write/all reads")
    allowed = {"error:" + code for code in intermediate_errors}
    try:
        for index in range(max_new_reads):
            now = time.monotonic()
            if now >= deadline:
                break
            # Spread the bounded fresh reads over the existing window. Eight
            # fast acquire errors must not consume every probe in its first
            # second. Waiting does not renew either absolute deadline.
            scheduled = began + index * deadline_seconds / max_new_reads
            if now < scheduled:
                time.sleep(min(scheduled - now, deadline - now))
            if time.monotonic() >= deadline:
                break
            progress(index + 1)
            # Progress checkpoints and scheduling consume the same window too.
            if time.monotonic() >= deadline:
                break
            try:
                body = request("/read", deadline=deadline)
            finally:
                observation = dict(observe())
                observation["recovery_scheduled_monotonic"] = scheduled
                observation["recovery_elapsed_seconds"] = time.monotonic() - began
                recovery["observations"].append(observation)
            if body.startswith("error:"):
                assert body in allowed, observation
                continue
            assert json.loads(body) == expected, observation
            deadline_remaining(deadline)
            recovery["recovered"] = True
            recovery["stop_reason"] = "recovered"
            break
        if not recovery["recovered"]:
            recovery["stop_reason"] = ("absolute_deadline" if time.monotonic() >= deadline
                                       else "new_read_limit")
    finally:
        recovery["finished_monotonic"] = time.monotonic()
        recovery["elapsed_seconds"] = recovery["finished_monotonic"] - began
        recovery["full_deadline_window_elapsed"] = recovery["finished_monotonic"] >= deadline
        if recovery["full_deadline_window_elapsed"] and not recovery["recovered"]:
            recovery["stop_reason"] = "absolute_deadline"
    if recovery["finished_monotonic"] > deadline or (
            not recovery["recovered"] and recovery["finished_monotonic"] >= deadline):
        recovery["recovered"] = False
        recovery["stop_reason"] = "absolute_deadline"
        raise TimeoutError(f"database recovery absolute deadline exceeded after {len(recovery['observations'])} new read requests")
    assert recovery["recovered"], recovery


def recover_fault_reads(request, report, expected, recovery_key, source_request, *,
                        start_event, progress):
    """Use the fault response's completion, including later checkpoint/oracle time."""
    recovery = {}
    report[recovery_key] = recovery
    recover_new_reads(request, lambda: report["last_request"], expected, recovery,
                      began=source_request["finished_monotonic"],
                      deadline_seconds=report["parameters"]["fault_recovery_deadline_seconds"],
                      max_new_reads=report["parameters"]["disconnect_recovery_max_new_reads"],
                      intermediate_errors=report["parameters"]["disconnect_recovery_intermediate_errors"],
                      start_event=start_event, progress=progress)


def port():
    with socket.socket() as listener:
        listener.bind(("127.0.0.1", 0))
        return listener.getsockname()[1]


def checkpoint(report, output, phase, **details):
    """Keep one atomic current checkpoint, including during fixture cleanup."""
    report["current_phase"] = phase
    report["phase_details"] = details
    report["updated_monotonic"] = time.monotonic()
    output = Path(output)
    output.parent.mkdir(parents=True, exist_ok=True)
    pending = output.with_name(output.name + ".tmp")
    pending.write_text(json.dumps(report, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")
    pending.replace(output)
    print(json.dumps({"phase": phase, **details}, ensure_ascii=False), flush=True)


def run(args, progress=None, **kwargs):
    """Wait for the tool, not EOF from an intentionally backgrounded server.

    Windows subprocess.run kills only its direct child on timeout, then calls
    communicate without a deadline. A descendant retaining stdout can therefore
    outlive that timeout. Files avoid pipe-reader joins; the outer acceptance
    process owns containment of the complete private fixture process tree.
    """
    tool = Path(str(args[0])).name
    operation = next((str(arg) for arg in args[1:] if str(arg) in ("start", "stop")), None)
    label = tool + (":" + operation if operation else "")

    def mark(state, **details):
        phase = label + ":" + state
        if progress:
            progress(phase, **details)
        else:
            print(json.dumps({"phase": phase, **details}), flush=True)

    mark("starting", timeout_seconds=60)
    began = time.monotonic()
    with tempfile.TemporaryFile() as stdout, tempfile.TemporaryFile() as stderr:
        child = subprocess.Popen(args, stdin=subprocess.DEVNULL, stdout=stdout, stderr=stderr, **kwargs)
        mark("running", pid=child.pid, timeout_seconds=60)
        timed_out = False
        cleanup_error = None
        try:
            child.wait(timeout=60)
        except subprocess.TimeoutExpired:
            timed_out = True
            try:
                child.kill()
                child.wait(timeout=5)
            except (OSError, subprocess.TimeoutExpired) as error:
                cleanup_error = f"{type(error).__name__}: {error}"
        captured = []
        sizes = []
        for stream in (stdout, stderr):
            sizes.append(stream.seek(0, os.SEEK_END))
            stream.seek(max(0, sizes[-1] - 65536))
            captured.append(stream.read(65536).decode("utf-8", errors="replace"))
        out, err = captured
        mark("timeout" if timed_out else "finished", pid=child.pid, exit_code=child.returncode,
             elapsed_seconds=time.monotonic() - began, stdout_bytes=sizes[0], stderr_bytes=sizes[1],
             stdout_tail=out[-2000:], stderr_tail=err[-4000:], direct_child_cleanup_error=cleanup_error)
        if timed_out:
            raise RuntimeError(f"{tool} exceeded 60 seconds; direct-child cleanup: {cleanup_error or 'reaped'}")
        if any(size > 65536 for size in sizes):
            raise RuntimeError(f"{tool} exceeded the fixture's 65536-byte per-stream output limit")
        if child.returncode:
            raise RuntimeError(f"{tool} failed: {err[-4000:]}")
        return out.strip()


def cleanup_fixture(report, output, service, proxy, data, stop_database, started,
                    on_checkpoint=None):
    """Attempt every cleanup and save its result even if another cleanup fails."""
    errors = []

    def mark(phase, **details):
        checkpoint(report, output, phase, **details)
        if on_checkpoint is not None:
            on_checkpoint(report)

    def clean(label, action):
        mark("cleanup:" + label + ":starting")
        try:
            action()
        except Exception as error:
            errors.append({"stage": label, "error": f"{type(error).__name__}: {error}"})

    def stop_service():
        if service is not None and service.poll() is None:
            service.kill()
            service.wait(timeout=5)

    try:
        clean("http", stop_service)
        if proxy is not None:
            clean("proxy", proxy.close)
        # pg_ctl may have created a server before its launching tool failed.
        # Only this fixture's private data path is inspected or stopped.
        if started or (Path(data) / "postmaster.pid").is_file():
            clean("postgres", stop_database)
    finally:
        report["cleanup_errors"] = errors
        if errors:
            report["status"] = "failed"
        mark("cleanup:finished", error_count=len(errors))
    if errors:
        raise RuntimeError(f"private PostgreSQL fixture cleanup failed: {errors}")


def prepare_windows_pg_directory(root, report):
    """Give only this user's restricted PostgreSQL token the fixture's access.

    PostgreSQL 17 reexecutes initdb with Administrators and Power Users marked
    deny-only (src/common/restricted_token.c). Python's Windows mkdir(0700)
    grants SYSTEM, Administrators and OWNER RIGHTS; the owner can itself be
    Administrators. Add the actual user SID on this newly created private root,
    keeping the protected ACL and all parent directories unchanged. Do not set
    PG_RESTRICT_EXEC, which would bypass PostgreSQL's normal token transition.
    """
    if sys.platform != "win32":
        return
    root = Path(root)
    if (root.is_symlink() or not root.is_dir()
            or not root.name.startswith("ku-http-pg-acceptance-")
            or root.resolve().parent != Path(tempfile.gettempdir()).resolve()):
        raise ValueError("PostgreSQL ACL setup requires this fixture's private temporary root")
    script = r"""
$ErrorActionPreference = 'Stop'
[Console]::OutputEncoding = [System.Text.UTF8Encoding]::new($false)
$root = $env:KU_PG_FIXTURE_ROOT
$sid = [System.Security.Principal.WindowsIdentity]::GetCurrent().User
$evidence = @{user_sid=$sid.Value; engine_version=$PSVersionTable.PSVersion.ToString();
  engine_edition=$PSVersionTable.PSEdition; engine_home=$PSHOME; module_path=$env:PSModulePath;
  rights='Modify'; inheritance='ContainerInherit,ObjectInherit';
  scope='private fixture root and inherited children only'}
try {
    $before = Get-Acl -LiteralPath $root
    $evidence.sddl_before = $before.Sddl
    $evidence.owner_before = $before.Owner
    $parent = [System.IO.Directory]::GetParent($root).FullName
    $evidence.parent_sddl_before = (Get-Acl -LiteralPath $parent).Sddl
    $requiredRights = [System.Security.AccessControl.FileSystemRights]::Modify
    $requiredInheritance = [System.Security.AccessControl.InheritanceFlags]'ContainerInherit, ObjectInherit'
    $rule = [System.Security.AccessControl.FileSystemAccessRule]::new(
        $sid, $requiredRights, $requiredInheritance,
        [System.Security.AccessControl.PropagationFlags]::None,
        [System.Security.AccessControl.AccessControlType]::Allow)
    $before.AddAccessRule($rule)
    Set-Acl -LiteralPath $root -AclObject $before
    $after = Get-Acl -LiteralPath $root
    $evidence.owner_after = $after.Owner
    $evidence.sddl_after = $after.Sddl
    $evidence.parent_sddl_after = (Get-Acl -LiteralPath $parent).Sddl
    $matchingRules = @()
    foreach ($entry in $after.GetAccessRules($true, $true, [System.Security.Principal.SecurityIdentifier])) {
        $entrySid = $entry.IdentityReference.Translate([System.Security.Principal.SecurityIdentifier]).Value
        if ($entrySid -eq $sid.Value) {
            $matchingRules += @{sid=$entrySid;
              allow=($entry.AccessControlType -eq [System.Security.AccessControl.AccessControlType]::Allow);
              modify=(($entry.FileSystemRights -band $requiredRights) -eq $requiredRights);
              object_and_container_inherit=(($entry.InheritanceFlags -band $requiredInheritance) -eq $requiredInheritance);
              applies_to_root=($entry.PropagationFlags -eq [System.Security.AccessControl.PropagationFlags]::None)}
        }
    }
    $evidence.current_user_access_rules = $matchingRules
    $evidence.current_user_modify_inheritable = @($matchingRules | Where-Object {
        $_.allow -and $_.modify -and $_.object_and_container_inherit -and $_.applies_to_root
    }).Count -gt 0
} catch {
    $evidence.error = $_.Exception.Message
    $evidence | ConvertTo-Json -Compress
    exit 1
}
$evidence | ConvertTo-Json -Compress -Depth 4
"""
    # A pwsh parent exports its Core-only module search path. Windows PowerShell
    # must build its own defaults to load Microsoft.PowerShell.Security/Get-Acl.
    # Change only this child's copy; PATH/libpq and the caller stay untouched.
    environment = {name: value for name, value in os.environ.items() if name.upper() != "PSMODULEPATH"}
    environment["KU_PG_FIXTURE_ROOT"] = str(root.resolve())
    result = subprocess.run(["powershell.exe", "-NoLogo", "-NoProfile", "-NonInteractive", "-Command", script],
                            capture_output=True, text=True, encoding="utf-8", errors="replace", timeout=20,
                            env=environment)
    evidence = {"exit_code": result.returncode, "python_version": sys.version,
                "child_psmodulepath_removed": True,
                "parent_psmodulepath_present": any(name.upper() == "PSMODULEPATH" for name in os.environ),
                "stdout_tail": result.stdout[-12000:], "stderr_tail": result.stderr[-2000:]}
    report["windows_pg_directory_acl"] = evidence
    if result.returncode or len(result.stdout) > 12000:
        raise RuntimeError(f"private PostgreSQL directory ACL setup failed: {evidence}")
    evidence["acl"] = json.loads(result.stdout)
    del evidence["stdout_tail"]
    if evidence["acl"]["parent_sddl_before"] != evidence["acl"]["parent_sddl_after"]:
        raise RuntimeError("PostgreSQL fixture parent ACL changed during private root setup")
    if evidence["acl"].get("current_user_modify_inheritable") is not True:
        raise RuntimeError("PostgreSQL fixture user SID lacks the required inherited Modify rule")


class DatabaseProxy:
    """Bounded test proxy; lose one confirmed INSERT acknowledgement."""

    def __init__(self, backend):
        self.backend = backend
        self.listener = socket.socket()
        self.listener.bind(("127.0.0.1", 0))
        self.listener.listen(8)
        self.listener.settimeout(0.1)
        self.port = self.listener.getsockname()[1]
        self.stopping = threading.Event()
        self.drop_insert = threading.Event()
        self.dropped = threading.Event()
        self.workers = []
        self.sockets = []
        self.lock = threading.Lock()
        self.capacity = threading.BoundedSemaphore(8)
        self.thread = threading.Thread(target=self.accept, daemon=True)
        self.thread.start()

    def accept(self):
        while not self.stopping.is_set():
            try:
                client, _ = self.listener.accept()
            except socket.timeout:
                continue
            except OSError:
                break
            if not self.capacity.acquire(blocking=False):
                client.close()
                continue
            worker = threading.Thread(target=self.forward, args=(client,), daemon=True)
            self.workers.append(worker)
            worker.start()

    def forward(self, client):
        backend = None
        try:
            backend = socket.create_connection(("127.0.0.1", self.backend), timeout=2)
            with self.lock:
                self.sockets.extend([client, backend])
            pending = bytearray()
            while not self.stopping.is_set():
                readable, _, _ = select.select([client, backend], [], [], 0.1)
                for source in readable:
                    data = source.recv(65536)
                    if not data:
                        return
                    if source is client:
                        backend.sendall(data)
                        continue
                    pending.extend(data)
                    while len(pending) >= 5:
                        length = struct.unpack("!I", pending[1:5])[0]
                        if length < 4 or length > 2 * 1024 * 1024:
                            raise RuntimeError("fixture protocol frame exceeds bound")
                        size = length + 1
                        if len(pending) < size:
                            break
                        frame = bytes(pending[:size])
                        del pending[:size]
                        if frame[:1] == b"C" and frame[5:].startswith(b"INSERT ") and self.drop_insert.is_set():
                            self.drop_insert.clear()
                            self.dropped.set()
                            return
                        client.sendall(frame)
        except (OSError, ValueError):
            pass
        finally:
            for connection in (client, backend):
                if connection is not None:
                    with self.lock:
                        if connection in self.sockets:
                            self.sockets.remove(connection)
                    connection.close()
            self.capacity.release()

    def close(self):
        self.stopping.set()
        self.listener.close()
        self.thread.join(timeout=3)
        with self.lock:
            for connection in self.sockets:
                try:
                    connection.shutdown(socket.SHUT_RDWR)
                except OSError:
                    pass
        for worker in self.workers:
            worker.join(timeout=3)
        if self.thread.is_alive() or any(worker.is_alive() for worker in self.workers):
            raise RuntimeError("fixture proxy failed to terminate")


def parse_args(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--ku", required=True, type=Path)
    parser.add_argument("--output", required=True, type=Path)
    parser.add_argument("--loops", type=int, default=4)
    parser.add_argument("--performance-seconds", type=int, default=0, help="Run only bounded read throughput for 3..60s; omit fault replay")
    parser.add_argument("--profile", choices=["debug", "release"], default="debug")
    parser.add_argument("--lane", choices=["both", "sync", "async"], default="both",
                        help="Run independent sync and/or named async HTTP -> PostgreSQL lifecycles")
    args = parser.parse_args(argv)
    if args.performance_seconds and not 3 <= args.performance_seconds <= 60:
        parser.error("performance-seconds must be 3..60")
    return args


def run_lane(args, lane, on_checkpoint=None):
    ku = args.ku.resolve()
    filename = "http_pg_lifecycle.ku" if lane == "sync" else "http_pg_lifecycle_async.ku"
    source = Path(__file__).resolve().parents[1] / "fixtures" / filename
    report = {"platform": os.uname().sysname if hasattr(os, "uname") else os.name,
              "lane": lane, "fixture": filename,
              "event_loops": args.loops, "database_retry": False,
              "cli_sha256": hashlib.sha256(ku.read_bytes()).hexdigest(),
              "fixture_sha256": hashlib.sha256(source.read_bytes()).hexdigest(),
              "parameters": {"blocking_threads": 8, "database_connections": 4,
                             "native_c_profile": args.profile,
                             "database_waiters": 8, "database_pending_bytes": 1048576,
                             "acquire_timeout_ms": 100, "query_timeout_ms": 150, "handler_timeout_ms": 2000,
                             "fault_recovery_deadline_seconds": 5,
                             "disconnect_recovery_max_new_reads": 8,
                             "disconnect_recovery_intermediate_errors": ["query_error", "execution_unknown", "acquire_timeout"],
                             "post_fault_recovery_contract": "fresh reads within the existing 5s/8-read bound; first read need not succeed; window starts at original fault response completion and includes checkpoints and write-side-effect checks",
                             "http_client_connection_policy": "eight persistent clients, no failed-request replay" if args.performance_seconds else "one connection per isolated fault request",
                             "database_transport": "direct Ku/libpq to private PostgreSQL; no fault proxy" if args.performance_seconds else "private Python fault proxy, capacity eight connections",
                             "tls": "disabled only inside isolated loopback fixture"},
              "checks": {}, "status": "running"}
    def progress(phase, **details):
        if phase.startswith(("http:", "fault:")):
            report["acceptance_phase"] = phase
        checkpoint(report, args.output, phase, **details)
        if on_checkpoint is not None:
            on_checkpoint(report)

    def execute(arguments, **kwargs):
        return run(arguments, progress=progress, **kwargs)

    progress("fixture:starting")
    pg_config = shutil.which("pg_config") or "pg_config"
    tools = Path(execute([pg_config, "--bindir"]))
    suffix = ".exe" if os.name == "nt" else ""
    for name in ["initdb", "pg_ctl", "psql"]:
        if not (tools / (name + suffix)).is_file():
            raise RuntimeError(f"missing PostgreSQL server tool: {name}")
    with tempfile.TemporaryDirectory(prefix="ku-http-pg-acceptance-") as temporary:
        root = Path(temporary)
        database_port, http_port = port(), port()
        data = root / "data"
        service = None
        request = None
        proxy = None
        control_token = None
        started = False
        logs = root / "http.log"
        try:
            progress("fixture-acl:starting")
            prepare_windows_pg_directory(root, report)
            progress("fixture-acl:finished")
            environment = os.environ | {"KU_EVENT_LOOPS": str(args.loops), "KU_BLOCKING_THREADS": "8",
                                        "KU_DB_MAX_CONNECTIONS": "4", "KU_DB_MAX_WAITERS": "8",
                                        "KU_DB_MAX_PENDING_BYTES": "1048576"}
            control_token = os.urandom(24).hex()
            environment["KU_SERVICE_CONTROL_TOKEN"] = control_token
            if not environment.get("KU_PG_LIB"):
                libdir = Path(execute([pg_config, "--libdir"]))
                library = next((libdir / name for name in ["libpq.dylib", "libpq.so", "libpq.lib", "libpq.dll.a"]
                                if (libdir / name).is_file()), None)
                if library is None:
                    raise RuntimeError("pg_config directory has no supported libpq shared/import library")
                private_lib = root / "libpq-link"
                private_lib.mkdir()
                if os.name == "nt":
                    shutil.copy2(library, private_lib / library.name)
                else:
                    (private_lib / library.name).symlink_to(library.resolve())
                environment["KU_PG_LIB"] = str(private_lib)
            environment["PGCLIENTENCODING"] = "UTF8"
            environment["PATH"] = str(tools) + os.pathsep + environment.get("PATH", "")
            execute([tools / "initdb", "-D", data, "-U", "ku_accept", "--auth-local=trust",
                 "--auth-host=trust", "--encoding=UTF8", "--locale=C", "--no-instructions"])
            options = f"-h 127.0.0.1 -p {database_port} -c max_connections=24 -c shared_buffers=16MB"
            if os.name != "nt":
                options += f' -k "{root}"'
            execute([tools / "pg_ctl", "-D", data, "-l", root / "postgres.log", "-w", "-t", "20", "start", "-o", options])
            started = True

            def sql(statement):
                return execute([tools / "psql", "-X", "-A", "-t", "-v", "ON_ERROR_STOP=1",
                            "-h", "127.0.0.1", "-p", str(database_port), "-U", "ku_accept",
                            "-d", "postgres", "-c", statement], env=environment)

            report["postgres_version"] = sql("SHOW server_version")
            sql("CREATE TABLE ku_events(id text PRIMARY KEY, payload text NOT NULL)")
            # Fault injection needs a protocol proxy. Read-only performance
            # measures the actual Ku/libpq/PostgreSQL path and must not include
            # a Python forwarder competing with its own HTTP load generator.
            proxy = None if args.performance_seconds else DatabaseProxy(database_port)
            connection_port = database_port if proxy is None else proxy.port
            (root / "db.conn").write_text(f"hostaddr=127.0.0.1 port={connection_port} user=ku_accept dbname=postgres sslmode=disable application_name=ku_runtime_acceptance", encoding="utf-8")
            (root / "http.addr").write_text(f"127.0.0.1:{http_port}", encoding="utf-8")
            local_source = root / "service.ku"
            shutil.copyfile(source, local_source)
            executable = root / ("service.exe" if os.name == "nt" else "service")
            build_start = time.monotonic()
            execute([ku, "build", "--backend", "c", "--profile", args.profile, local_source, "-o", executable], cwd=root, env=environment)
            report["build_seconds"] = time.monotonic() - build_start
            with logs.open("wb") as log:
                service = subprocess.Popen([executable], cwd=root, env=environment, stdout=log, stderr=log)

            request_lock = threading.Lock()

            def request(path, method="GET", control=False, persistent=None, phase=None, deadline=None):
                entered = time.monotonic()
                phase = (phase or report.get("acceptance_phase", "http:unknown"))[:128]
                observation = {"phase": phase, "route": path[:256], "method": method[:16], "status": None,
                               "body_prefix": None, "started_monotonic": entered}
                with request_lock:
                    if not args.performance_seconds:
                        observation["sequence"] = report.get("request_observations_total", 0) + 1
                        report["request_observations_total"] = observation["sequence"]
                    report["last_request"] = dict(observation)
                if deadline is not None and persistent is not None:
                    raise ValueError("absolute recovery deadline requires a distinct new connection")
                request_deadline = None if deadline is None else min(entered + 4, deadline)
                if request_deadline is not None:
                    observation["absolute_deadline_monotonic"] = request_deadline
                connection = persistent or (http.client.HTTPConnection("127.0.0.1", http_port, timeout=4)
                    if request_deadline is None else DeadlineHTTPConnection("127.0.0.1", http_port, request_deadline))
                try:
                    headers = {"X-Ku-Control-Token": control_token} if control else {}
                    if persistent is not None:
                        headers["Connection"] = "keep-alive"
                    connection.request(method, path, headers=headers)
                    response = connection.getresponse()
                    observation["status"] = response.status
                    raw_body = response.read(1024 * 1024)
                    prefix = raw_body.replace(control_token.encode("ascii"), b"<control-token>")[:2048]
                    observation.update(body_prefix=prefix.decode("utf-8", errors="replace").replace(control_token, "<control-token>"),
                                       body_prefix_raw_bytes=min(len(raw_body), 2048),
                                       body_bytes=len(raw_body), body_prefix_truncated=len(raw_body) > 2048)
                    body = raw_body.decode()
                    if request_deadline is not None:
                        # Buffered data may need no additional socket read.
                        # Completion, decoding and scheduling still consume the
                        # same absolute budget; late success is never accepted.
                        deadline_remaining(request_deadline)
                    if response.status != 200:
                        raise RuntimeError(f"HTTP {response.status}: {observation['body_prefix']}")
                    return body
                except Exception as error:
                    observation["exception"] = f"{type(error).__name__}: {error}".replace(control_token, "<control-token>")[:2048]
                    raise
                finally:
                    if persistent is None:
                        connection.close()
                    observation["finished_monotonic"] = time.monotonic()
                    observation["elapsed_seconds"] = observation["finished_monotonic"] - entered
                    with request_lock:
                        report["last_request"] = dict(observation)
                        if not args.performance_seconds:
                            # Keep bounded fault evidence without adding disk I/O
                            # to the separate read-throughput measurement.
                            history = report.setdefault("request_observations", [])
                            history.append(dict(observation))
                            del history[:-32]
                            checkpoint(report, args.output, phase + ":request", request=observation)

            deadline = time.monotonic() + 10
            progress("http:readiness")
            while True:
                if service.poll() is not None:
                    raise RuntimeError(f"HTTP service exited: {logs.read_text(encoding='utf-8')[-4000:]}")
                try:
                    if request("/health") == "ready":
                        break
                except OSError:
                    pass
                if time.monotonic() >= deadline:
                    raise RuntimeError("HTTP readiness deadline exceeded")
                time.sleep(0.02)
            expected = {"text": "中文🙂 ' \\", "number": "42", "absent": True, "rows": 1, "cols": 3}
            progress("http:acceptance")
            if args.performance_seconds:
                before = process_sample(service.pid)
                began = time.monotonic()
                deadline = began + args.performance_seconds
                def measure(_):
                    buckets = [0]*10001
                    successes = 0
                    errors = {}
                    examples = []
                    connection = http.client.HTTPConnection("127.0.0.1", http_port, timeout=4)
                    while time.monotonic() < deadline:
                        entered = time.monotonic()
                        category = "success"
                        observed_body = None
                        try:
                            observed_body = request("/read", persistent=connection)
                            if observed_body.startswith("error:"):
                                category = "ku_error:" + observed_body[6:70]
                            elif json.loads(observed_body) != expected:
                                category = "wrong_fields"
                        except Exception as error:
                            # Count the failed logical request once. Closing
                            # permits a subsequent new request to reconnect;
                            # this request is never replayed.
                            connection.close()
                            category = type(error).__name__
                            if len(examples) < 2:
                                examples.append({"category": category, "message": str(error)[:200],
                                                 "body": observed_body[:200] if observed_body is not None else None})
                        if category != "success" and len(examples) < 2 and not any(item.get("body") == observed_body for item in examples):
                            examples.append({"category": category, "body": observed_body[:200] if observed_body is not None else None})
                        buckets[min(10000, int((time.monotonic()-entered)*10000))] += 1
                        if category == "success":
                            successes += 1
                        else:
                            errors[category] = errors.get(category, 0)+1
                    connection.close()
                    return buckets, successes, errors, examples
                with concurrent.futures.ThreadPoolExecutor(max_workers=8) as clients:
                    rows = list(clients.map(measure, range(8)))
                elapsed = time.monotonic()-began
                histogram = [sum(row[0][index] for row in rows) for index in range(10001)]
                completed = sum(histogram)
                def percentile(fraction):
                    target, count = max(1, int(completed*fraction+.999999)), 0
                    for index, bucket in enumerate(histogram):
                        count += bucket
                        if count >= target:
                            return (index+1)/10
                    return 0
                failures = {}
                for row in rows:
                    for key, count in row[2].items():
                        failures[key] = failures.get(key, 0)+count
                report["throughput"] = {"seconds": elapsed, "clients": 8,
                    "arrivals_completed": completed, "successes": sum(row[1] for row in rows),
                    "errors": failures, "requests_per_second": completed/elapsed,
                    "error_examples": [example for row in rows for example in row[3]][:12],
                    "p50_ms": percentile(.5), "p95_ms": percentile(.95), "p99_ms": percentile(.99),
                    "before": before, "after": process_sample(service.pid),
                    "scope": "HTTP read -> Ku four-connection libpq client -> private PostgreSQL directly -> per-field JSON validation; no Python fault proxy; same-host fixed eight persistent HTTP clients; no failed-request replay or write faults"}
                assert completed > 0 and not failures, report["throughput"]
            else:
                progress("http:parameters-fields-unicode-null")
                assert json.loads(request("/read")) == expected
                report["checks"]["parameters_fields_unicode_null"] = "passed"
                progress("http:write-normal")
                assert request("/insert/normal") == "中文🙂 payload"
                if os.name == "nt":
                    # Retain the previous oracle's literal bytes as evidence;
                    # correctness below is checked independently of this argv.
                    report["psql_unicode_argv_literal_utf8_hex"] = sql(
                        "SELECT encode(convert_to('中文🙂 payload', 'UTF8'), 'hex')")
                # ASCII SQL and output avoid depending on Windows psql's
                # non-ASCII argv conversion while checking the actual DB bytes.
                write_observation = json.loads(sql(
                    "SELECT json_build_object("
                    "'server_encoding', current_setting('server_encoding'), "
                    "'client_encoding', current_setting('client_encoding'), "
                    "'total_count', (SELECT count(*) FROM ku_events), "
                    "'rows', (SELECT COALESCE(json_agg(json_build_object("
                    "'id_utf8_hex', encode(convert_to(id, 'UTF8'), 'hex'), "
                    "'payload_utf8_hex', encode(convert_to(payload, 'UTF8'), 'hex'), "
                    "'octet_length', octet_length(convert_to(payload, 'UTF8')))), '[]'::json) "
                    "FROM (SELECT id, payload FROM ku_events ORDER BY id LIMIT 2) AS observed))"))
                report["write_observation"] = write_observation
                progress("http:write-observation")
                payload_bytes = "中文🙂 payload".encode("utf-8")
                assert write_observation["total_count"] == 1, write_observation
                assert write_observation["rows"] == [{
                    "id_utf8_hex": b"normal".hex(), "payload_utf8_hex": payload_bytes.hex(),
                    "octet_length": len(payload_bytes)}], write_observation
                report["checks"]["write_side_effect"] = "exactly one row"
                progress("http:large-result")
                assert request("/rows") == "10000:10000"
                report["checks"]["large_result"] = "10000 rows, 2 columns, last row verified"
                progress("fault:slow-query")
                slow = request("/slow")
                slow_request = dict(report["last_request"])
                assert slow == "error:execution_unknown", slow
                progress("fault:timeout-new-read")
                recover_fault_reads(request, report, expected, "timeout_recovery", slow_request,
                                    start_event="original /slow response finished; subsequent checkpoints consume this window",
                                    progress=lambda index: progress("fault:timeout-new-read", new_request=index))
                report["checks"]["timeout_and_new_request_recovery"] = slow
                progress("fault:acknowledgement-lost-write")
                proxy.drop_insert.set()
                unknown = request("/insert/lost")
                lost_write_request = dict(report["last_request"])
                assert proxy.dropped.wait(1) and unknown == "error:execution_unknown", unknown
                progress("fault:acknowledgement-lost-side-effect")
                count = sql("SELECT count(*) FROM ku_events WHERE id='lost'")
                assert count == "1", count
                progress("fault:acknowledgement-lost-new-read")
                recover_fault_reads(request, report, expected, "lost_write_recovery", lost_write_request,
                                    start_event="original /insert/lost response finished; proxy confirmation, exactly-one-row oracle and checkpoints consume this window",
                                    progress=lambda index: progress("fault:acknowledgement-lost-new-read", new_request=index))
                report["checks"]["commit_acknowledgement_lost"] = {"reported": unknown, "actual_rows": int(count), "replayed": False}
                progress("fault:disconnect-terminate-backend")
                terminated = sql("SELECT pg_terminate_backend(pid) FROM pg_stat_activity WHERE application_name='ku_runtime_acceptance'")
                report["disconnect_backend_termination"] = {
                    "attempted": len(terminated.splitlines()),
                    "reported_terminated": terminated.splitlines().count("t"),
                    "raw_result": terminated[:256]}
                # Acquire retires stale slots lazily and a replacement connect
                # shares the original acquire deadline. No contract promises
                # that the second request succeeds. These are distinct read-only
                # requests; no failed operation or write is replayed.
                began = time.monotonic()
                recovery = {}
                report["disconnect_recovery"] = recovery
                recover_new_reads(request, lambda: report["last_request"], expected, recovery,
                                  began=began,
                                  deadline_seconds=report["parameters"]["fault_recovery_deadline_seconds"],
                                  max_new_reads=report["parameters"]["disconnect_recovery_max_new_reads"],
                                  intermediate_errors=report["parameters"]["disconnect_recovery_intermediate_errors"],
                                  start_event="pg_terminate_backend returned and termination result recorded",
                                  progress=lambda index: progress("fault:disconnect-new-read", new_request=index))
                report["checks"]["database_disconnect_recovery"] = {
                    "first": recovery["observations"][0]["body_prefix"], "next": "passed",
                    "new_read_requests": len(recovery["observations"]), "failed_request_replay": False}
                progress("fault:bounded-dependency-pressure")
                with concurrent.futures.ThreadPoolExecutor(max_workers=12) as pool:
                    outcomes = list(pool.map(lambda _: request("/slow"), range(12)))
                began = time.monotonic()
                counts = {value: outcomes.count(value) for value in sorted(set(outcomes))}
                assert all(value.startswith("error:") for value in outcomes), counts
                recovery = {}
                report["pressure_recovery"] = recovery
                recover_new_reads(request, lambda: report["last_request"], expected, recovery,
                                  began=began,
                                  deadline_seconds=report["parameters"]["fault_recovery_deadline_seconds"],
                                  max_new_reads=report["parameters"]["disconnect_recovery_max_new_reads"],
                                  intermediate_errors=report["parameters"]["disconnect_recovery_intermediate_errors"],
                                  start_event="all 12 pressure requests completed and the client pool joined",
                                  progress=lambda index: progress("fault:bounded-dependency-pressure-new-read", new_request=index))
                report["checks"]["bounded_dependency_pressure_recovery"] = counts
            progress("http:connection-accounting")
            connections = int(sql("SELECT count(*) FROM pg_stat_activity WHERE application_name='ku_runtime_acceptance'"))
            report["database_connections_after_dependency_pressure"] = connections
            assert connections <= 4, connections
            report["database_connections_before_exit"] = connections
            progress("http:runtime-metrics")
            report["runtime_metrics"] = json.loads(request("/__ku/metrics", control=True))
            assert len(report["runtime_metrics"]["loops"]) == args.loops
            assert all(loop["requests"] for loop in report["runtime_metrics"]["loops"])
            database_metrics = report["runtime_metrics"]["database"]
            validate_database_metrics(database_metrics, lane, not args.performance_seconds)
            if not args.performance_seconds:
                if lane == "sync":
                    report["checks"]["cancel_confirmed_and_local_charge_retired"] = database_metrics["confirmed_cancellations"]
                else:
                    # The async pump evicts/shuts down a failed lease; it does
                    # not execute synchronous PQcancel + drain confirmation.
                    report["checks"]["unknown_outcome_and_local_charge_retired"] = {
                        "unknown_results": database_metrics["unknown_results"],
                        "confirmed_cancellations": database_metrics["confirmed_cancellations"],
                        "database_rollback_confirmed": False,
                        "failed_request_replay": False}
            progress("http:normal-drain")
            request("/__ku/drain", method="POST", control=True)
            try:
                service.wait(timeout=5)
            except subprocess.TimeoutExpired:
                service.kill()
                service.wait(timeout=3)
                raise RuntimeError("service did not terminate within 5 seconds")
            assert service.returncode == 0, service.returncode
            report["checks"]["normal_drain_exit"] = service.returncode
            progress("http:connections-after-exit")
            assert sql("SELECT count(*) FROM pg_stat_activity WHERE application_name='ku_runtime_acceptance'") == "0"
            report["checks"]["database_connections_after_exit"] = 0
            report["status"] = "checks_passed"
        except Exception as error:
            report["status"] = "failed"
            failure = f"{type(error).__name__}: {error}"
            if control_token:
                failure = failure.replace(control_token, "<control-token>")
            report["failure"] = failure[:2048]
            report["failure_phase"] = report.get("acceptance_phase", report.get("current_phase", "unknown"))
            report["failure_request"] = dict(report.get("last_request", {}))
            report["failure_trace"] = [
                {"file": Path(frame.filename).name[:128], "line": frame.lineno,
                 "function": frame.name[:128]}
                for frame in traceback.extract_tb(error.__traceback__, limit=-12)]
            # JSONDecodeError alone hides valid HTTP error bodies such as
            # error:acquire_timeout. Print and persist the original response
            # before a distinct control request can replace last_request.
            checkpoint(report, args.output, "diagnostic:failure", failure=report["failure"],
                       failure_phase=report["failure_phase"], request=report["failure_request"])
            if request is not None and service is not None and service.poll() is None:
                try:
                    report["failure_runtime_metrics"] = json.loads(request(
                        "/__ku/metrics", control=True, phase="diagnostic:failure-metrics"))
                except Exception as metrics_error:
                    report["failure_runtime_metrics_error"] = (
                        f"{type(metrics_error).__name__}: {metrics_error}"
                        .replace(control_token, "<control-token>"))[:2048]
            if logs.is_file():
                report["service_log_tail"] = logs.read_text(encoding="utf-8", errors="replace")[-8192:]
            raise
        finally:
            cleanup_fixture(report, args.output, service, proxy, data,
                lambda: execute([tools / "pg_ctl", "-D", data, "-w", "-t", "15", "stop", "-m", "immediate"]), started,
                on_checkpoint=on_checkpoint)
    report["status"] = "passed"
    progress("fixture:finished")
    return report


def validate_database_metrics(metrics, lane, with_faults):
    assert metrics["reserved_connections"] == 4, metrics
    assert metrics["reserved_waiters"] == 8, metrics
    assert metrics["pending_operations"] == 0, metrics
    assert metrics["pending_bytes"] == 0, metrics
    if with_faults:
        if lane == "sync":
            assert metrics["confirmed_cancellations"] > 0, metrics
        else:
            assert metrics["unknown_results"] > 0, metrics


def run_lanes(args):
    lanes = ["sync", "async"] if args.lane == "both" else [args.lane]
    output = args.output.resolve()
    report = {"status": "running", "required_lanes": lanes, "lanes": {},
              "database_retry": False,
              "execution_policy": "sequential independent private clusters; no failed-request replay",
              "throughput_policy": "independent per-lane measurements; never pool lane QPS",
              "outer_acceptance_deadline": "unchanged; controlled by the existing CI process owner"}
    checkpoint(report, output, "lanes:starting")
    blocked = None
    interruption = None
    for lane in lanes:
        raw_output = output.with_name(f"{output.stem}.{lane}{output.suffix or '.json'}")
        entry = {"status": "blocked" if blocked else "running", "report_file": str(raw_output)}
        report["lanes"][lane] = entry
        if blocked:
            entry["reason"] = blocked
            checkpoint(report, output, "lane:blocked", lane=lane)
            continue

        def mirror(raw):
            # Existing phase boundaries only. In particular no disk writes are
            # added to the separate read-throughput request loop.
            entry["report"] = raw
            checkpoint(report, output, "lane:checkpoint", lane=lane,
                       lane_phase=raw.get("current_phase"))

        lane_args = argparse.Namespace(**(vars(args) | {"output": raw_output}))
        # Replace any earlier invocation's raw output before preflight. An old
        # successful cleanup receipt must never authorize the next new lane.
        checkpoint({"lane": lane, "status": "running"}, raw_output, "lane:starting")
        error = None
        try:
            run_lane(lane_args, lane, mirror)
        except BaseException as failure:
            error = failure
            if not isinstance(failure, Exception):
                interruption = failure
        finally:
            # Cleanup writes directly to the independent raw report even when
            # an earlier check raises; always retain that final cleanup result.
            if raw_output.is_file():
                try:
                    with raw_output.open("rb") as stream:
                        raw_bytes = stream.read(1048577)
                    if len(raw_bytes) > 1048576:
                        raise ValueError("lane report exceeds its 1 MiB evidence limit")
                    entry["report_sha256"] = hashlib.sha256(raw_bytes).hexdigest()
                    entry["report"] = json.loads(raw_bytes)
                except (OSError, ValueError) as failure:
                    entry["report_read_error"] = f"{type(failure).__name__}: {failure}"[:2048]
                    error = error or failure
                    blocked = f"{lane} lane final report is unavailable; cleanup is not confirmed"
            raw = entry.get("report", {})
            cleanup_confirmed = raw.get("cleanup_errors") == []
            entry["status"] = ("passed" if error is None and raw.get("status") == "passed"
                               and cleanup_confirmed else "failed")
            if error is not None:
                entry["failure"] = f"{type(error).__name__}: {error}"[:2048]
                if raw.get("status") != "failed" and "report_read_error" not in entry:
                    raw["status"] = "failed"
                    raw["failure"] = entry["failure"]
                    checkpoint(raw, raw_output, "lane:failed")
                    entry["report_sha256"] = hashlib.sha256(raw_output.read_bytes()).hexdigest()
            if not cleanup_confirmed:
                blocked = f"{lane} lane cleanup is not confirmed; no further fixture may start"
            if interruption is not None:
                blocked = f"{lane} lane interrupted; no further fixture may start"
            if entry["status"] != "passed":
                report["status"] = "failed"
            checkpoint(report, output, "lane:finished", lane=lane, status=entry["status"])

    report["status"] = "passed" if all(entry["status"] == "passed" for entry in report["lanes"].values()) else "failed"
    checkpoint(report, output, "lanes:finished")
    if interruption is not None:
        raise interruption
    if report["status"] != "passed":
        raise RuntimeError(f"HTTP PostgreSQL lifecycle failed; independent reports: {output}")
    return report


def main():
    report = run_lanes(parse_args())
    print(json.dumps({"status": report["status"], "lanes": {
        lane: entry["status"] for lane, entry in report["lanes"].items()}}, ensure_ascii=False))


if __name__ == "__main__":
    main()
