#!/usr/bin/env python3
"""B01--B08 bounded native capacity acceptance; no production services.

The original sustained fixture and its deadlines remain independent. All native
sources, executables, peer sockets and client binaries live in one TemporaryDirectory.
Only compact input identities, measurements and bounded diagnostic traces survive.
"""
import argparse
import hashlib
import http.client
import json
import os
from pathlib import Path
import platform
import shutil
import signal
import socket
import statistics
import subprocess
import sys
import tempfile
import time
import urllib.request

sys.dont_write_bytecode = True
from multiloop_load import Service, atomic_json
from runtime_matrix import machine_environment
from capacity_cpu import measurement_environment


def digest(path):
    return hashlib.sha256(Path(path).read_bytes()).hexdigest()


def command(args, **kwargs):
    result = subprocess.run(args, capture_output=True, text=True, timeout=180, **kwargs)
    if result.returncode:
        raise RuntimeError(f"command failed {args[0]}: {result.stderr[-6000:]}")
    return result.stdout[-3000:]


def build(ku, root, peer_port):
    repository = Path(__file__).resolve().parents[2]
    fixture = repository / "tests/fixtures/multicore_capacity.ku"
    source = root / "capacity.ku"
    source.write_text(fixture.read_text().replace("__PEER_PORT__", str(peer_port)))
    (root / "capacity.txt").write_text("file-ready")
    executable = root / ("capacity.exe" if os.name == "nt" else "capacity")
    output = command([str(ku), "build", "--backend", "c", "--profile", "release",
                      "--emit-c", "-o", str(executable), str(source)], cwd=root)
    generated = list(root.rglob("capacity.c"))
    return executable, {"fixture_sha256": digest(fixture), "expanded_source_sha256": digest(source),
                        "native_sha256": digest(executable), "build_output": output,
                        "generated_c_sha256": [digest(p) for p in generated]}


def baseline_natives(root):
    """Same final frontend/fixture/flags; replace only the named runtime(s).

    These are explicitly derived comparison artifacts, never final products or
    claims that the entire final compiler was built from the historical commit.
    The immutable old source is read locally first, otherwise from this public
    repository. No copied runtime or comparison binary is retained in the repo.
    """
    reference = "c1de5e3245bff62eb0b422d5c39a5212a1d1723b"
    repo = Path(__file__).resolve().parents[2]
    old, hashes = {}, {}
    for name in ("c_http_runtime.c", "c_task_driver.rs", "c_task_reactor.c"):
        path = "src/backend/" + name
        local = subprocess.run(["git", "show", reference+":"+path], cwd=repo,
                               capture_output=True, timeout=5)
        if local.returncode == 0:
            data = local.stdout
        else:
            with urllib.request.urlopen("https://raw.githubusercontent.com/fengyanweb/ku/"+reference+"/"+path, timeout=20) as response:
                data = response.read(1048577)
            if len(data) > 1048576:
                raise RuntimeError("comparison source exceeds fixed read bound")
        old[name] = data.decode()
        hashes[path] = hashlib.sha256(data).hexdigest()
    current_driver = (repo / "src/backend/c_task_driver.rs").read_text().split('const DRIVER_ABI: &str = r#"', 1)[1].rsplit('"#;', 1)[0]
    old_driver = old["c_task_driver.rs"].split('const DRIVER_ABI: &str = r#"', 1)[1].rsplit('"#;', 1)[0]
    current_http = (repo / "src/backend/c_http_runtime.c").read_text()
    current_reactor = (repo / "src/backend/c_task_reactor.c").read_text()
    generated = list(root.rglob("capacity.c"))
    if len(generated) != 1:
        raise RuntimeError("ambiguous generated comparison source")
    final_c = generated[0].read_text()
    if final_c.count(current_driver) != 1 or final_c.count(current_http) != 1 or final_c.count(current_reactor) != 1:
        raise RuntimeError("CLI and working-tree runtime text differ; rebuild the actual CLI")
    before_driver = final_c.replace(current_driver, old_driver, 1)
    entry = "ku_task_driver_begin_quantum(&instance->ticket,&instance->control)"
    changed_entries = before_driver.count(entry)
    if not changed_entries:
        raise RuntimeError("comparison requires the candidate generated entry")
    before_driver = before_driver.replace(entry, "ku_task_driver_set_intent(&instance->ticket,KU_TASK_DRIVER_YIELD)")
    marker = "    ku_task_driver_note_quantum_yield(yield_host->ticket,yield_host->control,__ku_charge_unchanged);"
    marker_count = before_driver.count(marker)
    if not marker_count:
        raise RuntimeError("comparison requires the actual dispatch-budget marker")
    before_driver = before_driver.replace("    KuTaskAdapterHostV1* yield_host=(KuTaskAdapterHostV1*)clock->host;\n"+marker+"\n", "")
    scalar = "ku_task_driver_scalar_quantum(&instance->ticket,&instance->control)"
    scalar_count = before_driver.count(scalar)
    before_driver = before_driver.replace(scalar, "(0)")
    before_both = before_driver.replace(current_http, old["c_http_runtime.c"], 1)
    before_scan = final_c.replace(current_reactor, old["c_task_reactor.c"], 1)
    outputs, manifest = {}, {"reference": reference, "old_source_sha256": hashes,
                            "restored_adapter_entries": changed_entries,
                            "restored_dispatch_markers": marker_count,
                            "disabled_scalar_reconcile_fast_paths": scalar_count,
                            "scope": "Final frontend and fixture; each derived variant replaces only its named historical runtime (driver, driver+HTTP, or reactor); all other runtimes remain final. Not whole historical compiler builds. Release C flags unchanged (-O2)."}
    for name, text in (("before_driver", before_driver), ("before_both", before_both), ("before_scan", before_scan)):
        source, executable = root / (name+".c"), root / name
        source.write_text(text)
        args = ["clang", str(source), "-std=c11", "-o", str(executable), "-O2", "-pthread"]
        command(args, cwd=root)
        outputs[name] = executable
        manifest[name] = {"generated_c_sha256": digest(source), "native_sha256": digest(executable), "command": args}
    return outputs, manifest


