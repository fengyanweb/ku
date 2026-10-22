#!/usr/bin/env python3
"""Bounded waiting-capacity checks for the existing native Service fixture.

The caller creates/stops Service. Timer responses are consumed normally after
the observation window; closing an outstanding socket never counts as success.
Partial-body occupancy is a separate connection experiment, not Task capacity.
"""
import errno
import http.client
import math
import selectors
import socket
import time

from multiloop_load import process_sample
from runtime_matrix import call, recover


MAX_RESPONSE_BYTES = 65536
MAX_ERROR_SAMPLES = 20
PENDING_CONNECT = {0, errno.EINPROGRESS, errno.EWOULDBLOCK, errno.EALREADY,
                   getattr(errno, "WSAEWOULDBLOCK", 10035),
                   getattr(errno, "WSAEINPROGRESS", 10036),
                   getattr(errno, "WSAEALREADY", 10037)}


class _Cohort:
    """One selector, fixed socket cap, bounded response buffers and evidence."""

    def __init__(self, service, limit, payload, expected, timeout, partial=False):
        if not 1 <= limit <= 256:
            raise ValueError("socket limit must be between 1 and 256")
        self.service, self.limit = service, limit
        self.payload, self.expected = payload, expected
        self.timeout, self.partial = timeout, partial
        self.selector = selectors.DefaultSelector()
        self.active = {}
        self.started = self.not_started = self.connected = self.sent = 0
        self.peak = self.peak_connected = 0
        self.counts, self.errors = {}, []
        self.elapsed_sum = 0.0
        self.elapsed_min = self.elapsed_max = None

    def launch(self):
        if len(self.active) >= self.limit:
            return False
        peer = None
        began = time.monotonic()
        try:
            peer = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
            peer.setblocking(False)
        except OSError as error:
            if peer is not None:
                peer.close()
            self.not_started += 1
            self._error("socket_not_created", {"error": type(error).__name__,
                                               "errno": error.errno})
            return False
        self.started += 1
        row = {"socket": peer, "began": began, "deadline": began + self.timeout,
               "connected": False, "offset": 0, "buffer": bytearray(),
               "status": None, "body_start": None, "body_length": None}
        self.active[peer] = row
        self.peak = max(self.peak, len(self.active))
        try:
            result = peer.connect_ex(("127.0.0.1", self.service.port))
            if result not in PENDING_CONNECT:
                self._finish(row, "connection_error", {"errno": result})
            else:
                self.selector.register(peer, selectors.EVENT_WRITE, row)
        except OSError as error:
            self._finish(row, "connection_error", {"error": type(error).__name__,
                                                    "errno": error.errno})
        return True

    def _error(self, category, detail):
        if len(self.errors) < MAX_ERROR_SAMPLES:
            self.errors.append({"category": category, **detail})

    def _finish(self, row, category, detail=None):
        peer = row["socket"]
        if peer not in self.active:
            return
        self.counts[category] = self.counts.get(category, 0) + 1
        elapsed = time.monotonic() - row["began"]
        self.elapsed_sum += elapsed
        self.elapsed_min = elapsed if self.elapsed_min is None else min(self.elapsed_min, elapsed)
        self.elapsed_max = elapsed if self.elapsed_max is None else max(self.elapsed_max, elapsed)
        if detail is not None:
            self._error(category, {"seconds": elapsed, "request_prefix_fully_sent":
                                  row["offset"] == len(self.payload), **detail})
        self.active.pop(peer)
        try:
            self.selector.unregister(peer)
        except (KeyError, ValueError):
            pass
        peer.close()

    def _response(self, row, eof=False):
        data = row["buffer"]
        if row["body_start"] is None:
            end = data.find(b"\r\n\r\n")
            if end < 0:
                if len(data) > 8192 or eof:
                    self._finish(row, "incomplete_response" if eof else "response_header_too_large",
                                 {"prefix": repr(bytes(data[:160]))})
                return
            try:
                lines = bytes(data[:end]).split(b"\r\n")
                protocol, status, *_ = lines[0].split(b" ")
                if protocol not in (b"HTTP/1.0", b"HTTP/1.1") or len(status) != 3 or not status.isdigit():
                    raise ValueError("invalid response status")
                headers = {}
                for line in lines[1:]:
                    name, value = line.split(b":", 1)
                    name = name.strip().lower()
                    if name in headers:
                        raise ValueError("duplicate response header")
                    headers[name] = value.strip()
                if b"transfer-encoding" in headers:
                    raise ValueError("fixture response must use Content-Length")
                length = int(headers[b"content-length"])
                if not 0 <= length <= MAX_RESPONSE_BYTES:
                    raise ValueError("response length outside client bound")
                row.update(status=int(status), body_start=end + 4, body_length=length)
            except (ValueError, KeyError) as error:
                self._finish(row, "invalid_response", {"detail": str(error)[:160],
                                                       "prefix": repr(bytes(data[:160]))})
                return
        total = row["body_start"] + row["body_length"]
        if len(data) < total:
            if eof:
                self._finish(row, "incomplete_response", {"status": row["status"],
                                                          "bytes": len(data), "expected_bytes": total})
            return
        body = bytes(data[row["body_start"]:total])
        status = row["status"]
        if status == 200 and not self.partial and body == self.expected:
            category = "correct_success"
        elif status in (429, 503):
            category = "http_rejected"
        elif self.partial and status == 408:
            category = "http_body_timeout"
        elif status == 200:
            category = "unexpected_success" if self.partial else "wrong_body"
        else:
            category = "other_http_failure"
        self._finish(row, category, None if category == "correct_success" else
                     {"status": status, "body": repr(body[:160])})

    def pump(self, timeout=.1):
        for key, mask in self.selector.select(max(0, min(.1, timeout))):
            row, peer = key.data, key.fileobj
            try:
                if mask & selectors.EVENT_WRITE:
                    if not row["connected"]:
                        error = peer.getsockopt(socket.SOL_SOCKET, socket.SO_ERROR)
                        if error:
                            self._finish(row, "connection_error", {"errno": error})
                            continue
                        row["connected"] = True
                        self.connected += 1
                        connected_now = sum(item["connected"] for item in self.active.values())
                        self.peak_connected = max(self.peak_connected, connected_now)
                    row["offset"] += peer.send(self.payload[row["offset"]:])
                    if row["offset"] == len(self.payload):
                        self.sent += 1
                        self.selector.modify(peer, selectors.EVENT_READ, row)
                if mask & selectors.EVENT_READ:
                    chunk = peer.recv(8192)
                    if chunk:
                        row["buffer"].extend(chunk)
                        if len(row["buffer"]) > MAX_RESPONSE_BYTES + 8192:
                            self._finish(row, "response_too_large", {"bytes": len(row["buffer"])})
                        else:
                            self._response(row)
                    else:
                        self._response(row, eof=True)
            except (BlockingIOError, InterruptedError):
                pass
            except OSError as error:
                self._finish(row, "transport_error", {"error": type(error).__name__,
                                                       "errno": error.errno})
        now = time.monotonic()
        for row in list(self.active.values()):
            if now >= row["deadline"]:
                self._finish(row, "client_timeout", {"response_status": row["status"]})

    def close(self, category):
        for row in list(self.active.values()):
            self._finish(row, category, {"response_status": row["status"]})
        self.selector.close()

    def result(self):
        completed = sum(self.counts.values())
        return {"planned": self.started + self.not_started, "not_started": self.not_started,
                "started": self.started, "connected": self.connected,
                "request_prefix_fully_sent": self.sent, "completed": completed,
                "unfinished": len(self.active), "counts": dict(self.counts),
                "peak_client_sockets": self.peak, "peak_connected_sockets": self.peak_connected,
                "socket_limit": self.limit, "response_buffer_limit_per_socket": MAX_RESPONSE_BYTES + 8192,
                "elapsed_seconds_min": self.elapsed_min, "elapsed_seconds_max": self.elapsed_max,
                "elapsed_seconds_mean": self.elapsed_sum / completed if completed else None,
                "first_errors": list(self.errors),
                "ledger_balanced": self.started == completed + len(self.active)}


