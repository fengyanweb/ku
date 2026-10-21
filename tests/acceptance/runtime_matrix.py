#!/usr/bin/env python3
"""Fixed 1/2/4-loop native acceptance with bounded, private local fixtures.

This is a short runtime matrix, not a replacement for a formal sustained load
test. The plan is written before building or sending traffic. HTTP bodies,
histograms, diagnostic samples, client threads and retained sockets are bounded.
No existing database, service configuration or website is accessed.
"""
import argparse
import concurrent.futures
import hashlib
import http.client
import json
import math
import os
from pathlib import Path
import platform
import queue
import socket
import subprocess
import sys
import tempfile
import threading
import time

sys.dont_write_bytecode = True
from multiloop_load import Results, Service, atomic_json, process_sample, unused_port


PLAN = {
    "event_loops": [1, 2, 4], "blocking_threads_total": 5,
    "http_blocking_threads": 4, "task_blocking_threads": 1, "thread_budget": 16,
    "idle_seconds": 3, "cpu_seconds": 5, "cpu_client_concurrency": 8,
    "fairness_seconds": 5, "fairness_arrivals_per_second": 50,
    "fairness_client_threads": 16, "client_queue_capacity": 64,
    "client_keepalive_idle_limit_ms": 500,
    "http_waiting_connections": 64, "timer_waiting_requests": 32,
    "slow_body_connections": 4, "slow_reader_connections": 1,
    "slow_response_bytes": 6291456, "cancel_cycles": 3, "cancel_per_cycle": 24,
    "blocking_requests": 6, "blocking_hold_after_timeout_ms": 100,
    "handler_timeout_ms": 1000, "body_timeout_ms": 1000, "write_timeout_ms": 1000,
    "recovery_deadline_seconds": 5, "unexpected_response_limit": 0,
    "ordinary_and_timer_p99_limit_ms": 250, "idle_one_core_cpu_limit_percent": 5,
    "rss_recovered_growth_limit_bytes": 16777216,
    "descriptor_recovered_growth_limit": 2,
    "windows_resource_initialization_cycles": 4,
    "windows_resource_initialization_concurrency": 16,
    "meaning": {
        "throughput": "Observed short closed-loop throughput, no linear-scaling claim or minimum speedup gate.",
        "waiting": "Accepted incomplete HTTP bodies and timer Task waits are reported separately. Neither proves Task TCP await.",
        "blocking": "HTTP checks actual running jobs and charged output after timeout. Pool-full and stale-generation checks use the existing controlled native regression when supplied.",
        "cancel": "Black-box disconnect/deadline/reuse evidence; exact internal stale generations are covered by the controlled native regression.",
        "rss": "Allocator retained capacity may remain within the declared 16 MiB delta; RSS is not exact per-Task memory accounting.",
        "windows_handles": "Retain cold counters and first-use costs. Three same-process repeat cycles and the full matrix must stay within the unchanged +2 handle limit of the first recovered cycle; failed samples never become zero counters.",
    },
}

EXTRA_HANDLERS = """
async fn matrix_blocking(): HttpResponse! {
    path = fs.read("block.path")?
    text = fs.read(path)?
    return ok(http.text(text))
}

fn matrix_large(): HttpResponse! {
    text = fs.read("large.txt")?
    text = text + text
    text = text + text
    text = text + text
    return ok(http.text(text))
}

fn matrix_resource_sync(): HttpResponse! {
    i = 0
    total = 0
    while (i < 500000) { total = total + i; i = i + 1 }
    if (total != 124999750000) { fail "incorrect resource probe calculation" }
    text = fs.read("resource.txt")?
    return ok(http.text(text))
}

async fn matrix_resource_file(): HttpResponse! {
    text = fs.read("resource.txt")?
    return ok(http.text(text))
}

"""


def machine_environment():
    value = {"machine": platform.machine(), "logical_cpus": os.cpu_count(),
             "transport": "private loopback HTTP, no TLS or external proxy",
             "database": "not accessed by this matrix", "native_c_compiler_override": os.environ.get("CC")}
    if hasattr(os, "sched_getaffinity"):
        value["load_generator_cpu_affinity"] = sorted(os.sched_getaffinity(0))
    if platform.system() == "Darwin":
        result = subprocess.run(["sysctl", "-n", "hw.memsize", "hw.model", "machdep.cpu.brand_string"],
                                capture_output=True, text=True, timeout=3)
        parts = result.stdout.splitlines()
        value["sysctl_hardware"] = dict(zip(("physical_memory_bytes", "model", "cpu"), parts))
        value["cpu_quota"] = "no cgroup quota on this native macOS host; competing host work is not excluded by the OS"
    elif platform.system() == "Linux":
        value["cgroup_files"] = {}
        base = Path("/sys/fs/cgroup")
        try:
            for line in Path("/proc/self/cgroup").read_text().splitlines():
                if line.startswith("0::"):
                    location = base / line[3:].lstrip("/")
                    if location.is_dir():
                        base = location
                    break
        except OSError:
            pass
        for name in ("cpu.max", "memory.max", "cpuset.cpus.effective"):
            try:
                value["cgroup_files"][str(base / name)] = (base / name).read_text()[:256].strip()
            except OSError:
                pass
        try:
            value["physical_memory_bytes"] = os.sysconf("SC_PHYS_PAGES") * os.sysconf("SC_PAGE_SIZE")
        except (ValueError, OSError):
            pass
    else:
        value["cpu_quota"] = "not measured by this driver; CI/host resource metadata must be retained separately"
    return value


