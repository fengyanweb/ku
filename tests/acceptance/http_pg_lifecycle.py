#!/usr/bin/env python3
"""Real native HTTP -> PostgreSQL acceptance in a private disposable cluster.

No existing db.conn, credentials or database are read. Requires PostgreSQL server
tools and libpq development files. Proxy faults affect only this fixture. Output
is evidence, never a claim that an unknown write was rolled back.
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

sys.dont_write_bytecode = True
from multiloop_load import process_sample


def port():
    with socket.socket() as listener:
        listener.bind(("127.0.0.1", 0))
        return listener.getsockname()[1]


def run(args, **kwargs):
    result = subprocess.run(args, capture_output=True, text=True, encoding="utf-8", timeout=60, **kwargs)
    if result.returncode:
        raise RuntimeError(f"{Path(str(args[0])).name} failed: {result.stderr[-4000:]}")
    return result.stdout.strip()


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


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--ku", required=True, type=Path)
    parser.add_argument("--output", required=True, type=Path)
    parser.add_argument("--loops", type=int, default=4)
    parser.add_argument("--performance-seconds", type=int, default=0, help="Run only bounded read throughput for 3..60s; omit fault replay")
    parser.add_argument("--profile", choices=["debug", "release"], default="debug")
    args = parser.parse_args()
    if args.performance_seconds and not 3 <= args.performance_seconds <= 60:
        parser.error("performance-seconds must be 3..60")
    ku = args.ku.resolve()
    pg_config = shutil.which("pg_config") or "pg_config"
    tools = Path(run([pg_config, "--bindir"]))
    suffix = ".exe" if os.name == "nt" else ""
    for name in ["initdb", "pg_ctl", "psql"]:
        if not (tools / (name + suffix)).is_file():
            raise RuntimeError(f"missing PostgreSQL server tool: {name}")
    source = Path(__file__).resolve().parents[1] / "fixtures/http_pg_lifecycle.ku"
    report = {"platform": os.uname().sysname if hasattr(os, "uname") else os.name,
              "event_loops": args.loops, "database_retry": False,
              "cli_sha256": hashlib.sha256(ku.read_bytes()).hexdigest(),
              "fixture_sha256": hashlib.sha256(source.read_bytes()).hexdigest(),
              "parameters": {"blocking_threads": 8, "database_connections": 4,
                             "native_c_profile": args.profile,
                             "database_waiters": 8, "database_pending_bytes": 1048576,
                             "acquire_timeout_ms": 100, "query_timeout_ms": 150, "handler_timeout_ms": 2000,
                             "fault_recovery_deadline_seconds": 5,
                             "http_client_connection_policy": "eight persistent clients, no failed-request replay" if args.performance_seconds else "one connection per isolated fault request",
                             "database_transport": "direct Ku/libpq to private PostgreSQL; no fault proxy" if args.performance_seconds else "private Python fault proxy, capacity eight connections",
                             "tls": "disabled only inside isolated loopback fixture"},
              "checks": {}}
    with tempfile.TemporaryDirectory(prefix="ku-http-pg-acceptance-") as temporary:
        root = Path(temporary)
        database_port, http_port = port(), port()
        data = root / "data"
        service = None
        proxy = None
        started = False
        logs = root / "http.log"
        try:
            environment = os.environ | {"KU_EVENT_LOOPS": str(args.loops), "KU_BLOCKING_THREADS": "8",
                                        "KU_DB_MAX_CONNECTIONS": "4", "KU_DB_MAX_WAITERS": "8",
                                        "KU_DB_MAX_PENDING_BYTES": "1048576"}
            control_token = os.urandom(24).hex()
            environment["KU_SERVICE_CONTROL_TOKEN"] = control_token
            if not environment.get("KU_PG_LIB"):
                libdir = Path(run([pg_config, "--libdir"]))
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
            run([tools / "initdb", "-D", data, "-U", "ku_accept", "--auth-local=trust",
                 "--auth-host=trust", "--encoding=UTF8", "--locale=C", "--no-instructions"])
            options = f"-h 127.0.0.1 -p {database_port} -c max_connections=24 -c shared_buffers=16MB"
            if os.name != "nt":
                options += f' -k "{root}"'
            run([tools / "pg_ctl", "-D", data, "-l", root / "postgres.log", "-w", "-t", "20", "start", "-o", options])
            started = True

            def sql(statement):
                return run([tools / "psql", "-X", "-A", "-t", "-v", "ON_ERROR_STOP=1",
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
            run([ku, "build", "--backend", "c", "--profile", args.profile, local_source, "-o", executable], cwd=root, env=environment)
            report["build_seconds"] = time.monotonic() - build_start
            with logs.open("wb") as log:
                service = subprocess.Popen([executable], cwd=root, env=environment, stdout=log, stderr=log)

            def request(path, method="GET", control=False, persistent=None):
                connection = persistent or http.client.HTTPConnection("127.0.0.1", http_port, timeout=4)
                try:
                    headers = {"X-Ku-Control-Token": control_token} if control else {}
                    if persistent is not None:
                        headers["Connection"] = "keep-alive"
                    connection.request(method, path, headers=headers)
                    response = connection.getresponse()
                    body = response.read(1024 * 1024).decode()
                    if response.status != 200:
                        raise RuntimeError(f"HTTP {response.status}: {body}")
                    return body
                finally:
                    if persistent is None:
                        connection.close()

            deadline = time.monotonic() + 10
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
                assert json.loads(request("/read")) == expected
                report["checks"]["parameters_fields_unicode_null"] = "passed"
                assert request("/insert/normal") == "中文🙂 payload"
                assert sql("SELECT count(*) FROM ku_events WHERE id='normal' AND payload='中文🙂 payload'") == "1"
                report["checks"]["write_side_effect"] = "exactly one row"
                assert request("/rows") == "10000:10000"
                report["checks"]["large_result"] = "10000 rows, 2 columns, last row verified"
                slow = request("/slow")
                assert slow == "error:execution_unknown", slow
                assert json.loads(request("/read")) == expected
                report["checks"]["timeout_and_new_request_recovery"] = slow
                proxy.drop_insert.set()
                unknown = request("/insert/lost")
                assert proxy.dropped.wait(1) and unknown == "error:execution_unknown", unknown
                count = sql("SELECT count(*) FROM ku_events WHERE id='lost'")
                assert count == "1", count
                assert json.loads(request("/read")) == expected
                report["checks"]["commit_acknowledgement_lost"] = {"reported": unknown, "actual_rows": int(count), "replayed": False}
                sql("SELECT pg_terminate_backend(pid) FROM pg_stat_activity WHERE application_name='ku_runtime_acceptance'")
                first = request("/read")
                assert first.startswith("error:") or json.loads(first) == expected
                assert json.loads(request("/read")) == expected
                report["checks"]["database_disconnect_recovery"] = {"first": first, "next": "passed"}
                with concurrent.futures.ThreadPoolExecutor(max_workers=12) as pool:
                    outcomes = list(pool.map(lambda _: request("/slow"), range(12)))
                counts = {value: outcomes.count(value) for value in sorted(set(outcomes))}
                assert all(value.startswith("error:") for value in outcomes), counts
                assert json.loads(request("/read")) == expected
                report["checks"]["bounded_dependency_pressure_recovery"] = counts
            connections = int(sql("SELECT count(*) FROM pg_stat_activity WHERE application_name='ku_runtime_acceptance'"))
            report["database_connections_after_dependency_pressure"] = connections
            assert connections <= 4, connections
            report["database_connections_before_exit"] = connections
            report["runtime_metrics"] = json.loads(request("/__ku/metrics", control=True))
            assert len(report["runtime_metrics"]["loops"]) == args.loops
            assert all(loop["requests"] for loop in report["runtime_metrics"]["loops"])
            database_metrics = report["runtime_metrics"]["database"]
            assert database_metrics["reserved_connections"] == 4
            assert database_metrics["reserved_waiters"] == 8
            assert database_metrics["pending_operations"] == 0
            assert database_metrics["pending_bytes"] == 0
            if not args.performance_seconds:
                assert database_metrics["confirmed_cancellations"] > 0
                report["checks"]["cancel_confirmed_and_local_charge_retired"] = database_metrics["confirmed_cancellations"]
            request("/__ku/drain", method="POST", control=True)
            try:
                service.wait(timeout=5)
            except subprocess.TimeoutExpired:
                service.kill()
                service.wait(timeout=3)
                raise RuntimeError("service did not terminate within 5 seconds")
            assert service.returncode == 0, service.returncode
            report["checks"]["normal_drain_exit"] = service.returncode
            assert sql("SELECT count(*) FROM pg_stat_activity WHERE application_name='ku_runtime_acceptance'") == "0"
            report["checks"]["database_connections_after_exit"] = 0
            report["status"] = "passed"
        except Exception as error:
            report["status"] = "failed"
            report["failure"] = f"{type(error).__name__}: {error}"
            if logs.is_file():
                report["service_log_tail"] = logs.read_text(encoding="utf-8", errors="replace")[-8192:]
            raise
        finally:
            if service and service.poll() is None:
                service.kill()
                service.wait(timeout=5)
            if proxy:
                proxy.close()
            if started:
                run([tools / "pg_ctl", "-D", data, "-w", "-t", "15", "stop", "-m", "immediate"])
            args.output.parent.mkdir(parents=True, exist_ok=True)
            args.output.write_text(json.dumps(report, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")
    print(json.dumps(report, ensure_ascii=False))


if __name__ == "__main__":
    main()
