#!/usr/bin/env python3
"""Bounded business-pressure and hot-computation fairness acceptance helpers.

The caller supplies a running private capacity fixture and its measured CPU
capacity. These helpers never build, change timeouts, replay a failed request,
or infer received traffic from a planned arrival count.
"""

import http.client
import math
import threading
import time

from capacity_cpu import run_client
from runtime_matrix import call, recover, zero_inflight


PROBE_INTERVAL_SECONDS = .1
PROBE_LIMIT = 600
PROBE_TIMEOUT_SECONDS = 1
FAIRNESS_LIMIT_MS = 250
RECOVERY_LIMIT_SECONDS = 5
HTTP_LIMITS = {"connections": 256, "active_requests": 128,
               "pending_requests": 128, "buffer_bytes": 8388608}


def _finite(value):
    return isinstance(value, (int, float)) and not isinstance(value, bool) and math.isfinite(value)


def _validate_seconds(seconds):
    if not _finite(seconds) or not 2 <= seconds <= 45:
        raise ValueError("seconds must be finite and in 2..45")


def _exception(error):
    return type(error).__name__ + ": " + str(error)[:256]


def _close(holders):
    for holder in holders.values():
        connection = holder.pop("connection", None)
        if connection is not None:
            connection.close()


def _summary(rows):
    counts = {}
    values = []
    for row in rows:
        counts[row["category"]] = counts.get(row["category"], 0) + 1
        values.append(row["elapsed_ms"])
    values.sort()

    def percentile(fraction):
        return values[max(0, math.ceil(len(values) * fraction) - 1)] if values else None

    return {"completed": len(rows), "counts": counts, "p50_ms": percentile(.5),
            "p95_ms": percentile(.95), "p99_ms": percentile(.99),
            "max_ms": values[-1] if values else None,
            "correct": counts.get("success", 0),
            "first_errors": [row for row in rows if row["category"] != "success"][:12]}


def _probe(service, path, expected, holder, origin, target=None):
    began = time.monotonic()
    category, elapsed, detail = call(service, path, expected, holder, timeout=PROBE_TIMEOUT_SECONDS)
    ended = time.monotonic()
    return {"path": path, "category": category, "elapsed_ms": elapsed * 1000,
            "start_monotonic": began, "end_monotonic": ended,
            "start_seconds": began - origin, "end_seconds": ended - origin,
            "schedule_lag_ms": None if target is None else max(0, began - target) * 1000,
            "detail": detail}


def _safe_recover(service, baseline):
    began = time.monotonic()
    try:
        result = recover(service, baseline)
        category, elapsed, detail = call(service, "/ok", b"ok-ready", timeout=PROBE_TIMEOUT_SECONDS)
        final = service.metrics()
        bytes_valid = _finite(final.get("buffer_bytes")) and _finite(baseline.get("buffer_bytes"))
        resources_zero = zero_inflight(final) and bytes_valid and final["buffer_bytes"] <= baseline["buffer_bytes"]
        result.update(ordinary_probe={"category": category, "elapsed_ms": elapsed * 1000, "detail": detail},
                      final_metrics=final, resource_zero=resources_zero,
                      observation_seconds=time.monotonic() - began)
        result["within_recovery_observation_budget"] = result["observation_seconds"] <= RECOVERY_LIMIT_SECONDS
        result["passed"] = result.get("passed") is True and category == "success" and resources_zero and result["within_recovery_observation_budget"]
        return result
    except (OSError, ValueError, RuntimeError, http.client.HTTPException) as error:
        return {"passed": False, "error": _exception(error), "observation_seconds": time.monotonic() - began}


def _complete_normal(result):
    ledger = result.get("client", {}).get("ledger", {})
    outcomes = ledger.get("results", {})
    started = ledger.get("started", 0)
    return result.get("measurement_valid") is True and started > 0 and ledger.get("not_started") == 0 and outcomes.get("correct_success") == started


