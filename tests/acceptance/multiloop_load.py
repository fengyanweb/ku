#!/usr/bin/env python3
"""Bounded native multiloop benchmark/acceptance; no existing database is used.

Writes fixed thresholds before running. Retains histograms and at most 20 error
samples, not unbounded per-request traces. Native build/probes are temporary and
reaped in finally. This is not a historical 504 investigation.
"""
import argparse
import concurrent.futures
import hashlib
import http.client
import json
import os
from pathlib import Path
import platform
import queue
import select
import socket
import subprocess
import tempfile
import threading
import time
import sys


def unused_port():
    with socket.socket() as handle:
        handle.bind(("127.0.0.1", 0))
        return handle.getsockname()[1]


def atomic_json(path, value):
    path.parent.mkdir(parents=True, exist_ok=True)
    temporary = path.with_suffix(path.suffix + ".writing")
    temporary.write_text(json.dumps(value, indent=2, ensure_ascii=False) + "\n", encoding="utf-8")
    temporary.replace(path)


def windows_process_sample(pid):
    """Read the child directly; every diagnostic handle belongs to this caller.

    PowerShell startup used to exceed the first sample's timeout on CI. The
    documented process counters and Toolhelp thread snapshot avoid subprocess
    startup, and neither a missing counter nor a failed close is a zero sample.
    """
    import ctypes
    from ctypes import wintypes

    class MemoryCounters(ctypes.Structure):
        _fields_ = [("cb", wintypes.DWORD), ("PageFaultCount", wintypes.DWORD)] + [
            (name, ctypes.c_size_t) for name in (
                "PeakWorkingSetSize", "WorkingSetSize", "QuotaPeakPagedPoolUsage",
                "QuotaPagedPoolUsage", "QuotaPeakNonPagedPoolUsage",
                "QuotaNonPagedPoolUsage", "PagefileUsage", "PeakPagefileUsage")]

    class ThreadEntry(ctypes.Structure):
        _fields_ = [("dwSize", wintypes.DWORD), ("cntUsage", wintypes.DWORD),
                    ("th32ThreadID", wintypes.DWORD), ("th32OwnerProcessID", wintypes.DWORD),
                    ("tpBasePri", wintypes.LONG), ("tpDeltaPri", wintypes.LONG),
                    ("dwFlags", wintypes.DWORD)]

    kernel = ctypes.WinDLL("kernel32", use_last_error=True)
    kernel.OpenProcess.argtypes = [wintypes.DWORD, wintypes.BOOL, wintypes.DWORD]
    kernel.OpenProcess.restype = wintypes.HANDLE
    kernel.CloseHandle.argtypes = [wintypes.HANDLE]
    kernel.CloseHandle.restype = wintypes.BOOL
    kernel.GetProcessHandleCount.argtypes = [wintypes.HANDLE, ctypes.POINTER(wintypes.DWORD)]
    kernel.GetProcessHandleCount.restype = wintypes.BOOL
    kernel.K32GetProcessMemoryInfo.argtypes = [wintypes.HANDLE, ctypes.POINTER(MemoryCounters), wintypes.DWORD]
    kernel.K32GetProcessMemoryInfo.restype = wintypes.BOOL
    kernel.GetProcessTimes.argtypes = [wintypes.HANDLE] + [ctypes.POINTER(wintypes.FILETIME)] * 4
    kernel.GetProcessTimes.restype = wintypes.BOOL
    kernel.CreateToolhelp32Snapshot.argtypes = [wintypes.DWORD, wintypes.DWORD]
    kernel.CreateToolhelp32Snapshot.restype = wintypes.HANDLE
    for function in (kernel.Thread32First, kernel.Thread32Next):
        function.argtypes = [wintypes.HANDLE, ctypes.POINTER(ThreadEntry)]
        function.restype = wintypes.BOOL

    def check(ok, operation):
        if not ok:
            raise OSError(ctypes.get_last_error(), operation)

    process = kernel.OpenProcess(0x1000, False, int(pid))  # PROCESS_QUERY_LIMITED_INFORMATION
    check(process, "OpenProcess")
    try:
        count = wintypes.DWORD()
        check(kernel.GetProcessHandleCount(process, ctypes.byref(count)), "GetProcessHandleCount")
        memory = MemoryCounters()
        memory.cb = ctypes.sizeof(memory)
        check(kernel.K32GetProcessMemoryInfo(process, ctypes.byref(memory), memory.cb), "K32GetProcessMemoryInfo")
        created, exited, system, user = (wintypes.FILETIME() for _ in range(4))
        check(kernel.GetProcessTimes(process, ctypes.byref(created), ctypes.byref(exited),
                                     ctypes.byref(system), ctypes.byref(user)), "GetProcessTimes")
        snapshot = kernel.CreateToolhelp32Snapshot(0x4, 0)  # TH32CS_SNAPTHREAD
        if snapshot == wintypes.HANDLE(-1).value:
            raise OSError(ctypes.get_last_error(), "CreateToolhelp32Snapshot")
        try:
            thread = ThreadEntry()
            thread.dwSize = ctypes.sizeof(thread)
            more = kernel.Thread32First(snapshot, ctypes.byref(thread))
            threads = 0
            while more:
                threads += int(thread.th32OwnerProcessID == int(pid))
                thread.dwSize = ctypes.sizeof(thread)
                more = kernel.Thread32Next(snapshot, ctypes.byref(thread))
            if ctypes.get_last_error() != 18:  # ERROR_NO_MORE_FILES
                raise OSError(ctypes.get_last_error(), "Thread32Next/First")
            if not threads:
                raise OSError("sampled process has no live threads")
        finally:
            check(kernel.CloseHandle(snapshot), "CloseHandle(thread snapshot)")
        ticks = lambda value: (value.dwHighDateTime << 32) | value.dwLowDateTime
        return {"handles": count.value, "threads": threads, "rss_bytes": memory.WorkingSetSize,
                "cpu_seconds": (ticks(system) + ticks(user)) / 10000000,
                "sampler": "Win32 process counters and Toolhelp thread snapshot"}
    finally:
        check(kernel.CloseHandle(process), "CloseHandle(process sample)")


