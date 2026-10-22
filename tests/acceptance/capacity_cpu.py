#!/usr/bin/env python3
"""Bounded measurement helpers for the existing native acceptance Service.

The caller owns the service, workload selection and calibration. This module
neither builds Ku nor changes a service configuration. One Go process generates
traffic; its independent clock is never paced by the Python sampler.
"""

from collections import deque
import http.client
import json
import math
import os
from pathlib import Path
import subprocess
import tempfile
import threading
import time

from multiloop_load import process_sample
from runtime_matrix import cpu_sample


SAMPLE_INTERVAL_SECONDS = 0.5
SAMPLE_LIMIT = 128
ERROR_LIMIT = 12
CLIENT_OUTPUT_LIMIT = 1024 * 1024
REQUEST_TIMEOUT_MS = 3000


def checksum(work_units, seed=3):
    """Expected eight-digit decimal body for the runtime-input CPU fixture."""
    if not isinstance(work_units, int) or isinstance(work_units, bool) or not 1 <= work_units <= 100000:
        raise ValueError("work_units must be an integer in 1..100000")
    if not isinstance(seed, int) or isinstance(seed, bool) or not 1 <= seed <= 1024:
        raise ValueError("seed must be an integer in 1..1024")
    total = seed
    for index in range(work_units * 1000):
        total = (total * 33 + index + seed) % 100000000
    return f"{total:08d}"


def work_path(units, seed=3):
    """Use bounded runtime string lengths instead of a compiler-known input."""
    if not isinstance(units, int) or isinstance(units, bool) or not 1 <= units <= 3000:
        raise ValueError("units must be an integer in 1..3000 (within the HTTP target limit)")
    if not isinstance(seed, int) or isinstance(seed, bool) or not 1 <= seed <= 1024:
        raise ValueError("seed must be an integer in 1..1024")
    path = "/work?work=" + "x" * units + "&seed=" + "s" * seed
    if len(path) > 4000:
        raise ValueError("combined runtime input exceeds the bounded HTTP target")
    return path


def _number(value):
    return isinstance(value, (int, float)) and not isinstance(value, bool) and math.isfinite(value)


def _sum_rows(metrics, array, field):
    rows = metrics.get(array)
    if not isinstance(rows, list) or not rows:
        return None
    values = [row.get(field) for row in rows if isinstance(row, dict)]
    if len(values) != len(rows) or any(not _number(value) for value in values):
        return None
    return sum(values)


def _compressed(metrics):
    result = {}
    for array, prefix, fields in (
        ("loops", "http", ("connections", "pending", "queued", "running", "resident", "requests", "cpu_ns")),
        ("tasks", "task", ("ready", "waiting", "running", "resident", "timers", "polls")),
        ("outcomes", "http", ("queued_bytes",)),
    ):
        for field in fields:
            value = _sum_rows(metrics, array, field)
            if value is not None:
                result[prefix + "_" + field] = value
    for field in ("buffer_bytes", "peak_buffer_bytes", "buffer_rejections"):
        value = metrics.get(field)
        if _number(value):
            result["http_" + field] = value
    for field in ("resident", "queued", "running", "output_charge_bytes", "net_wait", "net_step"):
        value = metrics.get("task_blocking", {}).get(field)
        if _number(value):
            result["blocking_" + field] = value
    for field in ("reserved_connections", "reserved_waiters", "pending_operations", "pending_bytes"):
        value = metrics.get("database", {}).get(field)
        if _number(value):
            result["database_" + field] = value
    ages = [row.get("cpu_sample_age_ms") for row in metrics.get("loops", []) if isinstance(row, dict)]
    if ages and all(_number(value) for value in ages):
        result["event_cpu_sample_age_ms_max"] = max(ages)
    return result