def _resource_edges(result):
    peaks = result.get("metric_peaks", {})
    outcomes = result.get("server_delta", {}).get("responses_prepared", {})
    rejected = outcomes.get("rejected_responses")
    simultaneous_connections = []
    for row in result.get("samples", []):
        metrics = row.get("metrics", {})
        connections, pending = metrics.get("http_connections"), metrics.get("http_pending")
        if _finite(connections) and _finite(pending):
            simultaneous_connections.append(connections + pending)
    witnesses = {
        "active_task_slots_reached": peaks.get("task_resident", 0) >= HTTP_LIMITS["active_requests"],
        "active_sync_slots_reached": peaks.get("http_resident", 0) >= HTTP_LIMITS["active_requests"],
        "pending_slots_reached": peaks.get("http_pending", 0) >= HTTP_LIMITS["pending_requests"],
        "connection_slots_reached": bool(simultaneous_connections) and max(simultaneous_connections) >= HTTP_LIMITS["connections"],
        "buffer_limit_reached": peaks.get("http_buffer_bytes", 0) >= HTTP_LIMITS["buffer_bytes"],
        "server_admission_rejected": _finite(rejected) and rejected > 0,
    }
    return {"observed": any(witnesses.values()), "witnesses": witnesses,
            "configured_fixture_limits": dict(HTTP_LIMITS), "sampled_peaks": peaks,
            "sampled_active_plus_pending_connections_max": max(simultaneous_connections) if simultaneous_connections else None,
            "server_rejected_responses": rejected,
            "scope": "Only the fixed /work fixture is used: it returns a computed 200 or execution failure, never an application-defined 503. Infrastructure 503 confirms admission rejection; sampled occupancy need not catch its instantaneous maximum. It does not uniquely identify which admission quota rejected."}