def _metrics(service, began):
    started = time.monotonic()
    try:
        metrics = service.metrics()
        tasks = metrics.get("tasks", [])
        loops = metrics.get("loops", [])
        return {"at_seconds": started - began, "management_ok": True,
                "management_seconds": time.monotonic() - started,
                "timers": sum(row.get("timers", 0) for row in tasks),
                "waiting_tasks": sum(row.get("waiting", 0) for row in tasks),
                "ready_tasks": sum(row.get("ready", 0) for row in tasks),
                "running_tasks": sum(row.get("running", 0) for row in tasks),
                "connections": sum(row.get("connections", 0) for row in loops),
                "pending_connections": sum(row.get("pending", 0) for row in loops),
                "metrics": metrics}
    except (OSError, ValueError, RuntimeError, http.client.HTTPException) as error:
        return {"at_seconds": started - began, "management_ok": False,
                "management_seconds": time.monotonic() - started,
                "error": type(error).__name__, "detail": str(error)[:160]}


def _timer_level(service, target, seconds):
    began = time.monotonic()
    before = process_sample(service.child.pid)
    payload = b"GET /wait HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n"
    cohort = _Cohort(service, target, payload, b"wait-ready", 50)
    samples, probes = [], []
    full_started = None
    try:
        for _ in range(target):
            cohort.launch()
        admission_deadline = began + 3
        next_sample = began
        latest = None
        while time.monotonic() < admission_deadline:
            cohort.pump()
            if time.monotonic() >= next_sample:
                latest = _metrics(service, began)
                samples.append(latest)
                next_sample = time.monotonic() + .1
                if latest.get("timers", 0) >= target:
                    full_started = time.monotonic()
                    break
        observation_began = time.monotonic()
        observation_end = observation_began + seconds
        next_sample = observation_began
        while time.monotonic() < observation_end:
            cohort.pump(min(.1, max(0, observation_end - time.monotonic())))
            if time.monotonic() >= next_sample:
                sample = _metrics(service, began)
                samples.append(sample)
                if target <= 64:
                    probe = call(service, "/ok", b"ok-ready", timeout=2)
                    probes.append({"at_seconds": time.monotonic() - began,
                                   "category": probe[0], "seconds": probe[1], "detail": probe[2]})
                next_sample = time.monotonic() + 1
        observed_seconds = time.monotonic() - observation_began
        during = process_sample(service.child.pid)
        # The fixed fixture sleeps 35s. Continue reading; never turn phase-end
        # disconnects into successful waits or silently discard unfinished work.
        completion_deadline = began + 51
        while cohort.active and time.monotonic() < completion_deadline:
            cohort.pump()
        cohort.close("unfinished_at_client_budget")
    finally:
        cohort.close("closed_on_generator_exception")
    completion = cohort.result()
    recovery = recover(service)
    valid_samples = [row for row in samples if row.get("management_ok")]
    observation_samples = [row for row in valid_samples
                           if row["at_seconds"] >= observation_began - began]
    minimum_timers = min((row["timers"] for row in observation_samples), default=0)
    peak_timers = max((row["timers"] for row in valid_samples), default=0)
    complete_success = completion["counts"] == {"correct_success": target}
    required_capacity = target <= 64
    unexpected = {key: value for key, value in completion["counts"].items()
                  if key not in ("correct_success", "http_rejected")}
    management_ok = bool(samples) and all(row["management_ok"] for row in samples)
    checks = {"ledger_balanced": completion["ledger_balanced"],
              "no_unfinished": completion["unfinished"] == 0,
              "management_available": management_ok, "recovered": recovery["passed"],
              "all_requests_correct": complete_success,
              "actual_timer_target_observed": peak_timers >= target,
              "timer_target_maintained_at_samples": minimum_timers >= target,
              "ordinary_requests_correct": bool(probes) and all(row["category"] == "success" for row in probes)}
    passed = checks["ledger_balanced"] and checks["no_unfinished"] and management_ok and recovery["passed"]
    if required_capacity:
        passed = passed and complete_success and full_started is not None and minimum_timers >= target and checks["ordinary_requests_correct"]
    else:
        # At the active-request ceiling, explicit refusal is a measured limit,
        # not evidence of corruption. Unexpected outcomes remain visible and fail.
        passed = passed and not unexpected and completion["not_started"] == 0
    return {"target": target, "required_capacity_level": required_capacity,
            "requested_observation_seconds": seconds, "observed_seconds": observed_seconds,
            "formal_30_second_window": seconds >= 30 and observed_seconds >= 30,
            "fixture_timer_seconds": 35, "request_client_timeout_seconds": 50,
            "sample_interval_seconds": 1, "admission_window_seconds": 3,
            "peak_actual_timers": peak_timers, "minimum_observed_timers": minimum_timers,
            "full_timer_target_reached_at_seconds": full_started - began if full_started is not None else None,
            "client": completion, "ordinary_probes": probes, "samples": samples,
            "resources_before": before, "resources_waiting": during,
            "resources_after": process_sample(service.child.pid), "recovery": recovery,
            "checks": checks, "passed": bool(passed),
            "meaning": "Timer Task waits; full 200/body responses only count as successful completion. The 128 level reports a capacity boundary, not a guaranteed 128-waiter claim."}