def cpu_sample(pid):
    value = process_sample(pid)
    try:
        if platform.system() == "Linux":
            tail = Path(f"/proc/{pid}/stat").read_text().rsplit(")", 1)[1].split()
            value["cpu_seconds"] = (int(tail[11]) + int(tail[12])) / os.sysconf("SC_CLK_TCK")
        elif "cpu_time" in value:
            seconds = 0.0
            for part in value["cpu_time"].split(":"):
                seconds = seconds * 60 + float(part)
            value["cpu_seconds"] = seconds
    except (OSError, ValueError, IndexError):
        value["cpu_sample_error"] = True
    return value


def resource_sample_errors(sample, descriptor_key, require_cpu=False):
    errors = []
    if sample.get("sample_error") or sample.get("cpu_sample_error"):
        errors.append("sample reports an error")
    for field in ("rss_bytes", descriptor_key, "threads"):
        value = sample.get(field)
        if not isinstance(value, int) or isinstance(value, bool) or value < 0:
            errors.append(f"missing or invalid {field}")
    if require_cpu:
        value = sample.get("cpu_seconds")
        if not isinstance(value, (int, float)) or isinstance(value, bool) or not math.isfinite(value) or value < 0:
            errors.append("missing or invalid cpu_seconds")
    return errors


def resource_recovery_check(recovery, baseline, final, descriptor_key):
    errors = {name: resource_sample_errors(sample, descriptor_key)
              for name, sample in (("baseline", baseline), ("final", final))}
    errors = {name: value for name, value in errors.items() if value}
    rss_delta = final["rss_bytes"] - baseline["rss_bytes"] if not errors else None
    descriptor_delta = final[descriptor_key] - baseline[descriptor_key] if not errors else None
    return {"recovery": recovery, "baseline": baseline, "final": final,
            "sample_errors": errors, "rss_delta_bytes": rss_delta, "descriptor_delta": descriptor_delta,
            "passed": not errors and recovery["passed"]
                and rss_delta <= PLAN["rss_recovered_growth_limit_bytes"]
                and descriptor_delta <= PLAN["descriptor_recovered_growth_limit"] and final["threads"] <= 16}


def windows_resource_initialization(service, cold):
    """Keep first-use costs visible, then test three repeats in this same child."""
    cycles = []
    first = None
    for index in range(PLAN["windows_resource_initialization_cycles"]):
        stages = []
        for path, expected in (("/resource-sync", b"file-ready"),
                               ("/timer", b"timer-ready"), ("/resource-file", b"file-ready")):
            with concurrent.futures.ThreadPoolExecutor(max_workers=PLAN["windows_resource_initialization_concurrency"]) as executor:
                probes = list(executor.map(lambda _: call(service, path, expected),
                                           range(PLAN["windows_resource_initialization_concurrency"])))
            recovery = recover(service)
            resources = cpu_sample(service.child.pid)
            stages.append({"path": path, "probes": probes, "recovery": recovery, "resources": resources,
                           "passed": all(probe[0] == "success" for probe in probes) and recovery["passed"]
                               and not resource_sample_errors(resources, "handles")})
        final = stages[-1]["resources"]
        if first is None:
            first = final
        comparison = resource_recovery_check(stages[-1]["recovery"], first, final, "handles")
        cycles.append({"cycle": index, "stages": stages, "recovered_resources": final,
                       "compared_to_first_recovered_cycle": comparison,
                       "passed": all(stage["passed"] for stage in stages) and comparison["passed"]})
    cold_comparison = resource_recovery_check({"passed": True}, cold, first, "handles")
    # The cold handle delta is reported, not relabelled as a released resource.
    # It cannot explain repeat growth: every later cycle and the actual matrix
    # still uses the original +2 limit. Keep the original cold RSS bound too.
    cold_valid = not cold_comparison["sample_errors"] and cold_comparison["rss_delta_bytes"] <= PLAN["rss_recovered_growth_limit_bytes"]
    cold_comparison.pop("passed")
    cold_comparison["scope"] = "First-use costs, not a claim that cold handles were released; repeat and final recovery use the unchanged +2 limit."
    return {"cold": cold, "cold_to_first_recovered_cycle": cold_comparison, "cycles": cycles,
            "baseline_for_repeated_work": first,
            "passed": cold_valid and all(cycle["passed"] for cycle in cycles)}