def process_sample(pid):
    sample = {"monotonic": time.monotonic()}
    try:
        if platform.system() == "Linux":
            fields = {}
            for line in Path(f"/proc/{pid}/status").read_text().splitlines():
                if ":" in line:
                    key, value = line.split(":", 1)
                    fields[key] = value.strip()
            sample.update(rss_bytes=int(fields["VmRSS"].split()[0])*1024,
                          threads=int(fields["Threads"]), fds=len(list(Path(f"/proc/{pid}/fd").iterdir())))
        elif platform.system() == "Darwin":
            output = subprocess.check_output(["ps", "-p", str(pid), "-o", "rss=,pcpu=,time="], text=True, timeout=2).split()
            sample.update(rss_bytes=int(output[0])*1024, cpu_percent=float(output[1]), cpu_time=output[2])
            threads = subprocess.check_output(["ps", "-M", "-p", str(pid)], text=True, timeout=2)
            sample["threads"] = max(0, len(threads.splitlines())-1)
            descriptors = subprocess.run(["lsof", "-a", "-p", str(pid), "-Ff"], capture_output=True, text=True, timeout=3)
            sample["fds"] = sum(line.startswith("f") and line[1:].isdigit() for line in descriptors.stdout.splitlines())
        elif platform.system() == "Windows":
            sample.update(windows_process_sample(pid))
            sample["sample_duration_ms"] = (time.monotonic() - sample["monotonic"]) * 1000
    except (OSError, ValueError, KeyError, subprocess.SubprocessError) as error:
        sample["sample_error"] = type(error).__name__
        if platform.system() == "Windows":
            sample["sample_error_detail"] = str(error)[:160]
    return sample