def run_waiting(service, seconds=30):
    """Observe 32/64/128 timer cohorts; Service and fixture remain caller-owned.

    Default takes about 105s plus bounded setup/recovery. A shorter observation
    still waits for the fixed 35s replies and is explicitly diagnostic only.
    """
    if not 0 < seconds <= 30:
        raise ValueError("timer observation must be greater than zero and at most 30 seconds")
    levels = [_timer_level(service, target, seconds) for target in (32, 64, 128)]
    return {"levels": levels, "selector_threads": 0, "maximum_client_sockets": 128,
            "formal_30_second_windows": all(row["formal_30_second_window"] for row in levels),
            "passed": all(row["passed"] for row in levels),
            "network_waits_covered": False,
            "limits": "Management and ordinary probes run on the selector's controlling thread; their bounded call time can delay client reads. This driver does not claim an independent arrival-rate clock or Task TCP capacity."}


def run_connection_occupancy(service, seconds=5, levels=(64, 128, 256)):
    """Rotate bounded incomplete bodies; never count them as business requests."""
    if not 0 < seconds <= 30 or not levels or any(not 1 <= level <= 256 for level in levels):
        raise ValueError("occupancy requires 1..256 sockets and a 0..30 second window")
    results = []
    payload = b"POST /echo HTTP/1.1\r\nHost: localhost\r\nContent-Length: 4000\r\nConnection: close\r\n\r\nx"
    for target in levels:
        began = time.monotonic()
        cohort = _Cohort(service, target, payload, None, 5, partial=True)
        samples, probes = [], []
        before = process_sample(service.child.pid)
        attempt_limit = target * (math.ceil(seconds) + 2)
        next_launch = began
        next_sample = began
        maximum_lag = 0.0
        try:
            while time.monotonic() < began + seconds:
                now = time.monotonic()
                for _ in range(32):
                    if now < next_launch or len(cohort.active) >= target or cohort.started + cohort.not_started >= attempt_limit:
                        break
                    maximum_lag = max(maximum_lag, now - next_launch)
                    cohort.launch()
                    next_launch += 1 / target
                cohort.pump(.02)
                if time.monotonic() >= next_sample:
                    samples.append(_metrics(service, began))
                    probe = call(service, "/ok", b"ok-ready", timeout=2)
                    probes.append({"at_seconds": time.monotonic() - began,
                                   "category": probe[0], "seconds": probe[1], "detail": probe[2]})
                    next_sample = time.monotonic() + .5
            during = process_sample(service.child.pid)
            pressure_ended = time.monotonic()
            cohort.close("closed_by_generator_at_phase_end")
        finally:
            cohort.close("closed_on_generator_exception")
        completion = cohort.result()
        recovery = recover(service)
        valid = [row for row in samples if row.get("management_ok")]
        unexpected = {key: value for key, value in completion["counts"].items()
                      if key not in ("http_body_timeout", "http_rejected", "closed_by_generator_at_phase_end")}
        results.append({"target_client_socket_limit": target, "window_seconds": pressure_ended - began,
                        "total_phase_seconds": time.monotonic() - began,
                        "requested_window_seconds": seconds, "maximum_attempts": attempt_limit,
                        "planned_start_rate_per_second": target, "maximum_launch_batch": 32,
                        "actual_started_per_second": completion["started"] / (pressure_ended - began),
                        "maximum_start_schedule_lag_ms": maximum_lag * 1000,
                        "client": completion, "samples": samples, "ordinary_probes": probes,
                        "peak_server_connections": max((row["connections"] for row in valid), default=0),
                        "peak_server_pending_connections": max((row["pending_connections"] for row in valid), default=0),
                        "complete_business_requests_from_partial_body_peers": 0,
                        "resources_before": before, "resources_waiting": during,
                        "resources_after": process_sample(service.child.pid), "recovery": recovery,
                        "unexpected_outcomes": unexpected,
                        "passed": bool(valid) and all(row["management_ok"] for row in samples)
                            and not unexpected and completion["ledger_balanced"] and recovery["passed"],
                        "meaning": "Incomplete 4000-byte bodies (one byte sent), rotated around the fixture's 1s body deadline. Report observed peaks; the socket cap is not proof all sockets were concurrently held at the server. Phase-end generator closes are explicit and are not successes."})
    return {"levels": results, "selector_threads": 0, "maximum_client_sockets": max(levels),
            "passed": all(row["passed"] for row in results)}