def compare(executable, baselines, client, root, checkpoint=None):
    from capacity_cpu import checksum, run_client, work_path
    result = []
    # Interleave both variants and N; short A/B is separate from formal 30s CPU.
    for repetition, order in enumerate(((1, 2, 4), (4, 1, 2), (2, 4, 1))):
        for loops in order:
            for pair in (
                (("before_driver", baselines["before_driver"], work_path(128), checksum(128)),
                 ("before_scan", baselines["before_scan"], work_path(128), checksum(128)),
                 ("final_driver", executable, work_path(128), checksum(128))),
                (("before_both_legacy", baselines["before_both"], "/cpu", "cpu-ready"),
                 ("final_legacy", executable, "/cpu", "cpu-ready")),
            ):
                for name, binary, path, expected in pair[::1 if repetition % 2 == 0 else -1]:
                    concurrency = 8 if path == "/cpu" else 32
                    service = Service(binary, root, loops)
                    row = {"variant": name, "loops": loops, "repetition": repetition,
                           "client_concurrency": concurrency}
                    try:
                        row["warmup"] = run_client(service, client, path, expected, 1, concurrency=concurrency)
                        row["measurement"] = run_client(service, client, path, expected, 5, concurrency=concurrency)
                    finally:
                        row["exit"] = service.stop()
                    result.append(row)
                    if checkpoint:
                        checkpoint(result)
                    print(json.dumps({"phase": "short_comparison", "variant": name, "loops": loops,
                                      "repetition": repetition, "valid": row["measurement"]["measurement_valid"],
                                      "correct_rps": row["measurement"]["client"]["rates"]["correct_per_measurement_second"]}), flush=True)
    return result


def trace_exchange(service, count=12):
    connection = http.client.HTTPConnection("127.0.0.1", service.port, timeout=2)
    samples = []
    try:
        for index in range(count):
            start = time.monotonic_ns()
            connection.request("GET", "/cpu", headers={"Connection": "keep-alive"})
            submitted = time.monotonic_ns()
            response = connection.getresponse()
            headers = time.monotonic_ns()
            body = response.read(65536)
            ended = time.monotonic_ns()
            samples.append({"index": index, "submit_ns": submitted-start,
                            "headers_after_submit_ns": headers-submitted,
                            "body_after_headers_ns": ended-headers, "total_ns": ended-start,
                            "status": response.status, "body": body.decode(errors="replace")})
            if response.status != 200 or body != b"cpu-ready":
                raise RuntimeError(f"diagnostic unexpected response: {samples[-1]}")
    finally:
        connection.close()
    return samples