def classify_pressure_evidence(result):
    """Describe bounded existing evidence without promoting a timeout to 504.

    This also accepts an already saved run_pressure result; it neither sends a
    diagnostic request nor changes the frozen acceptance gates.
    """
    overload = result.get("overload", {})
    client = overload.get("client", {})
    ledger = client.get("ledger", {})
    categories = ledger.get("results", {})
    stages, kinds = ledger.get("failure_stages", {}), ledger.get("failure_kinds", {})
    transport = categories.get("transport_or_timeout_failure")
    all_header_deadlines = _finite(transport) and transport > 0 and stages.get("read_status") == transport and kinds.get("timeout") == transport
    first_errors = client.get("first_errors") or []
    coverage = {}
    for row in first_errors:
        key = row.get("sample_category") or (str(row.get("category")) + "/" + str(row.get("failure_kind", "")))
        coverage[key] = coverage.get(key, 0) + 1
    samples = overload.get("samples", [])
    trace = []
    previous = None
    for row in samples:
        metrics = row.get("metrics", {})
        current = {"elapsed_seconds": row.get("elapsed_seconds"),
                   "active_connections": metrics.get("http_connections"),
                   "pending_connections": metrics.get("http_pending"),
                   "ready_tasks": metrics.get("task_ready"),
                   "resident_tasks": metrics.get("task_resident"),
                   "complete_business_requests_cumulative": metrics.get("http_requests"),
                   "task_polls_cumulative": metrics.get("task_polls"),
                   "event_cpu_cumulative_ns": metrics.get("http_cpu_ns"),
                   "event_cpu_sample_age_ms_max": metrics.get("event_cpu_sample_age_ms_max")}
        if previous is not None and all(_finite(value) for value in (current["elapsed_seconds"], previous["elapsed_seconds"], current["event_cpu_cumulative_ns"], previous["event_cpu_cumulative_ns"])):
            elapsed = current["elapsed_seconds"] - previous["elapsed_seconds"]
            cpu = current["event_cpu_cumulative_ns"] - previous["event_cpu_cumulative_ns"]
            current["approx_event_cores_since_previous_sample"] = cpu / 1e9 / elapsed if elapsed > 0 and cpu >= 0 else None
        trace.append(current)
        previous = current
    if len(trace) > 12:
        trace = trace[:6] + trace[-6:]
    normal = result.get("normal_precheck", {})
    normal_cpu = normal.get("server_delta", {}).get("event_cpu_seconds")
    normal_correct = normal.get("client", {}).get("ledger", {}).get("results", {}).get("correct_success")
    overloaded_cpu = overload.get("server_delta", {}).get("event_cpu_seconds")
    received = overload.get("server_delta", {}).get("complete_requests_received")
    return {"client_deadline_ms": client.get("configuration", {}).get("per_request_timeout_ms"),
            "all_transport_failures_are_complete_header_deadlines": all_header_deadlines,
            "header_deadline_count": transport if all_header_deadlines else None,
            "request_writes_complete": ledger.get("request_write_completed"),
            "request_writes_partial": ledger.get("request_write_partial"),
            "no_request_bytes_written": ledger.get("no_request_bytes_written"),
            "client_http_statuses": ledger.get("http_statuses"),
            "server_responses_prepared": overload.get("server_delta", {}).get("responses_prepared"),
            "sample_coverage": coverage, "bounded_occupancy_cpu_trace": trace,
            "normal_event_cpu_ms_per_correct_work": normal_cpu / normal_correct * 1000 if _finite(normal_cpu) and _finite(normal_correct) and normal_correct > 0 else None,
            "overload_event_cpu_ms_per_complete_business_request": overloaded_cpu / received * 1000 if _finite(overloaded_cpu) and _finite(received) and received > 0 else None,
            "normalization_scope": "CPU ratios include runtime/HTTP overhead and different concurrency. The overloaded denominator includes ordinary probes; these ratios do not isolate a single computation's instruction cost.",
            "confirmed_meaning": "read_status is the whole complete-response-header read/parse stage. A timeout here does not prove that zero response bytes or no status line arrived. Complete client writes do not individually prove service reception. A client deadline is not an HTTP 504 or a proved server rejection.",
            "unresolved_boundary": "Existing aggregate counters do not identify each timed-out request's queue, Task execution, response-preparation and delivery timestamps. Prepared 200 responses can coexist with client timeouts; they do not prove successful delivery or uniquely match a particular timeout.",
            "competing_process_cpu": "Not captured by this helper; reduced event-core use alone does not identify desktop rendering or another competing process.",
            "next_narrow_capture": "Compare the same-work poll-batching A/B first. If complete-header deadlines remain, retain at most four timeout-category client samples including request ID and consumed header bytes; correlate only a few selected requests' complete-receive/admit/first-execute/prepare/write/peer-close stages in a separate bounded diagnostic run. Do not increase deadlines or replay failures."}