class MatrixService(Service):
    """Reuse the existing control/metrics/drain client with one fs worker reserve."""
    def __init__(self, executable, root, loops, management=None, wait_ready=True):
        self.port, self.management = unused_port(), management or unused_port()
        self.token = "matrix-internal-token"
        (root / "http.addr").write_text(f"127.0.0.1:{self.port}", encoding="ascii")
        environment = os.environ | {
            "KU_EVENT_LOOPS": str(loops), "KU_BLOCKING_THREADS": "5", "KU_THREAD_BUDGET": "16",
            "KU_SERVICE_CONTROL_TOKEN": self.token,
            "KU_SERVICE_PROBE_ADDR": f"127.0.0.1:{self.management}",
            "KU_TASK_MAX_BYTES": "16777216",
        }
        self.log_path = root / f"matrix-service-{loops}-{time.monotonic_ns()}.log"
        self.log = self.log_path.open("wb")
        self.child = subprocess.Popen([str(executable)], cwd=root, env=environment,
                                      stdout=self.log, stderr=self.log)
        if not wait_ready:
            return
        try:
            deadline = time.monotonic() + 5
            while time.monotonic() < deadline:
                try:
                    status, body = self.control("/__ku/health/ready")
                    if status == 200 and body == self.token.encode():
                        return
                except (OSError, http.client.HTTPException):
                    pass
                if self.child.poll() is not None:
                    break
                time.sleep(.02)
            raise RuntimeError("matrix native service did not become ready: " + self.log_tail())
        except BaseException:
            self.stop()
            raise

    def log_tail(self):
        if not self.log.closed:
            self.log.flush()
        with self.log_path.open("rb") as handle:
            handle.seek(max(0, self.log_path.stat().st_size - 8192))
            return handle.read(8192).decode("utf-8", errors="replace")


class Recorder:
    def __init__(self):
        self.all = Results()
        self.routes = {}
        self.lock = threading.Lock()

    def add(self, path, category, elapsed, detail=None):
        with self.lock:
            result = self.routes.setdefault(path, Results())
        for result in (self.all, result):
            with result.lock:
                result.arrivals += 1
                result.inflight += 1
                result.peak_inflight = max(result.peak_inflight, result.inflight)
            result.add(category, elapsed, detail)

    def value(self):
        return {"all": self.all.value(), "routes": {path: value.value() for path, value in self.routes.items()}}


def call(service, path, expected=None, persistence=None, timeout=3):
    holder = persistence if persistence is not None else {}
    connection = holder.get("connection")
    # A keep-alive client must retire its idle socket before the fixture's
    # existing 1s idle deadline. Otherwise an already queued unsolicited 408
    # can be misattributed to the next request on that connection.
    if connection is not None and time.monotonic() - holder.get("last_used", 0) >= PLAN["client_keepalive_idle_limit_ms"] / 1000:
        connection.close()
        connection = None
    if connection is None:
        connection = http.client.HTTPConnection("127.0.0.1", service.port, timeout=timeout)
        holder["connection"] = connection
    began = time.monotonic()
    try:
        connection.request("GET", path, headers={"Connection": "keep-alive" if persistence is not None else "close"})
        response = connection.getresponse()
        body = response.read(65537)
        category = "success" if response.status == 200 else f"http_{response.status}"
        if len(body) > 65536:
            category = "response_too_large"
        elif expected is not None and response.status == 200 and body != expected:
            category = "wrong_body"
        detail = None if category == "success" else {"status": response.status, "body": repr(body[:160])}
        return category, time.monotonic() - began, detail
    except (OSError, http.client.HTTPException) as error:
        connection.close()
        holder.pop("connection", None)
        return "client_timeout" if isinstance(error, TimeoutError) else "connection_error", time.monotonic() - began, {"error": type(error).__name__}
    finally:
        if persistence is None:
            connection.close()
        else:
            holder["last_used"] = time.monotonic()


def zero_inflight(metrics):
    for row in metrics.get("loops", []):
        if any(row.get(field, 0) for field in ("queued", "running", "resident", "connections", "pending")):
            return False
    for row in metrics.get("tasks", []):
        if any(row.get(field, 0) for field in ("ready", "waiting", "running", "resident", "timers")):
            return False
    blocking = metrics.get("task_blocking", {})
    return not any(blocking.get(field, 0) for field in ("resident", "queued", "running", "output_charge_bytes", "net_wait", "net_step"))


def recover(service, baseline=None):
    began = time.monotonic()
    last = {}
    while time.monotonic() - began < PLAN["recovery_deadline_seconds"]:
        last = service.metrics()
        if zero_inflight(last) and last.get("buffer_bytes", 0) <= (baseline or {}).get("buffer_bytes", 0):
            return {"passed": True, "seconds": time.monotonic() - began, "metrics": last}
        time.sleep(.02)
    return {"passed": False, "seconds": time.monotonic() - began, "metrics": last}


def accepted(metrics):
    return sum(row.get("accepted", 0) for row in metrics.get("loops", []))


def wait_metric(service, predicate, seconds=1):
    deadline = time.monotonic() + seconds
    latest = service.metrics()
    while not predicate(latest) and time.monotonic() < deadline:
        time.sleep(.01)
        latest = service.metrics()
    return latest


def socket_request(service, path, receive_buffer=None, body_prefix=None):
    peer = socket.socket()
    if receive_buffer:
        peer.setsockopt(socket.SOL_SOCKET, socket.SO_RCVBUF, receive_buffer)
    peer.settimeout(2)
    try:
        peer.connect(("127.0.0.1", service.port))
        if body_prefix is not None:
            peer.sendall(b"POST /echo HTTP/1.1\r\nHost: localhost\r\nContent-Length: 4000\r\nConnection: close\r\n\r\n" + body_prefix)
        else:
            peer.sendall(f"GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n".encode("ascii"))
        return peer
    except BaseException:
        peer.close()
        raise