class Results:
    def __init__(self):
        self.lock = threading.Lock()
        self.counts = {}
        self.histogram = [0]*50001  # 0.1ms buckets, final bucket >=5s
        self.samples = []
        self.arrivals = 0
        self.client_shed = 0
        self.inflight = 0
        self.peak_inflight = 0

    def add(self, category, elapsed, sample=None):
        with self.lock:
            self.counts[category] = self.counts.get(category, 0)+1
            self.histogram[min(50000, int(elapsed*10000))] += 1
            if sample is not None and len(self.samples) < 20:
                self.samples.append(sample)
            self.inflight -= 1

    def value(self):
        with self.lock:
            total = sum(self.counts.values())
            def percentile(fraction):
                threshold = max(1, int(total*fraction+0.999999))
                cumulative = 0
                for index, count in enumerate(self.histogram):
                    cumulative += count
                    if cumulative >= threshold:
                        return (index+1)/10
                return 0
            return {"arrivals": self.arrivals, "completed": total, "client_shed": self.client_shed,
                    "counts": dict(self.counts), "peak_inflight": self.peak_inflight,
                    "p50_ms": percentile(.5), "p95_ms": percentile(.95), "p99_ms": percentile(.99),
                    "first_errors": list(self.samples)}


class Service:
    def __init__(self, executable, root, loops):
        self.port, self.management = unused_port(), unused_port()
        self.token = "acceptance-internal-token"
        (root/"http.addr").write_text(f"127.0.0.1:{self.port}", encoding="ascii")
        environment = os.environ | {"KU_EVENT_LOOPS": str(loops), "KU_BLOCKING_THREADS": "5",
            "KU_THREAD_BUDGET": "16", "KU_SERVICE_CONTROL_TOKEN": self.token,
            "KU_SERVICE_PROBE_ADDR": f"127.0.0.1:{self.management}", "KU_TASK_MAX_BYTES": "16777216"}
        self.log = (root/f"service-{loops}.log").open("wb")
        self.child = subprocess.Popen([str(executable)], cwd=root, env=environment,
                                      stdout=self.log, stderr=self.log)
        deadline = time.monotonic()+5
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
        self.stop()
        raise RuntimeError("native service did not become ready")

    def control(self, path, method="GET"):
        connection = http.client.HTTPConnection("127.0.0.1", self.management, timeout=2)
        try:
            connection.request(method, path, headers={"X-Ku-Control-Token": self.token, "Connection": "close"})
            response = connection.getresponse()
            return response.status, response.read(65536)
        finally:
            connection.close()

    def metrics(self):
        status, body = self.control("/__ku/metrics")
        if status != 200:
            raise RuntimeError(f"metrics status {status}")
        return json.loads(body)

    def stop(self):
        if self.child.poll() is None:
            try:
                self.control("/__ku/drain", "POST")
                self.child.wait(timeout=6)
            except (OSError, http.client.HTTPException, subprocess.TimeoutExpired):
                self.child.kill()
                self.child.wait(timeout=3)
        self.log.close()
        return self.child.returncode