def lock_probe(executable, client, root):
    """Two-second final-source diagnosis; never a throughput/scaling sample."""
    from capacity_cpu import checksum, run_client, work_path
    previous = os.environ.get("KU_TASK_DRIVER_TIMING")
    os.environ["KU_TASK_DRIVER_TIMING"] = "1"
    try:
        service = Service(executable, root, 4)
    finally:
        if previous is None:
            os.environ.pop("KU_TASK_DRIVER_TIMING", None)
        else:
            os.environ["KU_TASK_DRIVER_TIMING"] = previous
    result = {"scope": "Final-source bounded lock diagnosis; per-lock clock overhead perturbs performance. Excluded from timed capacity and comparison medians."}
    try:
        result["measurement"] = run_client(service, client, work_path(128), checksum(128), 2, concurrency=32)
    finally:
        result["exit"] = service.stop()
    return result


def diagnose(executable, root, evidence):
    service = Service(executable, root, 1)
    probes = []
    result = {"kind": "finite header/body and own-loopback trace", "instrumentation": []}
    try:
        if platform.system() == "Linux":
            for name, args in (
                ("packets", ["tcpdump", "-i", "lo", "-n", "-tt", "-l", "-s", "128", "-c", "240",
                             "tcp", "port", str(service.port)]),
                ("syscalls", ["strace", "-f", "-ttt", "-e", "trace=sendto,sendmsg,recvfrom,poll,ppoll",
                              "-p", str(service.child.pid)]),
            ):
                if shutil.which(args[0]):
                    log = (root / (name + ".txt")).open("wb")
                    prefix = [] if os.geteuid() == 0 else ["sudo", "-n"]
                    child = subprocess.Popen(prefix+args, stdout=log, stderr=log, start_new_session=True)
                    probes.append((name, child, log))
                    result["instrumentation"].append(name)
            # Startup synchronization only; this is outside the measured requests.
            time.sleep(.2)
        result["samples"] = trace_exchange(service)
        result["metrics"] = service.metrics()
    finally:
        for name, child, log in probes:
            if child.poll() is None:
                try:
                    os.killpg(child.pid, signal.SIGINT)
                    child.wait(timeout=3)
                except (OSError, subprocess.TimeoutExpired):
                    subprocess.run(["sudo", "-n", "kill", "-INT", "--", "-"+str(child.pid)], timeout=3, check=False)
                    child.wait(timeout=3)
            log.close()
            data = (root / (name + ".txt")).read_bytes()
            # Fixed trace bound: do not upload an unbounded diagnostic stream.
            (evidence / (name + ".txt")).write_bytes(data[:262144])
            result[name] = {"bytes": len(data), "retained_bytes": min(len(data), 262144),
                            "exit": child.returncode}
        result["exit"] = service.stop()
    result["passed"] = result["exit"] == 0
    return result


def collect_waiting(service, peer, seconds, result, save):
    from capacity_wait import run_network_waiting, run_waiting
    result["waiting_deadlines"] = {"request_timeout_ms": service.request_timeout_ms,
        "handler_timeout_ms": 45000, "runtime_default_request_timeout_ms": 30000,
        "original_sustained_handler_timeout_ms": 1000}
    result["waiting"] = run_waiting(service, seconds=seconds)
    save(result)
    result["network_waiting"] = run_network_waiting(service, peer, seconds=seconds)
    save(result)


def waiting_only(executable, root, args, save, peer):
    result = {"scope": "Only timer 32/64/128 and native TCP 64 waiting groups; CPU, A/B, fairness, overload and connection occupancy are not rerun."}
    service = Service(executable, root, 4, request_timeout_ms=45000)
    try:
        collect_waiting(service, peer, args.waiting, result, save)
    finally:
        result["waiting_exit"] = service.stop()
    return result