def read_status(peer, limit=65536):
    data = bytearray()
    try:
        while len(data) < limit:
            block = peer.recv(min(8192, limit - len(data)))
            if not block:
                break
            data.extend(block)
    except socket.timeout:
        return {"category": "client_timeout", "bytes": len(data)}
    except OSError as error:
        return {"category": "connection_error", "error": type(error).__name__, "bytes": len(data)}
    first = bytes(data).split(b"\r\n", 1)[0]
    try:
        status = int(first.split()[1])
    except (IndexError, ValueError):
        return {"category": "closed_without_response", "bytes": len(data)}
    return {"category": f"http_{status}", "bytes": len(data), "body_tail": repr(bytes(data[-80:]))}


def closed_cpu(service):
    recorder = Recorder()
    stop = threading.Event()
    began = time.monotonic()
    def worker():
        holder = {}
        try:
            while not stop.is_set():
                recorder.add("/cpu", *call(service, "/cpu", b"cpu-ready", holder))
        finally:
            if holder.get("connection"):
                holder["connection"].close()
    before = cpu_sample(service.child.pid)
    client_before = time.process_time()
    threads = [threading.Thread(target=worker) for _ in range(PLAN["cpu_client_concurrency"])]
    for thread in threads:
        thread.start()
    stop.wait(PLAN["cpu_seconds"])
    stop.set()
    for thread in threads:
        thread.join(timeout=4)
    if any(thread.is_alive() for thread in threads):
        raise RuntimeError("CPU load client did not terminate")
    elapsed = time.monotonic() - began
    result = recorder.value()
    result.update(seconds=elapsed, requests_per_second=result["all"]["completed"] / elapsed,
                  service_before=before, service_after=cpu_sample(service.child.pid),
                  generator_cpu_seconds=time.process_time() - client_before, metrics=service.metrics())
    result["passed"] = result["all"]["counts"] == {"success": result["all"]["completed"]} and result["all"]["completed"] > 0
    return result


def fair_load(service):
    work = queue.Queue(maxsize=PLAN["client_queue_capacity"])
    stopping = threading.Event()
    recorder = Recorder()
    client_shed = 0
    choices = [("/ok", b"ok-ready"), ("/timer", b"timer-ready"), ("/hot", b"hot-ready"), ("/cpu", b"cpu-ready"), ("/timer", b"timer-ready")]
    def worker():
        holder = {}
        try:
            while not stopping.is_set() or not work.empty():
                try:
                    path, expected = work.get(timeout=.1)
                except queue.Empty:
                    continue
                recorder.add(path, *call(service, path, expected, holder))
                work.task_done()
        finally:
            if holder.get("connection"):
                holder["connection"].close()
    threads = [threading.Thread(target=worker) for _ in range(PLAN["fairness_client_threads"])]
    for thread in threads:
        thread.start()
    began = time.monotonic()
    count = PLAN["fairness_seconds"] * PLAN["fairness_arrivals_per_second"]
    for index in range(count):
        target = began + index / PLAN["fairness_arrivals_per_second"]
        delay = target - time.monotonic()
        if delay > 0:
            time.sleep(delay)
        try:
            work.put_nowait(choices[index % len(choices)])
        except queue.Full:
            client_shed += 1
    stopping.set()
    for thread in threads:
        thread.join(timeout=4)
    if any(thread.is_alive() for thread in threads):
        raise RuntimeError("fairness load client did not terminate")
    result = recorder.value()
    result.update(scheduled_arrivals=count, client_shed=client_shed, seconds=time.monotonic() - began, metrics=service.metrics())
    result["passed"] = client_shed == 0 and result["all"]["completed"] == count and result["all"]["counts"] == {"success": count}
    for path in ("/ok", "/timer"):
        result["passed"] &= result["routes"].get(path, {}).get("p99_ms", float("inf")) <= PLAN["ordinary_and_timer_p99_limit_ms"]
    return result


def wait_connections(service):
    held = []
    before = service.metrics()
    resources_before = cpu_sample(service.child.pid)
    try:
        for _ in range(PLAN["http_waiting_connections"]):
            held.append(socket_request(service, "/echo", body_prefix=b"x"))
        snapshot = wait_metric(service, lambda value: sum(row["connections"] for row in value["loops"]) >= len(held), .4)
        during = cpu_sample(service.child.pid)
        visible = sum(row["connections"] for row in snapshot["loops"])
        probe = call(service, "/timer", b"timer-ready")
        result = {"client_established": len(held), "server_connections": visible,
                  "accepted_delta": accepted(snapshot) - accepted(before), "metrics": snapshot,
                  "resources_before": resources_before, "resources_waiting": during,
                  "ordinary_probe": probe,
                  "classification": "HTTP body sockets awaiting bytes; no per-socket Task network-await claim"}
        result["passed"] = visible >= len(held) and result["accepted_delta"] >= len(held) and probe[0] == "success" and during.get("threads", 17) <= 16
    finally:
        for peer in held:
            peer.close()
    result["recovery"] = recover(service)
    result["passed"] &= result["recovery"]["passed"]
    return result


