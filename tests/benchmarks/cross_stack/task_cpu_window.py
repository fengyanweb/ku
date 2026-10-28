#!/usr/bin/env python3
"""Reproducible serial Task CPU/HTTP diagnostic and confirmation windows.

Uses the existing cgroup/HTTP/child helpers. A spec pins actual artifacts;
diagnostic probes and ordinary scoring remain separate. No DB is opened.
"""
from __future__ import annotations

import argparse
import fcntl
import hashlib
import json
import os
from pathlib import Path
import signal
import socket
import statistics
import subprocess
import time

import linux_harness as h


def identity(path):
    path = Path(path).resolve(strict=True)
    return {"path": str(path), "bytes": path.stat().st_size,
            "sha256": hashlib.sha256(path.read_bytes()).hexdigest()}


def cpu_window(samples, label, start, end):
    rows = sorted((d["monotonic_ns"], d) for row in samples
                  for d in row["domains"] if d["label"] == label)
    def at(t, key):
        for (ta, a), (tb, b) in zip(rows, rows[1:]):
            if ta <= t <= tb and tb > ta:
                x, y = a["cpu_stat"][key], b["cpu_stat"][key]
                return x+(y-x)*(t-ta)/(tb-ta), x, y
        return None
    result = {"start_ns": start, "end_ns": end, "available": True,
              "method": "cgroup cumulative CPU interpolation; endpoint brackets are not a statistical CI"}
    for key in ("usage_usec", "user_usec", "system_usec"):
        a, b = at(start, key), at(end, key)
        if a is None or b is None:
            result["available"] = False
            continue
        result[key] = {"seconds": (b[0]-a[0])/1e6,
                       "lower_seconds": max(0, b[1]-a[2])/1e6,
                       "upper_seconds": max(0, b[2]-a[1])/1e6}
    middle = [d for t, d in rows if start <= t <= end]
    steady = middle[len(middle)//5:len(middle)*4//5]
    result["memory"] = {key: {
        "middle60_median_bytes": statistics.median(d[key] for d in steady) if steady else None,
        "sample_peak_bytes": max((d[key] for d in middle), default=None)}
        for key in ("memory_current", "sum_rss_bytes", "sum_pss_bytes")}
    return result


def qualify(driver, short_slo_ms=20, long_slo_ms=3000):
    failures, unknown = [], []
    if not driver.get("valid_driver"):
        failures.append("driver_invalid_retained_not_service_attribution")
    if not driver.get("ledger", {}).get("planned_reconciled"):
        failures.append("plan_not_reconciled")
    for kind, limit in (("ok", short_slo_ms), ("cpu", long_slo_ms)):
        view = driver.get("by_type", {}).get(kind, {})
        if not view.get("normal_planned"):
            unknown.append(kind+"_normal_plan_empty")
            continue
        if (view.get("normal_failure_fraction_planned") is None
                or view["normal_failure_fraction_planned"] > .001):
            failures.append(kind+"_normal_success_below_99.9_percent")
        p99 = view.get("correct_scheduled_to_complete", {}).get("p99_ms")
        if p99 is None:
            unknown.append(kind+"_p99_insufficient_samples")
        elif p99 > limit:
            failures.append(kind+"_p99_over_SLO")
        if view.get("results", {}).get("body_error", 0):
            failures.append(kind+"_wrong_body")
    if driver.get("ledger", {}).get("http_500_total", 0):
        failures.append("http_500")
    if driver.get("configuration", {}).get("cpu_cancel_every"):
        unknown.append("cancel_stop_and_physical_reuse_require_server_trace")
    return {"qualified": not failures and not unknown, "failures": failures,
            "unknown": unknown, "normal_SLO_ms": {"ok": short_slo_ms, "cpu": long_slo_ms}}


def generation_gate(driver, cost, start, end):
    ledger = driver["ledger"]
    generated = ledger.get("scheduled_to_generated")
    fraction = cost.get("usage_usec", {}).get("seconds", 0)/(3*((end-start)/1e9))
    if generated is None:
        # Historical V5 has no dispatch timestamp. Preserve its old conservative
        # gate; do not retrospectively relabel those windows as generator-qualified.
        lag = ledger["scheduled_to_sent"].get("p99_ms")
        complete = True
        basis = "legacy scheduled_to_sent; backpressure-censored"
    else:
        lag = generated.get("p99_ms")
        complete = generated.get("count") == ledger.get("planned") and bool(ledger.get("planned"))
        basis = "scheduled_to_generated; all arrival plans including unsent"
    return {"basis": basis, "dispatch_lag_p99_ms": lag, "limit_ms": 5,
            "all_plans_observed": complete,
            "send_lag_p99_ms": ledger["scheduled_to_sent"].get("p99_ms"),
            "driver_CPU_fraction_of_3CPU": fraction,
            "passed": complete and lag is not None and lag <= 5 and cost["available"] and fraction < .9,
            "scope": "dispatch gate is distinct from client queue/dial/Write/service backpressure; all service SLO failures remain"}


def driver_lifecycle_cpu(before, after, arrival_seconds):
    keys = ("usage_usec", "user_usec", "system_usec")
    delta = {key: after.get("cpu_stat", {}).get(key, -1) - before.get("cpu_stat", {}).get(key, -1) for key in keys}
    available = arrival_seconds > 0 and all(before.get("cpu_stat", {}).get(key, -1) >= 0 and after.get("cpu_stat", {}).get(key, -1) >= 0 and delta[key] >= 0 for key in keys)
    elapsed = (after.get("monotonic_ns", 0) - before.get("monotonic_ns", 0)) / 1e9
    available = available and elapsed > 0
    fraction = delta["usage_usec"] / 1e6 / (3 * arrival_seconds) if available else None
    return {"available": available, "CPU_seconds": {key: value / 1e6 if available else None for key, value in delta.items()},
            "fraction_of_3CPU_original_arrival_duration": fraction, "passed": available and fraction < .9,
            "original_arrival_seconds": arrival_seconds, "lifecycle_wall_seconds": elapsed,
            "before": before, "after": after,
            "scope": "Actual full driver cgroup before launch through process exit, including startup, CSV/gzip export and JSON; denominator is original planned arrival duration, never lengthened by export. This is an additional stricter gate; original arrival CPU gate is retained."}


def client(harness, service, spec, output, phase):
    output.mkdir(exist_ok=False)
    os.chown(output, harness.uid, harness.gid)
    domain = h.Domain(harness, "driver", 3, h.GIB, "2-4")
    report = output/"driver.json"
    command = [spec["driver"]["path"], "--address", service.address,
               "--workload", "task_mix", "--protocol", spec["protocol"]["path"],
               "--corpora-dir", spec["corpora_dir"], "--duration", str(phase["seconds"]),
               "--rate", str(phase["short_rate"]), "--cpu-rate", str(phase["long_rate"]),
               "--concurrency", str(spec.get("concurrency", 4096)),
               "--queue", str(spec.get("queue", 4096)), "--timeout", "3",
               "--keepalive-idle-ms", str(spec.get("idle_ms", 500)),
               "--rid-prefix", str(phase["rid_prefix"]),
               "--cpu-cancel-every", str(phase.get("cpu_cancel_every", 0)),
               "--cpu-cancel-delay-us", "250", "--output", str(report),
               "--events", str(output/"events.csv.gz")]
    env = os.environ.copy()
    env.update(GOMAXPROCS="3")
    env.pop("BENCH_DB_URL", None)
    sampler = h.Sampler([service.domain, domain], output/"resources.json",
                        interval=spec.get("sample_seconds", .2), maximum=20000).start()
    process = None
    try:
        lifecycle_before = domain.snapshot()
        process = harness.launch(command, Path(spec["root"]), env, domain, output/"driver.log")
        began = time.monotonic_ns()
        driver_identity = None
        while process.poll() is None and time.monotonic_ns()-began < 5_000_000_000:
            try:
                actual = Path(os.readlink(f"/proc/{process.pid}/exe"))
                if actual == Path(spec["driver"]["path"]).resolve():
                    driver_identity = identity(actual)
                    break
            except OSError:
                pass
            time.sleep(.005)
        code = process.wait(timeout=phase["seconds"]+20)
        if code != 0 or not report.exists():
            raise RuntimeError(f"driver exit {code}, report {report.exists()}; raw log retained")
        driver = h.read_json(report)
        clock = h.measurement_clock(driver["configuration"])
        time.sleep(.25)  # CPU endpoint bracket; outside arrival scoring.
    finally:
        if process is not None and process.poll() is None:
            domain.kill()
            process.wait(timeout=3)
        samples = sampler.stop()
        last = domain.snapshot()
        domain.kill()
        removed = domain.remove()
    result = {"command": command, "driver": driver, "clock": clock,
              "driver_exec_identity": driver_identity, "driver_after": last,
              "driver_cgroup_removed": removed, "sampler_error": sampler.error,
              "sampler_overflow": sampler.overflow, "qualification": qualify(driver),
              "service_pid": service.process.pid}
    result["driver_lifecycle_CPU"] = driver_lifecycle_cpu(lifecycle_before, last, phase["seconds"])
    if clock["available"]:
        start = clock["origin_ns"]
        end = max(start+int(phase["seconds"]*1e9),
                  driver["configuration"].get("last_actual_completed_monotonic_ns") or 0,
                  start+(driver["configuration"].get("last_actual_completed_offset_ns") or 0))
        result["service_CPU"] = cpu_window(samples, service.domain.label, start, end)
        result["driver_CPU"] = cpu_window(samples, domain.label, start, end)
        result["generation_gate"] = generation_gate(driver, result["driver_CPU"], start, end)
        result["generation_gate"]["full_driver_lifecycle_CPU_passed"] = result["driver_lifecycle_CPU"]["passed"]
        result["generation_gate"]["passed"] = result["generation_gate"]["passed"] and result["driver_lifecycle_CPU"]["passed"]
        storage = driver.get("events", {}).get("planned_storage") or {}
        export_start, export_end = storage.get("export_started_offset_ns"), storage.get("export_finished_offset_ns")
        if isinstance(export_start, int) and isinstance(export_end, int) and 0 <= export_start < export_end:
            result["driver_export_CPU"] = cpu_window(samples, domain.label, start + export_start, start + export_end)
            result["driver_export_CPU"]["scope"] = "Sample-bracketed entire driver cgroup during EOF CSV/gzip export, including main and Go runtime; not pure compressor SELF CPU. Included in full-lifecycle gate, not added again."
            result["driver_export_elapsed_seconds"] = (export_end - export_start) / 1e9
        else:
            result["driver_export_CPU"] = {"available": False, "reason": "No explicit completed EOF export interval"}
    h.write_json(output/"result.json", result)
    return result


def run(spec):
    root, output = Path(spec["root"]), Path(spec["output"])
    output.mkdir(exist_ok=False)
    os.chown(output, 501, 1000)  # A diagnostic export may be written after drain.
    os.sched_setaffinity(0, {5, 6, 7})
    for key in ("binary", "driver", "protocol"):
        if identity(spec[key]["path"]) != spec[key]:
            raise ValueError(key+" identity mismatch before launch")
    helper_identity = {"window": identity(__file__), "common": identity(h.__file__)}
    if spec.get("harness") is not None and helper_identity != spec["harness"]:
        raise ValueError("harness identity mismatch before launch")
    manifest = {"uid": 501, "gid": 1000, "cgroup_root": str(root/"not-a-cgroup"),
                "protocol": spec["protocol"]["path"], "driver_cpuset": "2-4",
                "stacks": {spec["stack"]: {"command": [spec["binary"]["path"]],
                    "runtime_layout": {"binary_dir": "bin", "fs_base_dir": "bin"}}
                    if spec["stack"] == "ku" else {"command": [spec["binary"]["path"]]}},
                "service_env": {"KU_EVENT_LOOPS": "2", "KU_BLOCKING_THREADS": "17",
                    "KU_THREAD_BUDGET": "64", "KU_TASK_MAX_BYTES": "67108864"}}
    manifest["cgroup_root"] = "/sys/fs/cgroup/ku-architecture-"+root.name
    local_root = output/"manifest"
    local_root.mkdir()
    h.write_json(local_root/"manifest.json", manifest)
    harness = h.Harness(local_root, output/"harness")
    result = {"spec": spec, "started_ns": time.monotonic_ns(), "phases": [],
              "harness": helper_identity,
              "diagnostic_only": spec.get("diagnostic_only", False), "errors": []}
    trace_pages = []
    trace_enabled = spec["stack"] == "rust" and spec.get("env", {}).get("BENCH_CPU_TRACE") == "1"
    if trace_enabled and not result["diagnostic_only"]:
        raise ValueError("Rust trace must be a separate diagnostic window")
    service = None
    held = []
    def metrics():
        value = service.metrics()
        if trace_enabled:
            trace_pages.append(value.get("cpu_trace", {}))
        return value
    try:
        service = harness.start(spec["stack"], 2, h.GIB, output/"service", spec.get("env", {}))
        result["startup"] = service.startup
        # Controlled idle FD count is a diagnostic input, never a hidden
        # business load. Keep the window below the original 5s idle timeout.
        count = spec.get("held_idle_connections", 0)
        if count:
            if not spec.get("diagnostic_only") or sum(p["seconds"] for p in spec["phases"]) > 4:
                raise ValueError("held idle diagnostic must finish within 4s")
            host, port = service.address.rsplit(":", 1)
            for i in range(count):
                sock = socket.create_connection((host, int(port)), timeout=2)
                held.append(sock)
                rid = "idle"+str(i)
                sock.sendall(("GET /ok?rid="+rid+" HTTP/1.1\r\nHost: local\r\nConnection: keep-alive\r\n\r\n").encode())
                data = b""
                expected = ("ok:"+rid).encode()
                while not data.endswith(expected):
                    chunk = sock.recv(4096)
                    if not chunk or len(data)+len(chunk) > 16384:
                        raise RuntimeError("idle holder response incomplete")
                    data += chunk
            result["held_idle_connections"] = count
        result["metrics_before"] = metrics()
        for index, phase in enumerate(spec["phases"]):
            profile = None
            log = None
            if spec.get("profile"):
                if not result["diagnostic_only"]:
                    raise ValueError("CPU sampling is diagnostic only")
                log = (output/f"perf-{index}.log").open("w")
                command = ["perf", "record", "-e", "cpu-clock", "-F", "99",
                           "--call-graph", "dwarf,16384", "--sample-cpu", "-k", "CLOCK_MONOTONIC",
                           "-p", str(service.process.pid), "-o", str(output/f"perf-{index}.data")]
                profile = subprocess.Popen(command, stdout=log, stderr=log)
                time.sleep(.1)
                if profile.poll() is not None:
                    raise RuntimeError("perf failed before load; see retained log")
                (output/f"maps-{index}.txt").write_text(Path(f"/proc/{service.process.pid}/maps").read_text())
            try:
                result["phases"].append(client(harness, service, spec, output/f"phase-{index}", phase))
            finally:
                if profile is not None:
                    profile.send_signal(signal.SIGINT)
                    try:
                        profile.wait(timeout=5)
                    except subprocess.TimeoutExpired:
                        profile.kill()
                        profile.wait(timeout=3)
                    log.close()
            if profile is not None:
                for name, command in (("header", ["perf", "report", "--header-only", "--stdio"]),
                                      ("script", ["perf", "script", "--ns", "-F", "+period,+symoff"]),
                                      ("report", ["perf", "report", "--stdio", "--no-children", "--percent-limit", "0"])):
                    with (output/f"perf-{index}-{name}.txt").open("w") as log:
                        p = subprocess.run(command+["-i", str(output/f"perf-{index}.data")],
                                           stdout=log, stderr=subprocess.STDOUT, timeout=30)
                        if p.returncode:
                            result["errors"].append("perf_"+name+"_export_failed")
        result["metrics_with_holders_after"] = metrics()
        for sock in held:
            sock.close()
        held.clear()
        deadline = time.monotonic()+5
        while time.monotonic() < deadline:
            value = metrics()
            if h.quiet_metrics(spec["stack"], value, require_database_metrics=False):
                break
            time.sleep(.05)
        result["metrics_after"] = value
        result["quiet_after"] = h.quiet_metrics(spec["stack"], value, require_database_metrics=False)
        if trace_enabled:
            # Metrics drains at most 256 entries. Capture every page, including
            # snapshots already consumed above, after the scored arrival window.
            for _ in range(128):
                if trace_pages[-1].get("pending") == 0:
                    break
                metrics()
            events = [event for page in trace_pages for event in page.get("events", [])]
            sequences = [event["sequence"] for event in events]
            complete = (bool(trace_pages) and trace_pages[-1].get("pending") == 0
                        and all(page.get("enabled") and page.get("dropped") == 0 for page in trace_pages)
                        and sequences == list(range(1, trace_pages[-1].get("last_sequence", -1)+1)))
            h.write_json(output/"rust-trace.json", {"pages": trace_pages, "complete": complete})
            result["rust_trace"] = {"complete": complete, "pages": len(trace_pages), "events": len(events)}
            if not complete:
                result["errors"].append("rust_trace_incomplete")
    except BaseException as error:
        result["errors"].append(repr(error))
        raise
    finally:
        for sock in held:
            sock.close()
        result["cleanup"] = harness.close()
        result["finished_ns"] = time.monotonic_ns()
        h.write_json(output/"result.json", result)
    return result


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--spec", type=Path, required=True)
    args = parser.parse_args()
    spec = h.read_json(args.spec)
    if not 0 < sum(x["seconds"] for x in spec["phases"]) <= 1900:
        parser.error("total window duration must be 0..1900s")
    with (Path(spec["root"])/"measurement.lock").open("w") as lock:
        fcntl.flock(lock, fcntl.LOCK_EX|fcntl.LOCK_NB)
        result = run(spec)
    print(json.dumps({"output": spec["output"], "errors": result["errors"],
                     "qualification": [p["qualification"] for p in result["phases"]]}))


if __name__ == "__main__":
    main()