def run_pressure(service, client, path, expected, measured_capacity_rps, seconds=30):
    """Check normal supply, then separately apply fixed open business pressure."""
    _validate_seconds(seconds)
    if not _finite(measured_capacity_rps) or measured_capacity_rps <= 0:
        raise ValueError("measured_capacity_rps must be finite and positive")
    normal_rate = measured_capacity_rps * .5
    overload_rate = min(10000, max(1, math.ceil(measured_capacity_rps * 5)))
    plan = {"normal_precheck_seconds": 2, "normal_rate": normal_rate,
            "overload_seconds": seconds, "overload_rate": overload_rate,
            "capacity_reference_rps": measured_capacity_rps,
            "minimum_overload_capacity_multiple": 3, "chosen_multiple": overload_rate / measured_capacity_rps,
            "client_workers": 512, "client_queue": 256,
            "request_path": path, "ordinary_probe_interval_seconds": PROBE_INTERVAL_SECONDS,
            "ordinary_probe_maximum_samples": PROBE_LIMIT,
            "recovery_limit_seconds": RECOVERY_LIMIT_SECONDS,
            "scope": "Business arrival pressure only. No incomplete-body connection flood is mixed into this result."}
    report = {"status": "running", "frozen_plan": plan}
    if overload_rate < 3 * measured_capacity_rps:
        report.update(status="insufficient_rate_range", passed=False,
                      reason="The fixed 10000/s client cap cannot offer at least three times the supplied capacity; no load was started.")
        return report
    baseline = service.metrics()
    report["baseline"] = baseline
    normal = run_client(service, client, path, expected, 2, concurrency=512, rate=normal_rate, queue=256)
    report["normal_precheck"] = normal
    report["normal_precheck_recovery"] = _safe_recover(service, baseline)
    if not _complete_normal(normal) or not report["normal_precheck_recovery"].get("passed"):
        report.update(status="normal_precheck_failed", passed=False,
                      reason="The independently scheduled normal precheck did not complete all actual starts correctly or recover; the 30-second overload was not started.")
        return report
    probes = []
    stop = threading.Event()
    load_started = time.monotonic()
    holder = {}

    def ordinary_probe():
        index = 0
        try:
            while index < PROBE_LIMIT and not stop.is_set():
                target = load_started + index * PROBE_INTERVAL_SECONDS
                if stop.wait(max(0, target - time.monotonic())):
                    break
                probes.append(_probe(service, "/ok", b"ok-ready", holder, load_started, target))
                index += 1
                # Do not burst stale probe slots after a slow response.
                if time.monotonic() - target > PROBE_INTERVAL_SECONDS:
                    index = min(PROBE_LIMIT, max(index, math.floor((time.monotonic() - load_started) / PROBE_INTERVAL_SECONDS)))
        finally:
            _close({"ordinary": holder})

    thread = threading.Thread(target=ordinary_probe, name="ku-capacity-ordinary-probe", daemon=True)
    thread.start()
    overloaded = {}
    try:
        overloaded = run_client(service, client, path, expected, seconds, concurrency=512, rate=overload_rate, queue=256)
        load_returned = time.monotonic()
        report["overload"] = overloaded
    except (OSError, ValueError, RuntimeError, http.client.HTTPException) as error:
        load_returned = time.monotonic()
        report["overload_error"] = _exception(error)
    finally:
        stop.set()
        thread.join(timeout=5)
        if thread.is_alive():
            report["probe_did_not_stop"] = True
    report["ordinary_probes"] = {"summary": _summary(probes), "samples": probes,
                                  "scope": "Independent bounded /ok observations. Errors during deliberate overload are classified separately, never silently retried."}
    report["resource_edges"] = _resource_edges(overloaded)
    ledger = overloaded.get("client", {}).get("ledger", {})
    started = ledger.get("started")
    not_started = ledger.get("not_started")
    report["offered_pressure"] = {"planned": ledger.get("planned"), "not_started": not_started,
                                  "not_started_reasons": ledger.get("not_started_reasons"),
                                  "started": started, "client_final_results": ledger.get("results"),
                                  "http_statuses": ledger.get("http_statuses"),
                                  "failure_stages": ledger.get("failure_stages"),
                                  "failure_kinds": ledger.get("failure_kinds"),
                                  "actual_started_rps": started / seconds if _finite(started) else None,
                                  "target_rate_fully_started": not_started == 0 and _finite(started) and started / seconds >= overload_rate * .99}
    server = overloaded.get("server_delta", {})
    received = server.get("complete_requests_received")
    before = overloaded.get("service_before", {}).get("begin_monotonic", load_started)
    after = overloaded.get("service_after", {}).get("end_monotonic", load_returned)
    overlapping = [row for row in probes if row["start_monotonic"] <= after and row["end_monotonic"] >= before]
    report["server_pressure"] = {"complete_requests_received_all_traffic": received,
                                 "overlapping_probe_attempts_upper_bound": len(overlapping),
                                 "work_requests_received_lower_bound": max(0, received - len(overlapping)) if _finite(received) else None,
                                 "work_requests_received_upper_bound": received,
                                 "admitted_all_traffic": server.get("admitted"),
                                 "responses_prepared_all_traffic": server.get("responses_prepared"),
                                 "scope": "Server deltas include the ordinary probe. Its possible contribution is bounded rather than subtracted as if every attempted probe were received. Transport failures remain separate from HTTP 503."}
    recovery = _safe_recover(service, baseline)
    recovered_at = time.monotonic()
    earliest_removal, latest_removal = min(load_started + seconds, load_returned), load_returned
    lower = max(0, recovered_at - latest_removal)
    upper = max(0, recovered_at - earliest_removal)
    recovery.update(withdrawal_time_interval_monotonic=[earliest_removal, latest_removal],
                    seconds_since_withdrawal_lower_bound=lower, seconds_since_withdrawal_upper_bound=upper,
                    timing_scope="The Go independent arrival window ends after run_client was invoked and before it returns. This conservative interval includes its launch/cleanup uncertainty; recovery is certified within five seconds only when the upper bound fits.")
    recovery["within_withdrawal_budget_confirmed"] = recovery.get("passed") is True and upper <= RECOVERY_LIMIT_SECONDS
    report["recovery"] = recovery
    result_classes = ledger.get("results", {})
    # Observed transport errors are retained and prevent closure until the
    # caller supplies a supported classification of their concrete cause.
    no_unexplained_failures = all(result_classes.get(key) == 0 for key in ("other_http_failure", "transport_or_timeout_failure", "unfinished_at_cutoff"))
    report["passed"] = overloaded.get("measurement_valid") is True and report["offered_pressure"]["target_rate_fully_started"] and report["resource_edges"]["observed"] and recovery["within_withdrawal_budget_confirmed"] and no_unexplained_failures and not report.get("probe_did_not_stop")
    report["status"] = "passed" if report["passed"] else "not_closed"
    report["remaining_checks"] = {"ordinary_capacity_precheck_valid": _complete_normal(normal),
                                   "overload_target_actual_starts": report["offered_pressure"]["target_rate_fully_started"],
                                   "server_resource_edge_observed": report["resource_edges"]["observed"],
                                   "unexplained_client_failure_absent": no_unexplained_failures,
                                   "recovery_withdrawal_budget_confirmed": recovery["within_withdrawal_budget_confirmed"]}
    report["failure_evidence"] = classify_pressure_evidence(report)
    return report