def timer_waits(service):
    held = []
    try:
        for _ in range(PLAN["timer_waiting_requests"]):
            held.append(socket_request(service, "/cancel"))
        metrics = wait_metric(service, lambda value: sum(row["timers"] for row in value["tasks"]) >= len(held), .5)
        timers = sum(row["timers"] for row in metrics["tasks"])
        probe = call(service, "/ok", b"ok-ready")
        result = {"clients": len(held), "actual_timer_registrations": timers, "metrics": metrics,
                  "resources": cpu_sample(service.child.pid), "ordinary_probe": probe,
                  "passed": timers >= len(held) and probe[0] == "success"}
    finally:
        for peer in held:
            peer.close()
    result["recovery"] = recover(service)
    result["passed"] &= result["recovery"]["passed"]
    return result


def slow_io(service):
    slow_bodies, slow_readers = [], []
    try:
        for _ in range(PLAN["slow_body_connections"]):
            slow_bodies.append(socket_request(service, "/echo", body_prefix=b"x"))
        for _ in range(PLAN["slow_reader_connections"]):
            slow_readers.append(socket_request(service, "/large", receive_buffer=1024))
        metrics = wait_metric(service, lambda value: value["buffer_bytes"] >= PLAN["slow_response_bytes"], .5)
        ordinary = [call(service, "/timer", b"timer-ready"), call(service, "/ok", b"ok-ready")]
        time.sleep(1.1)
        bodies = [read_status(peer) for peer in slow_bodies]
        # Reading a response here would remove the intended backpressure. The
        # output buffer release and connection cleanup are the server evidence.
        after = service.metrics()
        result = {"slow_bodies": bodies, "slow_reader_clients": len(slow_readers),
                  "during": metrics, "after_write_deadline": after, "ordinary_probes": ordinary,
                  "passed": all(item["category"] == "http_408" for item in bodies)
                      and all(item[0] == "success" and item[1] * 1000 <= 250 for item in ordinary)
                      and metrics["buffer_bytes"] <= 8388608
                      and after["buffer_bytes"] < metrics["buffer_bytes"]}
    finally:
        for peer in slow_bodies + slow_readers:
            peer.close()
    result["recovery"] = recover(service)
    result["passed"] &= result["recovery"]["passed"]
    return result


def cancel_reuse(service):
    cycles = []
    for index in range(PLAN["cancel_cycles"]):
        held = []
        try:
            for _ in range(PLAN["cancel_per_cycle"]):
                held.append(socket_request(service, "/cancel"))
            observed = wait_metric(service, lambda value: sum(row["timers"] for row in value["tasks"]) >= len(held), .4)
        finally:
            for peer in held:
                peer.close()
        # New connections exercise fd/connection-slot reuse while old timer
        # cancellations and completion notifications may still be retiring.
        probes = [call(service, "/ok", b"ok-ready") for _ in range(8)]
        recovery = recover(service)
        cycles.append({"cycle": index, "before_disconnect": observed, "new_connection_probes": probes, "recovery": recovery,
                       "passed": all(item[0] == "success" for item in probes) and recovery["passed"]})
    deadline_response = call(service, "/cancel")
    return {"cycles": cycles, "deadline_response": deadline_response, "after_deadline": recover(service),
            "passed": all(item["passed"] for item in cycles) and deadline_response[0] == "http_504"
                and .8 <= deadline_response[1] <= 2.0}


class BlockingGate:
    """A private real file/pipe read held in the kernel until release()."""
    def __init__(self, root):
        self.fd = None
        self.thread = None
        self.error = None
        self.release_event = threading.Event()
        self.connected = threading.Event()
        if os.name != "nt":
            self.path = str(root / "blocking.fifo")
            os.mkfifo(self.path)
            self.fd = os.open(self.path, os.O_RDWR | os.O_NONBLOCK)
        else:
            import ctypes
            from ctypes import wintypes
            self.ctypes = ctypes
            self.kernel = ctypes.WinDLL("kernel32", use_last_error=True)
            self.kernel.CreateNamedPipeW.restype = wintypes.HANDLE
            self.kernel.CreateNamedPipeW.argtypes = [wintypes.LPCWSTR, wintypes.DWORD, wintypes.DWORD, wintypes.DWORD,
                                                    wintypes.DWORD, wintypes.DWORD, wintypes.DWORD, wintypes.LPVOID]
            self.kernel.ConnectNamedPipe.argtypes = [wintypes.HANDLE, wintypes.LPVOID]
            self.kernel.WriteFile.argtypes = [wintypes.HANDLE, wintypes.LPCVOID, wintypes.DWORD,
                                             ctypes.POINTER(wintypes.DWORD), wintypes.LPVOID]
            self.kernel.CloseHandle.argtypes = [wintypes.HANDLE]
            self.path = "\\\\.\\pipe\\ku-runtime-matrix-" + str(os.getpid()) + "-" + str(time.monotonic_ns())
            self.handle = self.kernel.CreateNamedPipeW(self.path, 2, 0, 1, 4096, 4096, 0, None)
            if self.handle == wintypes.HANDLE(-1).value:
                raise OSError(ctypes.get_last_error(), "CreateNamedPipeW")
            def serve():
                try:
                    connected = self.kernel.ConnectNamedPipe(self.handle, None)
                    if not connected and ctypes.get_last_error() != 535:
                        raise OSError(ctypes.get_last_error(), "ConnectNamedPipe")
                    self.connected.set()
                    self.release_event.wait(8)
                    count = wintypes.DWORD()
                    if not self.kernel.WriteFile(self.handle, b"released", 8, ctypes.byref(count), None):
                        raise OSError(ctypes.get_last_error(), "WriteFile")
                except OSError as error:
                    self.error = str(error)
                finally:
                    self.kernel.CloseHandle(self.handle)
            self.thread = threading.Thread(target=serve, daemon=True)
            self.thread.start()
        (root / "block.path").write_text(self.path, encoding="utf-8")

    def release(self):
        if self.fd is not None:
            os.write(self.fd, b"released")
            os.close(self.fd)
            self.fd = None
            Path(self.path).unlink(missing_ok=True)
        if self.thread is not None:
            self.release_event.set()
            if not self.connected.is_set():
                # Wake a fixture that failed before the runtime opened the pipe.
                try:
                    with open(self.path, "rb", buffering=0):
                        pass
                except OSError:
                    pass
            self.thread.join(timeout=3)
            if self.thread.is_alive():
                raise RuntimeError("private named-pipe fixture did not terminate")