def request(service, path, expected, results, persistence):
    started = time.monotonic()
    connection = persistence.get("connection")
    # The fixture closes idle sockets at 1s. A worker may receive no job for
    # longer than that even while the shared queue is active. Retire its idle
    # client connection before sending a new request; never replay a failed one.
    if connection is not None and started-persistence.get("last_completed", started) >= .5:
        connection.close()
        persistence.pop("connection", None)
        connection = None
    if connection is None:
        connection = http.client.HTTPConnection("127.0.0.1", service.port, timeout=3)
        persistence["connection"] = connection
    try:
        connection.request("GET", path, headers={"Connection": "keep-alive"})
        response = connection.getresponse()
        body = response.read(65536)
        category = "success" if response.status == 200 and body == expected else f"http_{response.status}"
        if response.status == 200 and body != expected:
            category = "wrong_body"
        results.add(category, time.monotonic()-started,
                    None if category == "success" else {"path": path, "status": response.status, "body": repr(body[:160])})
        persistence["last_completed"] = time.monotonic()
    except (OSError, http.client.HTTPException) as error:
        if isinstance(error, (http.client.RemoteDisconnected, ConnectionResetError, BrokenPipeError)):
            category = "server_transport_close"
        else:
            category = "client_timeout" if isinstance(error, TimeoutError) else "connection_error"
        results.add(category, time.monotonic()-started,
                    {"path": path, "error": type(error).__name__, "errno": getattr(error, "errno", None),
                     "detail": str(error)[:160]})
        connection.close()
        persistence.pop("connection", None)


class PartialBodyFlood:
    """Keep a bounded population of incomplete bodies throughout overload.

    These deliberate peers are counted separately from measured requests. A
    transport close without a status is ambiguous, never reported as a proved
    expected admission rejection.
    """
    def __init__(self, service):
        self.service = service
        self.stopping = threading.Event()
        self.counts = {}
        self.peak = 0
        self.thread = threading.Thread(target=self.run)

    def count(self, key):
        self.counts[key] = self.counts.get(key, 0)+1

    def run(self):
        held = {}
        # Bound replenishment as well as simultaneous sockets. Immediate 503
        # responses must not turn this into an unbounded reconnect loop.
        next_attempt = time.monotonic()
        try:
            while not self.stopping.is_set():
                for _ in range(min(32, 280-len(held))):
                    if self.stopping.is_set():
                        break
                    now = time.monotonic()
                    if now < next_attempt:
                        break
                    next_attempt = now+1/280
                    peer = None
                    try:
                        peer = socket.create_connection(("127.0.0.1", self.service.port), timeout=.02)
                        peer.sendall(b"POST /echo HTTP/1.1\r\nHost: localhost\r\nContent-Length: 4000\r\n\r\nx")
                        peer.setblocking(False)
                        held[peer] = b""
                        self.count("connected")
                        self.peak = max(self.peak, len(held))
                    except OSError as error:
                        if peer is not None and peer not in held:
                            peer.close()
                        self.count("connect_"+type(error).__name__)
                wait = max(0, min(.02, next_attempt-time.monotonic())) if len(held) < 280 else .02
                if not held:
                    self.stopping.wait(wait)
                    continue
                readable, _, _ = select.select(list(held), [], [], wait)
                for peer in readable:
                    try:
                        chunk = peer.recv(1024)
                        prefix = held[peer]+chunk
                        if b"\r\n" in prefix:
                            first = prefix.split(b"\r\n", 1)[0].split()
                            key = "http_"+first[1].decode("ascii") if len(first)>1 and first[1].isdigit() else "malformed_response"
                        elif chunk and len(prefix)<1024:
                            held[peer] = prefix
                            continue
                        else:
                            key = "closed_without_status_ambiguous" if not chunk else "oversize_status"
                    except OSError as error:
                        key = "transport_"+type(error).__name__+"_ambiguous"
                    self.count(key)
                    held.pop(peer)
                    peer.close()
        finally:
            self.counts["closed_by_generator_at_phase_end"] = len(held)
            for peer in held:
                peer.close()

    def start(self):
        self.thread.start()

    def stop(self):
        self.stopping.set()
        self.thread.join(timeout=3)
        if self.thread.is_alive():
            raise RuntimeError("bounded partial-body generator did not stop")
        return {"counts": self.counts, "peak_connections": self.peak, "connection_limit": 280,
                "maximum_connection_attempts_per_second": 280,
                "classification": "408 body deadline and 503 admission are expected; statusless transport failures remain ambiguous"}