def _metric_snapshot(service):
    began = time.monotonic()
    metrics = service.metrics()
    ended = time.monotonic()
    if not isinstance(metrics, dict):
        raise ValueError("service metrics must be a JSON object")
    return {"begin_monotonic": began, "end_monotonic": ended,
            "duration_ms": (ended - began) * 1000, "metrics": metrics}


def _process_snapshot(pid, with_cpu=False):
    began = time.monotonic()
    value = cpu_sample(pid) if with_cpu else process_sample(pid)
    ended = time.monotonic()
    return {"begin_monotonic": began, "end_monotonic": ended,
            "duration_ms": (ended - began) * 1000, "value": value}


def _delta(first, last, label, errors):
    if not _number(first) or not _number(last):
        errors.append(label + ": missing or nonnumeric counter")
        return None
    if last < first:
        errors.append(label + ": counter decreased")
        return None
    return last - first


def _server_delta(before, after, native_before, native_after):
    errors = []
    first, last = before["metrics"], after["metrics"]
    observed_wall = after["end_monotonic"] - before["end_monotonic"]
    result = {"observation_seconds": observed_wall,
              "window_scope": "Before/after control observations include process launch, client measurement, bounded cleanup and observation gaps; they are not the exact Go measurement window.",
              "counter_scope": "requests counts complete business bodies before UTF-8/admission; response counters count prepared responses, not confirmed client delivery. All concurrent traffic on this private service is included.",
              "complete_requests_received": _delta(_sum_rows(first, "loops", "requests"), _sum_rows(last, "loops", "requests"), "complete_requests_received", errors),
              "connections_accepted": _delta(first.get("accepted"), last.get("accepted"), "connections_accepted", errors),
              "admitted": _delta(_sum_rows(first, "outcomes", "admitted"), _sum_rows(last, "outcomes", "admitted"), "admitted", errors),
              "responses_prepared": {}}
    for field in ("success_responses", "rejected_responses", "timeout_responses", "other_responses"):
        result["responses_prepared"][field] = _delta(_sum_rows(first, "outcomes", field), _sum_rows(last, "outcomes", field), field, errors)
    initial = {row.get("id"): row for row in first.get("loops", []) if isinstance(row, dict)}
    final = {row.get("id"): row for row in last.get("loops", []) if isinstance(row, dict)}
    owners = []
    if set(initial) != set(final) or not initial:
        errors.append("event owners changed or are missing")
    for owner in sorted(set(initial) | set(final)):
        start, finish = initial.get(owner, {}), final.get(owner, {})
        prefix = "owner_" + str(owner)
        cpu = _delta(start.get("cpu_ns"), finish.get("cpu_ns"), prefix + ".cpu_ns", errors)
        sample_ms = _delta(start.get("cpu_sample_ms"), finish.get("cpu_sample_ms"), prefix + ".cpu_sample_ms", errors)
        clock_errors = _delta(start.get("cpu_clock_errors"), finish.get("cpu_clock_errors"), prefix + ".cpu_clock_errors", errors)
        owners.append({"id": owner, "cpu_seconds": None if cpu is None else cpu / 1e9,
                       "cpu_sample_window_seconds": None if sample_ms is None else sample_ms / 1000,
                       "cpu_sample_age_before_ms": start.get("cpu_sample_age_ms"),
                       "cpu_sample_age_after_ms": finish.get("cpu_sample_age_ms"),
                       "cpu_clock_errors_delta": clock_errors,
                       "complete_requests_received": _delta(start.get("requests"), finish.get("requests"), prefix + ".requests", errors)})
        if clock_errors:
            errors.append(prefix + ": CPU clock errors observed")
    total_cpu = sum(row["cpu_seconds"] for row in owners) if owners and all(row["cpu_seconds"] is not None for row in owners) else None
    result["event_threads"] = owners
    result["event_cpu_seconds"] = total_cpu
    result["average_event_cores_over_observation_window"] = total_cpu / observed_wall if total_cpu is not None and observed_wall > 0 else None
    native_first, native_last = native_before["value"], native_after["value"]
    result["native_process_cpu_seconds"] = _delta(native_first.get("cpu_seconds"), native_last.get("cpu_seconds"), "native_process_cpu_seconds", errors)
    result["native_process_cpu_window_seconds"] = native_after["end_monotonic"] - native_before["end_monotonic"]
    timing_first, timing_last = first.get("task_driver_timing", {}), last.get("task_driver_timing", {})
    timing = {"enabled_before": timing_first.get("enabled"), "enabled_after": timing_last.get("enabled"),
              "scope": "Delta cumulative totals; max_* remain lifetime maxima and cannot be subtracted into phase maxima."}
    for key in ("lock_count", "lock_wait_ns", "lock_hold_ns", "condition_count", "condition_sleep_reacquire_ns", "clock_errors"):
        if key in timing_first or key in timing_last:
            timing[key + "_delta"] = _delta(timing_first.get(key), timing_last.get(key), "task_driver_timing." + key, errors)
    for key in ("max_lock_wait_ns", "max_lock_hold_ns", "max_condition_sleep_reacquire_ns"):
        if key in timing_first or key in timing_last:
            timing[key + "_before"] = timing_first.get(key)
            timing[key + "_after"] = timing_last.get(key)
    if timing.get("clock_errors_delta"):
        errors.append("task_driver_timing: clock errors observed")
    result["task_driver_timing"] = timing
    result["counter_errors"] = errors[:32]
    return result