def capacity(ku, executable, client, root, args, save, peer):
    from capacity_cpu import checksum, run_client, work_path
    from capacity_wait import run_connection_occupancy
    from capacity_pressure import run_fairness, run_pressure
    result = {"calibration": [], "rounds": [], "legacy": []}
    service = Service(executable, root, 1)
    try:
        # Freeze the first bounded workload costing >=10 ms event CPU/request.
        # All later N configurations use exactly this same input and response.
        for units in (32, 128, 512, 1000):
            value = run_client(service, client, work_path(units), checksum(units), 3, concurrency=1)
            result["calibration"].append({"work_units": units, **value})
            save(result)
            if value["server_delta"].get("event_cpu_seconds", 0) / max(1, value["client"]["ledger"]["results"].get("correct_success", 0)) >= .01:
                break
        result["frozen_work_units"] = units
    finally:
        service.stop()
    path, expected = work_path(units), checksum(units)
    for repetition in range(args.repeat):
        for loops in ((1, 2, 4), (4, 1, 2), (2, 4, 1))[repetition % 3]:
            service = Service(executable, root, loops)
            item = {"repetition": repetition, "loops": loops}
            try:
                item["warmup"] = run_client(service, client, path, expected, args.warmup, concurrency=32)
                item["measurement"] = run_client(service, client, path, expected, args.duration, concurrency=32)
            finally:
                item["exit"] = service.stop()
            result["rounds"].append(item)
            save(result)
            print(json.dumps({"phase": "formal_cpu", "repetition": repetition, "loops": loops,
                              "correct_rps": item["measurement"]["client"]["rates"]["correct_per_measurement_second"],
                              "valid": item["measurement"]["measurement_valid"]}), flush=True)
    for loops in (1, 2, 4):
        service = Service(executable, root, loops)
        item = {"loops": loops}
        try:
            item["measurement"] = run_client(service, client, "/cpu", "cpu-ready", 5, concurrency=8)
        finally:
            item["exit"] = service.stop()
        result["legacy"].append(item)
        save(result)
    service = Service(executable, root, 4, request_timeout_ms=45000)
    try:
        collect_waiting(service, peer, args.waiting, result, save)
    finally:
        result["waiting_exit"] = service.stop()
    # The long-wait fixture's dedicated deadline must not leak into the
    # ordinary fairness/overload/connection groups. Keep their original 30s
    # whole-request limit and the independent Go 3s client deadline.
    service = Service(executable, root, 4, request_timeout_ms=30000)
    result["ordinary_request_timeout_ms"] = service.request_timeout_ms
    try:
        result["fairness"] = run_fairness(service, client, path, expected)
        save(result)
        measured = [r["measurement"]["client"]["rates"]["correct_per_measurement_second"] for r in result["rounds"] if r["loops"] == 4]
        import statistics
        result["business_overload"] = run_pressure(service, client, path, expected, statistics.median(measured), seconds=30)
        save(result)
        result["connection_occupancy"] = run_connection_occupancy(service, seconds=30)
        save(result)
    finally:
        result["capacity_exit"] = service.stop()
    return result