def run_fairness(service, client, path, expected, seconds=10):
    """Keep four ready CPU clients running while other routes finish in place."""
    _validate_seconds(seconds)
    baseline = service.metrics()
    report = {"status": "running", "frozen_plan": {"seconds": seconds, "hot_workers": 4,
              "hot_path": path, "probe_routes": ["/ok", "/timer", "/file"],
              "probe_interval_seconds": PROBE_INTERVAL_SECONDS, "latency_limit_ms": FAIRNESS_LIMIT_MS,
              "window_seconds": 1, "maximum_probe_samples": PROBE_LIMIT,
              "scope": "The /file fixture reads one small regular file through the existing background adapter. This is not heavy blocking-work capacity or arbitrary async database coverage."}, "baseline": baseline}
    done = threading.Event()
    hot = {}
    launched = time.monotonic()

    def hot_work():
        try:
            hot["result"] = run_client(service, client, path, expected, seconds, concurrency=4, rate=0, queue=0)
        except (OSError, ValueError, RuntimeError, http.client.HTTPException) as error:
            hot["error"] = _exception(error)
        finally:
            done.set()

    thread = threading.Thread(target=hot_work, name="ku-capacity-hot-client", daemon=True)
    thread.start()
    holders = {route: {} for route in ("/ok", "/timer", "/file")}
    samples = []
    startup = []
    ready = False
    # No business probe opens a socket before all four hot connections have
    # been observed. With a clean fixture their accepts follow owner rotation.
    ready_deadline = min(launched + 3, launched + seconds - .5)
    try:
        while time.monotonic() < ready_deadline and not done.is_set():
            snapshot = service.metrics()
            connections = sum(row.get("connections", 0) for row in snapshot.get("loops", []))
            resident = sum(row.get("resident", 0) for row in snapshot.get("tasks", []))
            startup.append({"elapsed_seconds": time.monotonic() - launched, "connections": connections,
                            "task_resident": resident,
                            "owner_connections": [{"id": row.get("id"), "connections": row.get("connections")} for row in snapshot.get("loops", [])]})
            if connections >= 4 and resident >= 1:
                ready = True
                break
            done.wait(.02)
        report["hot_observed_ready"] = ready
        report["hot_startup_observations"] = startup[:128]
        probe_origin = time.monotonic()
        # Go starts after the hot thread is launched. Until launched+seconds it
        # cannot have finished its requested measurement normally. Finishing
        # before this conservative bound proves overlap without guessing its
        # process startup time from a Python thread's alive flag.
        overlap_deadline = launched + seconds
        dispatch_deadline = overlap_deadline - .5
        choices = [("/ok", b"ok-ready"), ("/timer", b"timer-ready"), ("/file", b"file-ready")]
        index = 0
        while ready and index < PROBE_LIMIT and not done.is_set() and time.monotonic() < dispatch_deadline:
            target = probe_origin + index * PROBE_INTERVAL_SECONDS
            if done.wait(max(0, target - time.monotonic())):
                break
            route, body = choices[index % len(choices)]
            row = _probe(service, route, body, holders[route], probe_origin, target)
            row["completed_during_guaranteed_hot_window"] = row["end_monotonic"] < overlap_deadline and not done.is_set()
            row["window"] = int(row["end_seconds"])
            samples.append(row)
            index += 1
    except (OSError, ValueError, RuntimeError, http.client.HTTPException) as error:
        report["probe_error"] = _exception(error)
    finally:
        _close(holders)
        thread.join(timeout=seconds + 45)
        if thread.is_alive():
            report["hot_thread_did_not_stop"] = True
    report["hot"] = hot.get("result")
    if "error" in hot:
        report["hot_error"] = hot["error"]
    eligible = [row for row in samples if row["completed_during_guaranteed_hot_window"]]
    routes = {route: _summary([row for row in eligible if row["path"] == route]) for route in holders}
    windows = []
    if samples:
        for number in range(max(row["window"] for row in samples) + 1):
            rows = [row for row in eligible if row["window"] == number]
            windows.append({"window": number, "routes": {route: _summary([row for row in rows if row["path"] == route]) for route in holders},
                            "scope": "Completions within this one-second window, conservatively inside the continuing hot measurement."})
    report["probes"] = {"samples": samples, "eligible_summary": _summary(eligible), "routes": routes,
                         "one_second_windows": windows,
                         "outside_hot_window_samples": len(samples) - len(eligible)}
    report["recovery"] = _safe_recover(service, baseline)
    hot_result = hot.get("result", {})
    hot_ledger = hot_result.get("client", {}).get("ledger", {})
    hot_outcomes = hot_ledger.get("results", {})
    all_hot_correct = hot_ledger.get("started", 0) > 0 and hot_outcomes.get("correct_success") == hot_ledger.get("started")
    route_passes = {route: row["completed"] > 0 and row["correct"] == row["completed"] and row["p99_ms"] <= FAIRNESS_LIMIT_MS for route, row in routes.items()}
    full_windows = [row for row in windows if row["window"] + 1 <= (samples[-1]["end_seconds"] if samples else 0)]
    window_progress = bool(full_windows) and all(all(route["correct"] > 0 for route in row["routes"].values()) for row in full_windows)
    report["checks"] = {"hot_observed_ready": ready, "hot_measurement_valid": hot_result.get("measurement_valid") is True,
                         "hot_computation_correct": all_hot_correct, "routes_within_latency_budget": route_passes,
                         "full_windows_all_routes_progress": window_progress,
                         "no_late_or_failed_probe_hidden": len(eligible) == len(samples),
                         "recovery": report["recovery"].get("passed") is True}
    report["passed"] = ready and hot_result.get("measurement_valid") is True and all_hot_correct and all(route_passes.values()) and window_progress and len(eligible) == len(samples) and report["recovery"].get("passed") is True and not report.get("probe_error") and not report.get("hot_thread_did_not_stop")
    report["status"] = "passed" if report["passed"] else "not_closed"
    return report