def blocking_lifetime(service, root):
    gate = BlockingGate(root)
    held = []
    try:
        for _ in range(PLAN["blocking_requests"]):
            held.append(socket_request(service, "/blocking"))
        running = wait_metric(service, lambda value: value.get("task_blocking", {}).get("running") == 1
                              and value["task_blocking"]["resident"] >= len(held), .7)
        observed_at = time.monotonic()
        responses = [read_status(peer) for peer in held]
        # Wait only 100 ms after the HTTP timeout, within the existing cleanup
        # grace. Native work must remain charged until the FIFO actually exits.
        time.sleep(PLAN["blocking_hold_after_timeout_ms"] / 1000)
        after_timeout = service.metrics()
        probe = call(service, "/ok", b"ok-ready")
        pool = after_timeout.get("task_blocking", {})
        result = {"before_timeout": running, "after_http_timeout": after_timeout,
                  "held_seconds_after_running_observed": time.monotonic() - observed_at,
                  "responses": responses, "ordinary_probe": probe,
                  "outer_http_active_limit": 128, "native_pool_capacity": 1024,
                  "pool_full_claim": False,
                  "passed": running.get("task_blocking", {}).get("running") == 1
                      and all(item["category"] == "http_504" for item in responses)
                      and pool.get("running") == 1 and pool.get("cancelled_still_running") == 1
                      and pool.get("queued") == 0 and pool.get("resident") == 1
                      and pool.get("output_charge_bytes", 0) >= 4096
                      and pool.get("workers_created") == 1 and probe[0] == "success"}
    finally:
        gate.release()
        for peer in held:
            peer.close()
    result["fixture_error"] = gate.error
    result["recovery"] = recover(service)
    result["passed"] &= result["recovery"]["passed"] and gate.error is None
    return result


def startup_failure(executable, root, loops):
    occupied = socket.socket()
    occupied.bind(("127.0.0.1", 0))
    occupied.listen(1)
    service = None
    try:
        service = MatrixService(executable, root, loops, occupied.getsockname()[1], wait_ready=False)
        try:
            status = service.child.wait(timeout=5)
        except subprocess.TimeoutExpired:
            service.child.kill()
            service.child.wait(timeout=3)
            status = "did_not_exit"
        business_released = False
        with socket.socket() as probe:
            try:
                probe.bind(("127.0.0.1", service.port))
                business_released = True
            except OSError:
                pass
        return {"exit": status, "business_listener_released": business_released,
                "log_tail": service.log_tail(), "injected_failure": "management listener bind after service resources begin startup",
                "passed": isinstance(status, int) and status != 0 and business_released}
    finally:
        occupied.close()
        if service:
            service.stop()


def normal_drain(service):
    peer = socket_request(service, "/timer")
    began = time.monotonic()
    try:
        before = wait_metric(service, lambda value: sum(row["timers"] for row in value["tasks"]) > 0, .02)
        status, body = service.control("/__ku/drain", "POST")
        response = read_status(peer)
        try:
            exit_code = service.child.wait(timeout=5)
        except subprocess.TimeoutExpired:
            exit_code = "drain_deadline_exceeded"
        return {"before": before, "drain_status": status, "active_response": response,
                "exit": exit_code, "seconds": time.monotonic() - began,
                "passed": status == 200 and body == service.token.encode() and exit_code == 0
                    and response["category"] == "http_200" and "timer-ready" in response.get("body_tail", "")}
    finally:
        peer.close()