def _read_tail(handle, limit):
    handle.flush()
    size = handle.seek(0, os.SEEK_END)
    handle.seek(max(0, size - limit))
    return handle.read(limit).decode("utf-8", errors="replace")


def _reap(child):
    """Use wait4 without racing Popen.poll/wait so POSIX exit CPU is exact."""
    if hasattr(os, "wait4"):
        try:
            pid, status, usage = os.wait4(child.pid, os.WNOHANG)
        except InterruptedError:
            return None
        if pid:
            child.returncode = os.waitstatus_to_exitcode(status)
            return usage
        return None
    child.poll()
    return None


def run_client(service, client_path, path, expected, seconds, concurrency=32, rate=0, queue=64):
    """Run one bounded Go generator and observe the caller-owned service.

    Exact POSIX generator CPU includes startup and cleanup via wait4. Other
    platforms preserve the last live sample and explicitly report its exit-tail
    gap. Sampler failures are data, never zero-valued process counters.
    """
    if not _number(seconds) or not 0 < seconds <= 120:
        raise ValueError("seconds must be finite and in (0, 120]")
    if not isinstance(concurrency, int) or not 1 <= concurrency <= 4096:
        raise ValueError("concurrency must be in 1..4096")
    if not _number(rate) or rate < 0 or rate > 1000000 or seconds * rate > 10000000:
        raise ValueError("invalid bounded rate")
    if not isinstance(queue, int) or not 0 <= queue <= 65536:
        raise ValueError("queue must be in 0..65536")
    if isinstance(expected, bytes):
        expected = expected.decode("utf-8")
    if not isinstance(expected, str):
        raise TypeError("expected must be text or UTF-8 bytes")
    client_path = Path(client_path).resolve(strict=True)
    initial = _metric_snapshot(service)
    native_before = _process_snapshot(service.child.pid, with_cpu=True)
    report = {"status": "running", "path": path, "seconds": seconds,
              "service_before": initial, "native_before": native_before,
              "sampling": {"interval_seconds": SAMPLE_INTERVAL_SECONDS, "retained_samples_limit": SAMPLE_LIMIT}}
    samples = deque(maxlen=SAMPLE_LIMIT)
    sample_errors = []
    peaks = _compressed(initial["metrics"])
    resource_peaks = {"native": {}, "client": {}}
    sample_count = 0
    maximum_sample_seconds = 0.0
    stop_sampler = threading.Event()
    child = None
    client_last_live = None
    sampler = None
    usage = None
    timed_out = False
    began = time.monotonic()

    def observe_resource(name, snapshot):
        for field in ("rss_bytes", "threads", "fds", "handles"):
            value = snapshot["value"].get(field)
            if _number(value):
                resource_peaks[name][field] = max(value, resource_peaks[name].get(field, value))

    def sample():
        nonlocal sample_count, maximum_sample_seconds, client_last_live
        next_sample = time.monotonic()
        while not stop_sampler.wait(max(0, next_sample - time.monotonic())):
            sample_began = time.monotonic()
            row = {"elapsed_seconds": sample_began - began}
            try:
                metrics = _metric_snapshot(service)
                row["metric_duration_ms"] = metrics["duration_ms"]
                row["metrics"] = _compressed(metrics["metrics"])
                for key, value in row["metrics"].items():
                    peaks[key] = max(value, peaks.get(key, value))
            except (OSError, ValueError, RuntimeError, http.client.HTTPException) as error:
                row["metrics_error"] = type(error).__name__ + ": " + str(error)[:256]
                if len(sample_errors) < ERROR_LIMIT:
                    sample_errors.append(row["metrics_error"])
            native = _process_snapshot(service.child.pid)
            row["native"] = native["value"]
            observe_resource("native", native)
            client = _process_snapshot(child.pid, with_cpu=True)
            row["client"] = client["value"]
            observe_resource("client", client)
            if _number(client["value"].get("cpu_seconds")):
                client_last_live = client
            row["sample_duration_seconds"] = time.monotonic() - sample_began
            maximum_sample_seconds = max(maximum_sample_seconds, row["sample_duration_seconds"])
            samples.append(row)
            sample_count += 1
            next_sample += SAMPLE_INTERVAL_SECONDS
            if next_sample < time.monotonic():
                next_sample = time.monotonic() + SAMPLE_INTERVAL_SECONDS

    with tempfile.TemporaryFile() as output, tempfile.TemporaryFile() as error_output:
        command = [str(client_path), "--address", "127.0.0.1:" + str(service.port), "--path", path,
                   "--expected", expected, "--duration", str(seconds), "--concurrency", str(concurrency),
                   "--rate", str(rate), "--queue", str(queue), "--timeout-ms", str(REQUEST_TIMEOUT_MS),
                   "--run-id", "capacity-" + str(time.monotonic_ns())]
        report["command"] = command
        try:
            child = subprocess.Popen(command, stdout=output, stderr=error_output)
            report["client_pid"] = child.pid
            sampler = threading.Thread(target=sample, name="ku-capacity-observer", daemon=True)
            sampler.start()
            deadline = began + seconds + REQUEST_TIMEOUT_MS / 1000 + 5
            while child.returncode is None:
                current_usage = _reap(child)
                if current_usage is not None:
                    usage = current_usage
                if child.returncode is not None:
                    break
                if time.monotonic() >= deadline:
                    timed_out = True
                    child.kill()
                    kill_deadline = time.monotonic() + 3
                    while child.returncode is None and time.monotonic() < kill_deadline:
                        current_usage = _reap(child)
                        if current_usage is not None:
                            usage = current_usage
                        if child.returncode is None:
                            time.sleep(.01)
                    break
                time.sleep(.01)
        finally:
            if child is not None and child.returncode is None:
                child.kill()
                try:
                    child.wait(timeout=3)
                except subprocess.TimeoutExpired:
                    report["client_reap_failed"] = True
            stop_sampler.set()
            if sampler is not None:
                # Existing macOS ps/lsof helpers each have finite subprocess
                # deadlines; allow one in-progress pair plus its control read.
                sampler.join(timeout=20)
                if sampler.is_alive():
                    report["sampler_did_not_stop"] = True
        report["client_exit"] = None if child is None else child.returncode
        report["client_timed_out"] = timed_out
        report["client_stderr_tail"] = _read_tail(error_output, 8192)
        output.seek(0)
        payload = output.read(CLIENT_OUTPUT_LIMIT + 1)
        try:
            if len(payload) > CLIENT_OUTPUT_LIMIT:
                raise ValueError("client JSON output exceeded the bounded limit")
            client_result = json.loads(payload)
            if not isinstance(client_result, dict):
                raise ValueError("client JSON output is not an object")
            report["client"] = client_result
        except (ValueError, UnicodeError) as error:
            report["client_parse_error"] = str(error)[:256]
            report["client_stdout_tail"] = payload[-2048:].decode("utf-8", errors="replace")
    report["wrapper_seconds"] = time.monotonic() - began
    report["samples"] = list(samples)
    report["sampling"].update(observed_samples=sample_count, maximum_sample_seconds=maximum_sample_seconds,
                              errors=sample_errors, scope="Peak observations are sampled, not continuous maxima; compression never paces the independent Go arrival clock.")
    report["metric_peaks"] = peaks
    report["process_resource_peaks"] = resource_peaks
    report["client_last_live_process_sample"] = client_last_live
    if usage is not None:
        report["client_cpu"] = {"seconds": usage.ru_utime + usage.ru_stime, "user_seconds": usage.ru_utime,
                                "system_seconds": usage.ru_stime, "source": "POSIX wait4 exact terminated process lifetime",
                                "scope": "Includes Go startup, measurement and finite cleanup; not restricted to the Go measurement window.",
                                "exit_tail_missing": False, "max_rss_raw": usage.ru_maxrss,
                                "max_rss_raw_units": "bytes on macOS, KiB on Linux; platform raw value retained"}
    else:
        last_cpu = client_last_live["value"].get("cpu_seconds") if client_last_live else None
        self_cpu = report.get("client", {}).get("client_resources", {}).get("cpu_seconds")
        report["client_cpu"] = {"seconds": self_cpu if _number(self_cpu) else last_cpu,
                                "source": "client self-report" if _number(self_cpu) else "last live external process sample; lower bound",
                                "exit_tail_missing": not _number(self_cpu)}
    try:
        final = _metric_snapshot(service)
        native_after = _process_snapshot(service.child.pid, with_cpu=True)
        report["service_after"], report["native_after"] = final, native_after
        report["server_delta"] = _server_delta(initial, final, native_before, native_after)
        for key, value in _compressed(final["metrics"]).items():
            peaks[key] = max(value, peaks.get(key, value))
        observe_resource("native", native_after)
        ledger = report.get("client", {}).get("ledger", {})
        received = report["server_delta"]["complete_requests_received"]
        started, written = ledger.get("started"), ledger.get("request_write_completed")
        report["reconciliation"] = {"planned": ledger.get("planned"), "not_started": ledger.get("not_started"),
                                    "started": started, "server_received": received,
                                    "server_received_minus_client_started": received - started if _number(received) and _number(started) else None,
                                    "complete_writes_minus_server_received": written - received if _number(received) and _number(written) else None,
                                    "scope": "Signed differences retain network/observation boundaries. A successful client write is not evidence of server reception; concurrent nonclient traffic is also in server deltas."}
    except (OSError, ValueError, RuntimeError, http.client.HTTPException) as error:
        report["server_after_error"] = type(error).__name__ + ": " + str(error)[:256]
    ledger = report.get("client", {}).get("ledger", {})
    report["status"] = "measured" if report.get("client_exit") == 0 and not timed_out and not report.get("client_parse_error") and not report.get("sampler_did_not_stop") and not report.get("server_after_error") and ledger.get("planned_reconciled") is True and ledger.get("started_reconciled") is True else "failed"
    report["measurement_valid"] = report["status"] == "measured" and not report.get("server_delta", {}).get("counter_errors") and not sample_errors
    report["status_scope"] = "measured confirms driver termination and bookkeeping, not acceptance of throughput, errors, CPU scaling, fairness or recovery thresholds. The caller applies the frozen gates."
    return report