def assessment(report):
    """Apply the thresholds frozen in plan, never reinterpret a failed sample."""
    if report.get("plan", {}).get("mode") == "waiting":
        data = report.get("capacity", {})
        timer, network = data.get("waiting", {}), data.get("network_waiting", {})
        levels = timer.get("levels", [])
        # At 128 the existing contract records a capacity boundary. Its ordinary
        # probe and guaranteed-capacity observations are not required gates.
        boundary_checks = ("ledger_balanced", "no_unfinished", "management_available", "recovered",
            "resource_samples_valid", "native_threads_fixed", "waiting_cpu_within_existing_idle_limit",
            "descriptors_recovered", "rss_recovered")
        checks = {"formal_waiting_plan": report["plan"].get("waiting") == 30,
            "timer_levels_32_64_128": [row.get("target") for row in levels] == [32, 64, 128],
            "timer_waiting": timer.get("passed") is True and timer.get("formal_30_second_windows") is True,
            "network_target_64": network.get("target") == 64,
            "network_waiting": network.get("passed") is True and network.get("formal_30_second_window") is True,
            "network_all_checks": bool(network.get("checks")) and all(value is True for value in network["checks"].values()),
            "waiting_service_exit": data.get("waiting_exit") == 0,
            "dedicated_wait_deadlines": data.get("waiting_deadlines", {}).get("request_timeout_ms") == 45000
                and data.get("waiting_deadlines", {}).get("handler_timeout_ms") == 45000}
        for row in levels:
            required = row.get("checks", {})
            names = boundary_checks if row.get("target") == 128 else boundary_checks + (
                "all_requests_correct", "actual_timer_target_observed", "timer_target_maintained_at_samples",
                "ordinary_requests_correct", "ordinary_p99_within_existing_limit")
            checks[f"timer_{row.get('target')}_all_required_checks"] = (row.get("passed") is True
                and row.get("formal_30_second_window") is True and bool(required)
                and all(required.get(name) is True for name in names))
        return {"scope": "Only the dedicated timer/TCP waiting groups. This supplement does not revalidate CPU, A/B, fairness, overload, connection occupancy, other platforms or the complete release.",
            "checks": checks, "passed": all(checks.values()),
            "failed": [name for name, passed in checks.items() if not passed]}
    result = {"scope": "B03--B06 local-platform gates; CI/correctness/source audit remain separate"}
    data = report.get("capacity", {})
    groups = {n: [r for r in data.get("rounds", []) if r["loops"] == n] for n in (1, 2, 4)}
    def correct(row):
        value = row.get("measurement", {})
        ledger = value.get("client", {}).get("ledger", {})
        started = ledger.get("started", 0)
        reasons = ledger.get("not_started_reasons", {})
        # Closed workers may create one final plan just before the deadline,
        # then observe cutoff before starting it. It is retained as unstarted,
        # never a request failure or claimed server arrival.
        cutoff_only = (ledger.get("not_started", 0) <= 32
                       and reasons.get("client_queue_full", 0) == 0
                       and reasons.get("scheduler_at_cutoff", 0) == 0
                       and ledger.get("not_started", 0) == reasons.get("queued_or_closed_at_cutoff", 0))
        return (row.get("exit") == 0 and value.get("measurement_valid") is True
                and started > 0 and cutoff_only
                and ledger.get("results", {}).get("correct_success") == started
                and value.get("server_delta", {}).get("complete_requests_received") == started)
    summary = {}
    for n, rows in groups.items():
        rates = [r["measurement"]["client"]["rates"]["correct_per_measurement_second"] for r in rows]
        cores = [r["measurement"]["server_delta"].get("average_event_cores_over_observation_window") for r in rows]
        summary[n] = {"rates": rates, "median_rps": statistics.median(rates) if rates else None,
                      "event_cores": cores, "all_samples_correct": bool(rows) and all(map(correct, rows))}
    result["cpu"] = summary
    complete = all(len(groups[n]) == 3 for n in groups)
    one, two, four = (summary[n] for n in (1, 2, 4))
    enough = complete and all(r["all_samples_correct"] for r in summary.values())
    def client_headroom(row):
        m = row["measurement"]
        cpu = m.get("client_cpu", {}).get("seconds")
        # wait4 gives Go process-lifetime CPU. Its own measurement+cleanup wall
        # is conservative (startup is not credited) and cannot be diluted by
        # native/Python samples or a slow observer join after Go has exited.
        wall = m.get("client", {}).get("timing", {}).get("with_cleanup_seconds")
        return (isinstance(cpu, (int, float)) and not isinstance(cpu, bool)
                and 0 <= cpu < float("inf")
                and isinstance(wall, (int, float)) and not isinstance(wall, bool)
                and 0 < wall < float("inf") and cpu / wall <= .5)
    checks = {
        "formal_cpu_plan": report["plan"]["duration"] == 30 and report["plan"]["warmup"] == 5 and report["plan"]["repeat"] == 3,
        "nine_correct_reconciled_samples": enough,
        "go_generator_below_half_core": enough and all(client_headroom(r) for rows in groups.values() for r in rows),
        "one_owner_compute_bound": enough and min(one["event_cores"]) >= .65,
        "four_owner_real_multicore": enough and min(four["event_cores"]) > 1.1,
        "four_owner_gain_exceeds_round_range": enough and min(four["rates"]) > max(one["rates"]),
        "four_owner_median_gain_at_least_ten_percent": enough and four["median_rps"] >= one["median_rps"] * 1.10,
        "two_owner_no_material_regression": enough and two["median_rps"] >= one["median_rps"] * .95,
        "four_owner_no_material_regression_from_two": enough and four["median_rps"] >= two["median_rps"] * .95,
        "owners_all_execute": enough and all(all(o.get("complete_requests_received", 0) > 0 for o in r["measurement"]["server_delta"]["event_threads"]) for rows in groups.values() for r in rows),
        "timer_waiting": data.get("waiting", {}).get("passed") is True and data.get("waiting", {}).get("formal_30_second_windows") is True,
        "network_waiting": data.get("network_waiting", {}).get("passed") is True and data.get("network_waiting", {}).get("formal_30_second_window") is True,
        "hot_fairness": data.get("fairness", {}).get("passed") is True,
        "business_overload": data.get("business_overload", {}).get("passed") is True,
        "connection_occupancy": data.get("connection_occupancy", {}).get("passed") is True
            and data.get("connection_occupancy", {}).get("formal_30_second_windows") is True,
        "waiting_service_exit": data.get("waiting_exit") == 0,
        "capacity_service_exit": data.get("capacity_exit") == 0,
        "ordinary_request_deadline_unchanged": data.get("ordinary_request_timeout_ms") == 30000,
    }
    comparison = report.get("short_comparison", [])
    paired = {}
    for variant in ("before_driver", "before_scan", "final_driver", "before_both_legacy", "final_legacy"):
        paired[variant] = {}
        for n in (1, 2, 4):
            rows = [r for r in comparison if r["variant"] == variant and r["loops"] == n]
            rates = [r["measurement"]["client"]["rates"]["correct_per_measurement_second"] for r in rows]
            paired[variant][n] = {"rates": rates, "median_rps": statistics.median(rates) if rates else None,
                                 "all_correct": len(rows) == 3 and all(map(correct, rows))}
    result["short_comparison"] = paired
    pair_valid = all(row["all_correct"] for variant in paired.values() for row in variant.values())
    checks["comparison_correct"] = pair_valid
    checks["candidate_driver_benefit"] = pair_valid and paired["final_driver"][4]["median_rps"] >= paired["before_driver"][4]["median_rps"] * 1.05
    checks["reactor_scan_benefit"] = pair_valid and paired["final_driver"][4]["median_rps"] >= paired["before_scan"][4]["median_rps"] * 1.05
    checks["legacy_final_no_material_four_owner_regression"] = pair_valid and paired["final_legacy"][4]["median_rps"] >= paired["final_legacy"][1]["median_rps"] * .95
    result.update(checks=checks, passed=all(checks.values()), failed=[name for name, passed in checks.items() if not passed])
    return result


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--ku", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--mode", choices=("diagnose", "capacity", "all", "waiting"), default="capacity")
    parser.add_argument("--go", default="go")
    parser.add_argument("--duration", type=float, default=30)
    parser.add_argument("--warmup", type=float, default=5)
    parser.add_argument("--repeat", type=int, default=3)
    parser.add_argument("--waiting", type=float, default=30)
    args = parser.parse_args()
    if not (0 < args.duration <= 60 and 0 < args.warmup <= 10 and 1 <= args.repeat <= 3 and 0 < args.waiting <= 30):
        parser.error("duration/repetition outside bounded acceptance plan")
    args.ku = args.ku.resolve()
    args.output = args.output.resolve()
    args.output.parent.mkdir(parents=True, exist_ok=True)
    repo = Path(__file__).resolve().parents[2]
    report = {"source_commit": command(["git", "rev-parse", "HEAD"], cwd=repo).strip(),
              "source_dirty": bool(command(["git", "status", "--porcelain"], cwd=repo)),
              "cli_sha256": digest(args.ku), "environment": machine_environment(),
              "capacity_environment": measurement_environment(),
              "plan": vars(args) | {"ku": str(args.ku), "output": str(args.output),
                 "loops": [4] if args.mode == "waiting" else [1, 2, 4],
                 "client_concurrency": None if args.mode == "waiting" else 32, "total_background_threads": 5,
                 "thread_budget": 16, "task_bytes": 16777216,
                 "dedicated_wait_handler_deadline_ms": 45000,
                 "dedicated_wait_request_deadline_ms": 45000,
                 "runtime_default_request_deadline_ms": 30000,
                 "original_sustained_handler_deadline_ms": 1000}}
    report["plan"]["frozen_gates"] = {"normal_errors": 0, "go_generator_maximum_one_core_fraction": .5,
        "n1_minimum_event_cores": .65, "n4_minimum_event_cores": 1.1,
        "n4_over_n1_median_minimum": 1.10, "n4_round_minimum_above_n1_round_maximum": True,
        "material_median_regression_fraction": .05, "candidate_driver_minimum_median_gain": .05,
        "fairness_p99_ms": 250, "recovery_seconds": 5,
        "scope": "Conservative local acceptance criteria frozen before traffic, not universal performance promises. Noisy or CPU-limited environments fail/invalidate rather than receiving relaxed thresholds."}
    report["plan"]["frozen_gates"]["closed_client_cutoff"] = "At most one final unstarted plan per worker (32); only the explicit cutoff bucket. No queue rejection, request failure or false server-arrival credit is allowed. Open-arrival checks remain separate."
    if args.mode == "waiting":
        report["plan"]["frozen_gates"] = {"timer_targets": [32, 64, 128], "network_target": 64,
            "observation_seconds_per_group": 30, "waiting_idle_one_core_limit_percent": 5,
            "ordinary_p99_ms": 250, "recovery_seconds": 5,
            "scope": "Existing timer/TCP checks only, including the existing 128-timer capacity-boundary rule; no other capacity gate is rerun or declared passed."}
    save = lambda value=None: atomic_json(args.output, report)
    save()
    from capacity_wait import NetworkPeer
    with tempfile.TemporaryDirectory(prefix="ku-multicore-capacity-") as directory, NetworkPeer() as peer:
        root = Path(directory)
        executable, identity = build(args.ku, root, peer.port)
        report["build"] = identity
        save()
        def checkpoint(value):
            report["capacity"] = value
            save()
        if args.mode == "waiting":
            report["capacity"] = waiting_only(executable, root, args, checkpoint, peer)
            report["assessment"] = assessment(report)
            save()
        else:
            if args.mode in ("diagnose", "all"):
                report["diagnosis"] = diagnose(executable, root, args.output.parent)
                save()
            client = root / ("client.exe" if os.name == "nt" else "client")
            report["go_version"] = command([args.go, "version"]).strip()
            command([args.go, "build", "-trimpath", "-o", str(client),
                     str(repo / "tests/acceptance/capacity_client.go")], cwd=root)
            report["client_sha256"] = digest(client)
            baseline, baseline_manifest = baseline_natives(root)
            report["comparison_inputs"] = baseline_manifest
            save()
            def comparison_checkpoint(value):
                report["short_comparison"] = value
                save()
            report["short_comparison"] = compare(executable, baseline, client, root, comparison_checkpoint)
            preliminary = assessment(report)
            report["short_comparison_gate"] = {name: preliminary["checks"][name] for name in (
                "comparison_correct", "candidate_driver_benefit", "reactor_scan_benefit", "legacy_final_no_material_four_owner_regression")}
            save()
            report["lock_diagnosis"] = lock_probe(executable, client, root)
            save()
            if args.mode in ("capacity", "all"):
                if all(report["short_comparison_gate"].values()):
                    report["capacity"] = capacity(args.ku, executable, client, root, args, checkpoint, peer)
                    report["assessment"] = assessment(report)
                else:
                    report["assessment"] = {"passed": False, "failed": ["short_comparison_gate"],
                        "reason": "Short comparison did not establish candidate benefit/correctness and legacy recovery. Formal capacity was not started; inspect retained samples before another run."}
                save()
    report["temporary_directory_removed"] = True
    save()
    print(json.dumps({"report": str(args.output), "source": report["source_commit"],
                      "mode": args.mode, "temporary_directory_removed": True}))
    if args.mode in ("capacity", "all", "waiting") and not report["assessment"]["passed"]:
        raise SystemExit("Frozen capacity gates failed: " + ", ".join(report["assessment"]["failed"]))


if __name__ == "__main__":
    main()