def phase(service, seconds, rps, overload=False, progress=None):
    results = Results()
    work = queue.Queue(maxsize=64)
    stopping = threading.Event()
    def worker():
        persistence = {}
        try:
            while not stopping.is_set() or not work.empty():
                try:
                    path, expected = work.get(timeout=.1)
                except queue.Empty:
                    continue
                request(service, path, expected, results, persistence)
                work.task_done()
        finally:
            if persistence.get("connection"):
                persistence["connection"].close()
    threads = [threading.Thread(target=worker) for _ in range(32)]
    for thread in threads:
        thread.start()
    flood = PartialBodyFlood(service) if overload else None
    samples = []
    began = time.monotonic()
    sample_stop = threading.Event()
    def sample_resources():
        # ps/lsof and management I/O can take hundreds of milliseconds. Keep
        # them off the arrival clock so observation cannot create burst traffic.
        while not sample_stop.is_set():
            sample = process_sample(service.child.pid)
            try:
                sample["metrics"] = service.metrics()
            except (OSError, ValueError, http.client.HTTPException) as error:
                sample["metrics_error"] = type(error).__name__
            samples.append(sample)  # At most 361 samples for allowed duration.
            if progress:
                progress({"elapsed_seconds": time.monotonic()-began, "requests": results.value(),
                          "resource_samples": list(samples)})
            sample_stop.wait(10)
    sampler = threading.Thread(target=sample_resources)
    sampler.start()
    try:
        if flood:
            flood.start()
        next_arrival = began
        index = 0
        while time.monotonic()-began < seconds:
            now = time.monotonic()
            if now < next_arrival:
                time.sleep(min(.002, next_arrival-now))
                continue
            with results.lock:
                results.arrivals += 1
                if results.inflight >= 64:
                    results.client_shed += 1
                    next_arrival += 1/rps
                    continue
                results.inflight += 1
                results.peak_inflight = max(results.peak_inflight, results.inflight)
            choices = [("/ok", b"ok-ready"), ("/cpu", b"cpu-ready"), ("/timer", b"timer-ready"),
                       ("/cpu", b"cpu-ready"), ("/hot", b"hot-ready")]
            work.put_nowait(choices[index % len(choices)])
            index += 1
            next_arrival += 1/rps
    finally:
        overload_removed = time.monotonic() if flood else None
        flood_result = flood.stop() if flood else None
        stopping.set()
        sample_stop.set()
        for thread in threads:
            thread.join(timeout=10)
        if any(thread.is_alive() for thread in threads):
            raise RuntimeError("load generator threads did not finish")
        sampler.join(timeout=6)
        if sampler.is_alive():
            raise RuntimeError("bounded resource sampler did not finish")
    result = results.value()
    result.update(duration_seconds=time.monotonic()-began, target_rps=rps, overload=overload,
                  resource_samples=samples)
    if flood_result:
        result["deliberate_partial_body_peers"] = flood_result
        result["overload_removed_monotonic"] = overload_removed
    return result