def controlled_blocking_regression(binary):
    name = "native_task_blocking_write_tracks_real_jobs_budget_and_original_cleanup_deadline"
    result = subprocess.run([str(binary.resolve()), "--exact", name, "--nocapture"],
                            capture_output=True, text=True, timeout=90)
    return {"test": name, "binary_sha256": hashlib.sha256(binary.read_bytes()).hexdigest(),
            "exit": result.returncode, "stdout_tail": result.stdout[-6000:], "stderr_tail": result.stderr[-6000:],
            "passed": result.returncode == 0 and "1 passed" in result.stdout,
            "scope": "Controlled native job held inside the file adapter by a fixture event: capacity 2, one running plus one queued, full rejection, cancelled queued call never entered, stale generation rejected, late running work retains original budget/deadline. Actual kernel FIFO/pipe read lifetime is checked separately by the HTTP case."}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--ku", required=True, type=Path)
    parser.add_argument("--output", required=True, type=Path)
    parser.add_argument("--loops", type=int, nargs="+", choices=[1, 2, 4], default=[1, 2, 4])
    parser.add_argument("--profile", choices=["debug", "release"], default="debug", help="C build profile, recorded before running; use release for final acceptance")
    parser.add_argument("--prepare-only", action="store_true", help="Freeze plan and fixture hash; send no traffic and do not build")
    parser.add_argument("--build-only", action="store_true", help="Build private fixture and clean it without starting a service")
    parser.add_argument("--cpu-only", action="store_true", help="Short CPU comparison only; never reports complete matrix acceptance")
    parser.add_argument("--blocking-regression", type=Path, help="Current compiled native_task_adapter_test binary for actual pool-full/stale generation evidence")
    args = parser.parse_args()
    fixture = Path(__file__).resolve().parents[1] / "fixtures/multiloop_service.ku"
    source = fixture.read_text(encoding="utf-8")
    assert source.count("fn main(): null! {") == 1 and source.count("    app.listen(address)?") == 1
    source = source.replace("fn main(): null! {", EXTRA_HANDLERS + "fn main(): null! {")
    source = source.replace("    app.listen(address)?", '    app.get("/blocking", matrix_blocking)\n    app.get("/large", matrix_large)\n    app.get("/resource-sync", matrix_resource_sync)\n    app.get("/resource-file", matrix_resource_file)\n    app.listen(address)?')
    report = {"status": "prepared", "platform": platform.platform(), "python": platform.python_version(),
              "command": sys.argv, "host_logical_cpus": os.cpu_count(),
              "environment": machine_environment(),
              "git_commit": subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=fixture.parents[2], text=True, timeout=3).strip(),
              "working_tree_is_modified": bool(subprocess.check_output(["git", "status", "--porcelain"], cwd=fixture.parents[2], text=True, timeout=3).strip()),
              "cli_sha256": hashlib.sha256(args.ku.read_bytes()).hexdigest(),
              "fixture_sha256": hashlib.sha256(source.encode()).hexdigest(), "plan": dict(PLAN, native_c_profile=args.profile),
              "selected_loops": args.loops, "cpu_profile_only": args.cpu_only, "runs": {}, "not_claimed": [
                  "formal 30-minute acceptance", "historical 504 root cause", "database or output revalidation",
                  "Task TCP waits from HTTP body sockets", "linear throughput scaling", "website publication"]}
    atomic_json(args.output, report)
    if args.prepare_only:
        return
    report["status"] = "running"
    try:
        with tempfile.TemporaryDirectory(prefix="ku-runtime-matrix-") as temporary:
            root = Path(temporary)
            program = root / "matrix.ku"
            program.write_text(source, encoding="utf-8")
            # fs.read has an existing 1,000,000-byte limit. The response is
            # intentionally larger than a TCP sender buffer without changing it.
            (root / "large.txt").write_bytes(b"x" * (PLAN["slow_response_bytes"] // 8))
            (root / "resource.txt").write_text("file-ready", encoding="ascii")
            executable = root / ("matrix.exe" if os.name == "nt" else "matrix")
            profile_arguments = ["--release"] if args.profile == "release" else ["--profile", "debug"]
            build = subprocess.run([str(args.ku.resolve()), "build", *profile_arguments, "--backend", "c", "-o", str(executable), str(program)],
                                   cwd=root, capture_output=True, text=True, timeout=120)
            report["build"] = {"exit": build.returncode, "stdout_tail": build.stdout[-4000:], "stderr_tail": build.stderr[-4000:]}
            atomic_json(args.output, report)
            if build.returncode:
                raise RuntimeError("matrix fixture build failed")
            report["native_sha256"] = hashlib.sha256(executable.read_bytes()).hexdigest()
            report["generated_c_sha256"] = {str(path.relative_to(root)): hashlib.sha256(path.read_bytes()).hexdigest()
                                              for path in root.rglob("matrix.c")}
            if args.build_only:
                report["status"] = "build_passed"
                atomic_json(args.output, report)
                return
            for loops in args.loops:
                run = {"status": "running", "checks": {}}
                report["runs"][str(loops)] = run
                service = None
                try:
                    run["checks"]["A13_partial_startup"] = startup_failure(executable, root, loops)
                    service = MatrixService(executable, root, loops)
                    run["baseline_metrics"] = service.metrics()
                    baseline = cpu_sample(service.child.pid)
                    run["cold_resources"] = baseline
                    before = cpu_sample(service.child.pid)
                    time.sleep(PLAN["idle_seconds"])
                    after = cpu_sample(service.child.pid)
                    wall = after["monotonic"] - before["monotonic"]
                    idle_errors = resource_sample_errors(before, "handles" if os.name == "nt" else "fds", True)
                    idle_errors += resource_sample_errors(after, "handles" if os.name == "nt" else "fds", True)
                    cpu = (after["cpu_seconds"] - before["cpu_seconds"]) / wall * 100 if not idle_errors and wall > 0 else None
                    run["checks"]["idle"] = {"before": before, "after": after, "one_core_cpu_percent": cpu,
                                                "sample_errors": idle_errors,
                                                "passed": not idle_errors and cpu is not None and cpu <= PLAN["idle_one_core_cpu_limit_percent"] and after["threads"] <= 16}
                    if os.name == "nt" and not args.cpu_only:
                        initialization = windows_resource_initialization(service, baseline)
                        run["checks"]["A18_windows_first_use_and_repeats"] = initialization
                        baseline = initialization["baseline_for_repeated_work"]
                    atomic_json(args.output, report)
                    for name, action in [
                        ("cpu", lambda: closed_cpu(service)), ("fairness", lambda: fair_load(service)),
                        ("http_real_waits", lambda: wait_connections(service)),
                        ("timer_real_waits", lambda: timer_waits(service)),
                        ("A03_slow_io", lambda: slow_io(service)),
                        ("A05_cancel_reuse", lambda: cancel_reuse(service)),
                        ("A08_blocking_lifetime", lambda: blocking_lifetime(service, root)),
                    ]:
                        if args.cpu_only and name != "cpu":
                            continue
                        run["checks"][name] = action()
                        run["checks"][name]["resources_after"] = cpu_sample(service.child.pid)
                        atomic_json(args.output, report)
                    recovery = recover(service, run["baseline_metrics"])
                    final = cpu_sample(service.child.pid)
                    descriptor_key = "handles" if os.name == "nt" else "fds"
                    run["checks"]["A18_recovery"] = resource_recovery_check(recovery, baseline, final, descriptor_key)
                    metrics = service.metrics()
                    run["checks"]["all_loops_worked"] = {
                        "loops": metrics["loops"], "tasks": metrics["tasks"],
                        "passed": len(metrics["loops"]) == loops and all(item["requests"] > 0 for item in metrics["loops"])
                            and all(item["polls"] > 0 for item in metrics["tasks"])}
                    run["checks"]["A13_normal_drain"] = normal_drain(service)
                finally:
                    if service:
                        run["normal_drain_exit"] = service.stop()
                        run["log_tail"] = service.log_path.read_bytes()[-8192:].decode("utf-8", errors="replace")
                run["status"] = "passed" if all(item.get("passed") for item in run["checks"].values()) else "failed"
                atomic_json(args.output, report)
        if args.cpu_only:
            report["A08_controlled_full_and_generation"] = {"passed": None, "status": "outside_selected_cpu_profile"}
        elif args.blocking_regression:
            report["A08_controlled_full_and_generation"] = controlled_blocking_regression(args.blocking_regression)
        else:
            report["A08_controlled_full_and_generation"] = {"passed": False, "status": "not_run", "reason": "Supply current native_task_adapter_test binary; outer HTTP admission 128 cannot fill the 1024-slot internal pool."}
        report["comparison"] = [{"loops": int(loops), "idle_cpu_percent": run["checks"]["idle"]["one_core_cpu_percent"],
                                 "threads": run["checks"]["idle"]["after"].get("threads"),
                                 "idle_rss_bytes": run["checks"]["idle"]["after"].get("rss_bytes"),
                                 "idle_fds_or_handles": run["checks"]["idle"]["after"].get("fds", run["checks"]["idle"]["after"].get("handles")),
                                 "cpu_requests_per_second": run["checks"]["cpu"]["requests_per_second"],
                                 "cpu_p95_ms": run["checks"]["cpu"]["all"]["p95_ms"],
                                 "cpu_p99_ms": run["checks"]["cpu"]["all"]["p99_ms"],
                                 "timer_p99_ms": run["checks"].get("fairness", {}).get("routes", {}).get("/timer", {}).get("p99_ms"),
                                 "ordinary_p99_ms": run["checks"].get("fairness", {}).get("routes", {}).get("/ok", {}).get("p99_ms")}
                                for loops, run in report["runs"].items()]
        report["status"] = "passed" if all(run["status"] == "passed" for run in report["runs"].values()) and (args.cpu_only or report["A08_controlled_full_and_generation"]["passed"]) else "failed"
        if args.cpu_only and report["status"] == "passed":
            report["status"] = "cpu_profile_passed"
    except BaseException as error:
        report["status"] = "failed"
        report["failure"] = {"type": type(error).__name__, "message": str(error)[:6000]}
        atomic_json(args.output, report)
        raise
    atomic_json(args.output, report)
    print(json.dumps({"status": report["status"], "output": str(args.output)}, ensure_ascii=False))
    if report["status"] not in ("passed", "cpu_profile_passed"):
        raise SystemExit(1)


if __name__ == "__main__":
    main()