def recovery_probe(service, removed_at):
    """Measure from fault removal, including load-client retirement time."""
    deadline = removed_at+5
    last = {"status": "not_observed"}
    while time.monotonic() < deadline:
        connection = http.client.HTTPConnection("127.0.0.1", service.port,
            timeout=max(.001, deadline-time.monotonic()))
        try:
            connection.request("GET", "/ok", headers={"Connection": "close"})
            response = connection.getresponse()
            body = response.read(65536)
            metrics = service.metrics()
            queued = sum(row["queued"]+row["running"]+row["resident"]+row["pending"] for row in metrics["loops"])
            tasks = sum(row["ready"]+row["waiting"]+row["running"]+row["resident"] for row in metrics.get("tasks", []))
            last = {"status": response.status, "body_correct": body == b"ok-ready", "queued_or_running": queued+tasks}
            if response.status == 200 and body == b"ok-ready" and not queued and not tasks:
                return {"passed": True, "seconds_since_overload_removal": time.monotonic()-removed_at,
                        "metrics": metrics}
        except (OSError, http.client.HTTPException) as error:
            last = {"error": type(error).__name__}
        finally:
            connection.close()
        time.sleep(.02)
    return {"passed": False, "seconds_since_overload_removal": time.monotonic()-removed_at,
            "last": last}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--ku", required=True, type=Path)
    parser.add_argument("--output", required=True, type=Path)
    parser.add_argument("--duration", type=int, default=15)
    parser.add_argument("--rps", type=float, default=100)
    parser.add_argument("--loops", type=int, nargs="+", default=[1, 2, 4])
    parser.add_argument("--profile", choices=["debug", "release"], default="release")
    parser.add_argument("--overload-only", action="store_true", help="Bounded 3..30s overload and recovery preflight, not sustained acceptance")
    args = parser.parse_args()
    if args.duration < 3 or args.duration > 3600 or not 1 <= args.rps <= 5000 or any(n not in [1, 2, 4] for n in args.loops):
        parser.error("duration 3..3600, rps 1..5000, loops 1/2/4 required")
    if args.overload_only and args.duration > 30:
        parser.error("overload preflight duration must be 3..30 seconds")
    repository = Path(__file__).resolve().parents[2]
    report = {"status": "running", "platform": platform.platform(), "cpu_count": os.cpu_count(),
        "cli_sha256": hashlib.sha256(args.ku.resolve().read_bytes()).hexdigest(),
        "fixture_sha256": hashlib.sha256((repository/"tests/fixtures/multiloop_service.ku").read_bytes()).hexdigest(),
        "source_commit": subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=repository, text=True).strip(),
        "source_dirty": bool(subprocess.check_output(["git", "status", "--porcelain"], cwd=repository, text=True)),
        "tls": "none; direct loopback HTTP", "load_generator": "same host, fixed32threads, max64inflight",
        "parameters": {"duration_seconds": args.duration, "arrival_rps": args.rps, "loops": args.loops,
            "native_c_profile": args.profile, "blocking_threads_total": 5, "thread_budget": 16,
            "normal_unexpected_errors": 0, "normal_p99_ms": 250, "pre_overload_peak_loop_delay_ms": 100,
            "overload_http_statuses": [200, 408, 503, 504], "overload_unclassified_measured_errors": 0,
            "overload_slow_peer_attempts_per_second": 280, "overload_slow_peer_connections": 280,
            "recovery_seconds": 5, "formal_phases_seconds": {"warm": 60, "steady": 1470, "overload": 30,
                "recovery": args.duration-1560} if args.duration >= 1800 else None,
            "overload_duration_reason": "280 slow peers over 30s fits the same-host 16384 ephemeral-port budget; service deadlines unchanged",
            "queue_bytes": "fixed64 descriptors, no queued request bodies", "timers": "25ms real async wait"}, "runs": []}
    atomic_json(args.output, report)  # Freeze before any timed workload.
    with tempfile.TemporaryDirectory(prefix="ku-multiloop-acceptance-") as directory:
        root = Path(directory)
        source = root/"service.ku"
        source.write_bytes((repository/"tests/fixtures/multiloop_service.ku").read_bytes())
        executable = root/("service.exe" if os.name == "nt" else "service")
        build = subprocess.run([str(args.ku.resolve()), "build", "--backend", "c", "--profile", args.profile, str(source), "-o", str(executable)],
                               cwd=root, capture_output=True, text=True, timeout=120)
        if build.returncode:
            raise RuntimeError(build.stderr[-6000:])
        report["artifact_sha256"] = hashlib.sha256(executable.read_bytes()).hexdigest()
        report["build_output"] = build.stdout[-2000:]
        for loops in args.loops:
            service = Service(executable, root, loops)
            try:
                item = {"loops": loops, "baseline": process_sample(service.child.pid), "phases": []}
                report["runs"].append(item)
                if args.overload_only:
                    phases = [("overload", args.duration, args.rps*3, True)]
                elif args.duration >= 1800:
                    phases = [("warm", 60, args.rps, False), ("steady", 1470, args.rps, False),
                              ("overload", 30, args.rps*3, True), ("recovery", args.duration-1560, args.rps, False)]
                else:
                    phases = [("short", args.duration, args.rps, False)]
                for name, seconds, rate, overload in phases:
                    def checkpoint(value):
                        report["live"] = {"loops": loops, "phase": name, **value}
                        atomic_json(args.output, report)
                    result = phase(service, seconds, rate, overload, progress=checkpoint)
                    report.pop("live", None)
                    result["name"] = name
                    result["end_metrics"] = service.metrics()
                    result["end_resources"] = process_sample(service.child.pid)
                    if overload:
                        allowed = {"success", "http_408", "http_503", "http_504", "server_transport_close"}
                        result["overload_classification"] = {
                            "correct": result["counts"].get("success", 0),
                            "explicit_admission_rejection": result["counts"].get("http_503", 0),
                            "explicit_deadline": sum(result["counts"].get(key, 0) for key in ["http_408", "http_504"]),
                            "server_transport_close": result["counts"].get("server_transport_close", 0),
                            "load_client_budget_refusal": result["client_shed"],
                            "unclassified_or_unexpected": {key: count for key, count in result["counts"].items() if key not in allowed},
                            "meaning": "Direct-peer EOF/reset/broken-pipe is server transport termination, not an HTTP 503 or a uniquely identified close reason. Local queue refusals are load-client limits and remain in total arrivals. Other transport/client/protocol errors fail. Deliberate slow peers remain separate."}
                        result["overload_classification"]["pressure_observed"] = (
                            sum(row["rejected_responses"] for row in result["end_metrics"]["outcomes"]) > 0)
                    item["phases"].append(result)
                    if overload:
                        result["recovery_probe"] = recovery_probe(service, result["overload_removed_monotonic"])
                    atomic_json(args.output, report)
                    unexpected = sum(count for key, count in result["counts"].items() if key != "success")
                    if not overload and (unexpected or result["client_shed"] or result["p99_ms"] > 250):
                        raise RuntimeError(f"{name} failed frozen threshold: {result['counts']} p99={result['p99_ms']} shed={result['client_shed']}")
                    # The metric is cumulative. After overload it cannot establish
                    # a separate recovery-phase maximum, so do not claim one.
                    if name in ["short", "warm", "steady"]:
                        peak = max(row["peak_loop_delay_ms"] for row in result["end_metrics"]["outcomes"])
                        if peak > 100:
                            raise RuntimeError(f"{name} loop delay exceeded frozen 100ms threshold: {peak}")
                    if overload and (not result["recovery_probe"]["passed"] or result["counts"].get("wrong_body", 0)):
                        raise RuntimeError("overload response correctness or five-second recovery failed")
                    if overload and (result["overload_classification"]["unclassified_or_unexpected"]
                                     or not result["overload_classification"]["pressure_observed"]):
                        raise RuntimeError("overload has unexpected errors or did not reach actual admission pressure")
                item["final_metrics"] = service.metrics()
                item["exit_code"] = service.stop()
                if item["exit_code"] != 0:
                    raise RuntimeError(f"nonzero native drain exit: {item['exit_code']}")
                atomic_json(args.output, report)
            finally:
                service.stop()
        report["status"] = "passed"
        atomic_json(args.output, report)
    print(json.dumps({"status": report["status"], "output": str(args.output)}))


if __name__ == "__main__":
    try:
        main()
    except Exception as error:
        if "--output" in sys.argv:
            target = Path(sys.argv[sys.argv.index("--output")+1])
            if target.is_file():
                evidence = json.loads(target.read_text(encoding="utf-8"))
                evidence.update(status="failed", error=f"{type(error).__name__}: {error}")
                atomic_json(target, evidence)
        raise
