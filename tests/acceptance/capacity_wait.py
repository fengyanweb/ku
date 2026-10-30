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
import threading
import time

from multiloop_load import process_sample
from runtime_matrix import PLAN, call, cpu_sample, recover, resource_sample_errors, zero_inflight


MAX_RESPONSE_BYTES = 65536
MAX_ERROR_SAMPLES = 20
PENDING_CONNECT = {0, errno.EINPROGRESS, errno.EWOULDBLOCK, errno.EALREADY,
                   getattr(errno, "WSAEWOULDBLOCK", 10035),
                   getattr(errno, "WSAEINPROGRESS", 10036),
                   getattr(errno, "WSAEALREADY", 10037)}


class NetworkPeer:
    """One bounded selector thread holds one-byte TCP replies until released.

    No connection owns a thread or a growing input buffer. release() affects
    the current held cohort; later connections wait for the next release.
    The event thread alone registers, reads, writes and closes accepted peers.
    """

    def __init__(self, limit=128):
        if not 1 <= limit <= 128:
            raise ValueError("network peer limit must be between 1 and 128")
        self.limit, self.port = limit, None
        self._lock = threading.Lock()
        self._ready, self._stop = threading.Event(), threading.Event()
        self._release_pending = 0
        self._thread = self._listener = self._wake_read = self._wake_write = None
        self._stats = {name: 0 for name in (
            "accepted", "active", "held", "peak_active", "peak_held", "received_k",
            "released_connections", "replied_k", "release_commands", "capacity_rejected",
            "closed_before_release", "protocol_errors", "transport_errors", "context_closes")}
        self._errors = []

    def __enter__(self):
        if self._thread is not None:
            raise RuntimeError("NetworkPeer contexts cannot be reentered")
        try:
            self._listener = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
            self._listener.bind(("127.0.0.1", 0))
            self._listener.listen(self.limit)
            self._listener.setblocking(False)
            self.port = self._listener.getsockname()[1]
            self._wake_read, self._wake_write = socket.socketpair()
            self._wake_read.setblocking(False)
            self._wake_write.setblocking(False)
            self._thread = threading.Thread(target=self._run, name="ku-capacity-network-peer", daemon=True)
            self._thread.start()
            if not self._ready.wait(2) or not self._thread.is_alive():
                raise RuntimeError("network peer did not start: " + repr(self.snapshot()))
            return self
        except BaseException:
            self._close()
            raise

    def _notify(self):
        if self._wake_write is not None:
            try:
                self._wake_write.send(b"!")
            except (BlockingIOError, BrokenPipeError, OSError):
                # A full wake channel already guarantees readability. Stop and
                # release predicates are checked after every finite select.
                pass

    def release(self):
        with self._lock:
            self._release_pending += 1
        self._notify()

    def snapshot(self):
        with self._lock:
            result = {**self._stats, "errors": list(self._errors)}
        result.update(connection_limit=self.limit, selector_threads=1,
                      thread_alive=self._thread is not None and self._thread.is_alive(),
                      input_bytes_per_connection=1, output_bytes_per_connection=1)
        return result

    def _error(self, category, error):
        with self._lock:
            self._stats[category] += 1
            if len(self._errors) < MAX_ERROR_SAMPLES:
                self._errors.append({"category": category, "detail": str(error)[:160]})

    def _run(self):
        selector, clients = None, {}

        def close_client(peer, reason=None):
            row = clients.pop(peer, None)
            if row is None:
                return
            try:
                selector.unregister(peer)
            except (KeyError, ValueError):
                pass
            peer.close()
            with self._lock:
                self._stats["active"] -= 1
                if row["held"]:
                    self._stats["held"] -= 1
                if reason:
                    self._stats[reason] += 1

        try:
            selector = selectors.DefaultSelector()
            selector.register(self._listener, selectors.EVENT_READ, "listener")
            selector.register(self._wake_read, selectors.EVENT_READ, "wake")
            self._ready.set()
            while not self._stop.is_set():
                for key, mask in selector.select(.1):
                    if key.data == "wake":
                        try:
                            while self._wake_read.recv(256):
                                pass
                        except (BlockingIOError, InterruptedError):
                            pass
                    elif key.data == "listener":
                        for _ in range(16):
                            try:
                                peer, _ = self._listener.accept()
                            except (BlockingIOError, InterruptedError):
                                break
                            with self._lock:
                                self._stats["accepted"] += 1
                            if len(clients) >= self.limit:
                                peer.close()
                                with self._lock:
                                    self._stats["capacity_rejected"] += 1
                                continue
                            try:
                                peer.setblocking(False)
                            except OSError as error:
                                peer.close()
                                self._error("transport_errors", error)
                                continue
                            clients[peer] = {"held": False, "received": False, "released": False}
                            with self._lock:
                                self._stats["active"] += 1
                                self._stats["peak_active"] = max(self._stats["peak_active"], self._stats["active"])
                            try:
                                selector.register(peer, selectors.EVENT_READ, clients[peer])
                            except (OSError, ValueError) as error:
                                self._error("transport_errors", error)
                                close_client(peer)
                    else:
                        peer, row = key.fileobj, key.data
                        try:
                            if mask & selectors.EVENT_READ:
                                data = peer.recv(1)
                                if not data:
                                    close_client(peer, "closed_before_release" if row["held"] else None)
                                    continue
                                if row["received"] or data != b"K":
                                    self._error("protocol_errors", "expected exactly one K")
                                    close_client(peer)
                                    continue
                                row["received"], row["held"] = True, True
                                with self._lock:
                                    self._stats["received_k"] += 1
                                    self._stats["held"] += 1
                                    self._stats["peak_held"] = max(self._stats["peak_held"], self._stats["held"])
                            if mask & selectors.EVENT_WRITE and row["released"]:
                                if peer.send(b"K") != 1:
                                    raise OSError("one-byte reply was not sent")
                                with self._lock:
                                    self._stats["replied_k"] += 1
                                close_client(peer)
                        except (BlockingIOError, InterruptedError):
                            pass
                        except OSError as error:
                            self._error("transport_errors", error)
                            close_client(peer)
                with self._lock:
                    release_count, self._release_pending = self._release_pending, 0
                    self._stats["release_commands"] += release_count
                if release_count:
                    for peer, row in clients.items():
                        if not row["held"]:
                            continue
                        row["held"], row["released"] = False, True
                        selector.modify(peer, selectors.EVENT_READ | selectors.EVENT_WRITE, row)
                        with self._lock:
                            self._stats["held"] -= 1
                            self._stats["released_connections"] += 1
        except BaseException as error:
            self._error("transport_errors", error)
        finally:
            self._ready.set()
            for peer in list(clients):
                close_client(peer, "context_closes")
            if selector is not None:
                selector.close()
            for peer in (self._listener, self._wake_read):
                if peer is not None:
                    peer.close()

    def _close(self):
        self._stop.set()
        self._notify()
        if self._thread is not None and self._thread.ident is not None:
            self._thread.join(3)
            if self._thread.is_alive():
                raise RuntimeError("network peer selector did not join within 3 seconds")
        else:
            for peer in (self._listener, self._wake_read):
                if peer is not None:
                    peer.close()
        if self._wake_write is not None:
            self._wake_write.close()

    def __exit__(self, kind, error, traceback):
        self._close()
        if kind is None and self.snapshot()["transport_errors"]:
            raise RuntimeError("network peer failed: " + repr(self.snapshot()))
        return False


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


def run_network_waiting(service, peer, seconds=30, target=64):
    """Keep real Task TCP reads parked, then release and consume every reply.

    The 40s native read/client budgets and the fixture's 45s handler deadline
    belong to this waiting experiment. They do not alter the formal HTTP plan.
    Recovery's existing 5s budget starts when the external peer is released,
    rather than after an additional client-completion window.
    """
    if not 0 < seconds <= 30 or not 1 <= target <= 64:
        raise ValueError("network waiting requires 0..30 seconds and 1..64 clients")
    peer_before = peer.snapshot()
    resources_before = cpu_sample(service.child.pid)
    began = time.monotonic()
    baseline = service.metrics()
    cohort = _Cohort(service, target,
                     b"GET /network HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
                     b"network-ready", 40)
    samples, probes, resources = [], [], []
    full_started, released_at = None, None
    observation_began = observation_end = time.monotonic()
    recovery = {"passed": False, "seconds": None, "deadline_seconds": 5}

    def sample():
        value = _metrics(service, began)
        value["peer"] = peer.snapshot()
        value["native_net_wait"] = value.get("metrics", {}).get("task_blocking", {}).get("net_wait", 0)
        samples.append(value)
        return value

    try:
        for _ in range(target):
            cohort.launch()
        admission_deadline = time.monotonic() + 3
        next_sample = time.monotonic()
        while time.monotonic() < admission_deadline:
            cohort.pump(.02)
            if time.monotonic() >= next_sample:
                latest = sample()
                if (latest.get("management_ok") and latest.get("waiting_tasks", 0) >= target
                        and latest["native_net_wait"] >= target and latest["peer"]["held"] >= target):
                    full_started = time.monotonic()
                    break
                next_sample = time.monotonic() + .1
        observation_began = time.monotonic()
        observation_end = observation_began + seconds if full_started is not None else observation_began
        next_sample = next_resource = observation_began
        while time.monotonic() < observation_end:
            cohort.pump(min(.1, max(0, observation_end - time.monotonic())))
            if time.monotonic() >= next_sample:
                sample()
                probe = call(service, "/ok", b"ok-ready", timeout=2)
                probes.append({"at_seconds": time.monotonic() - began,
                               "category": probe[0], "seconds": probe[1], "detail": probe[2]})
                next_sample = time.monotonic() + 1
            if time.monotonic() >= next_resource:
                resource = cpu_sample(service.child.pid)
                resource["at_seconds"] = time.monotonic() - began
                resources.append(resource)
                next_resource = time.monotonic() + 10
        observed_seconds = time.monotonic() - observation_began
        resources_waiting = cpu_sample(service.child.pid)
        resources_waiting["at_seconds"] = time.monotonic() - began
        resources.append(resources_waiting)
        sample()
        released_at = time.monotonic()
        peer.release()
        recovery_deadline = released_at + PLAN["recovery_deadline_seconds"]
        next_sample = released_at
        latest_recovery = {}
        while time.monotonic() < recovery_deadline:
            cohort.pump(.02)
            if time.monotonic() >= next_sample:
                latest_recovery = service.metrics()
                peer_recovery = peer.snapshot()
                if (not cohort.active and zero_inflight(latest_recovery)
                        and latest_recovery.get("buffer_bytes", 0) <= baseline.get("buffer_bytes", 0)
                        and peer_recovery["active"] == 0 and peer_recovery["held"] == 0):
                    recovery["passed"] = True
                    break
                next_sample = time.monotonic() + .05
        recovery.update(seconds=time.monotonic() - released_at,
                        metrics=latest_recovery, peer=peer.snapshot())
        cohort.close("unfinished_at_recovery_deadline")
    finally:
        if released_at is None:
            peer.release()
        cohort.close("closed_on_generator_exception")

    completion, peer_after = cohort.result(), peer.snapshot()
    resources_after = cpu_sample(service.child.pid)
    observation_samples = [row for row in samples
                           if row["at_seconds"] >= observation_began - began]
    resource_samples = [resources_before, *resources, resources_after]
    descriptor_key = "handles" if "handles" in resources_before else "fds"
    sampler_errors = [resource_sample_errors(row, descriptor_key, require_cpu=True)
                      for row in resource_samples]
    valid_resources = not any(sampler_errors)
    waiting_wall = resources_waiting["monotonic"] - resources_before["monotonic"]
    waiting_cpu = (resources_waiting["cpu_seconds"] - resources_before["cpu_seconds"]
                   if valid_resources else None)
    waiting_cpu_percent = 100 * waiting_cpu / waiting_wall if waiting_cpu is not None and waiting_wall > 0 else None
    ordinary_latencies = sorted(row["seconds"] for row in probes)
    ordinary_p99_ms = ordinary_latencies[min(len(ordinary_latencies) - 1,
                                            math.ceil(len(ordinary_latencies) * .99) - 1)] * 1000 if ordinary_latencies else None

    def peak(field):
        values = [row[field] for row in resource_samples
                  if isinstance(row.get(field), int) and not isinstance(row[field], bool)
                  and row[field] >= 0 and not row.get("sample_error")]
        return max(values) if values else None

    checks = {
        "peer_started_empty": peer_before["active"] == 0 and peer_before["held"] == 0,
        "actual_network_target_observed": full_started is not None,
        "network_target_maintained_at_samples": bool(observation_samples) and all(
            row.get("waiting_tasks", 0) >= target and row["native_net_wait"] >= target
            and row["peer"]["held"] >= target for row in observation_samples),
        "management_available": bool(samples) and all(row["management_ok"] for row in samples),
        "ordinary_requests_correct": bool(probes) and all(row["category"] == "success" for row in probes),
        "ordinary_p99_within_existing_limit": ordinary_p99_ms is not None
            and ordinary_p99_ms <= PLAN["ordinary_and_timer_p99_limit_ms"],
        "all_network_requests_correct": completion["counts"] == {"correct_success": target},
        "client_ledger_balanced": completion["ledger_balanced"] and completion["not_started"] == 0
            and completion["unfinished"] == 0,
        "peer_replied_to_all": peer_after["replied_k"] - peer_before["replied_k"] == target,
        "peer_no_errors": all(peer_after[name] == peer_before[name] for name in
            ("capacity_rejected", "closed_before_release", "protocol_errors", "transport_errors")),
        "resource_samples_valid": valid_resources,
        "native_threads_fixed": valid_resources and all(row["threads"] <= resources_before["threads"] for row in resource_samples),
        "waiting_cpu_within_existing_idle_limit": waiting_cpu_percent is not None and waiting_cpu >= 0
            and waiting_cpu_percent <= PLAN["idle_one_core_cpu_limit_percent"],
        "recovered_within_existing_deadline": recovery["passed"] and recovery["seconds"] <= PLAN["recovery_deadline_seconds"],
        "descriptors_recovered": valid_resources and resources_after[descriptor_key] - resources_before[descriptor_key]
            <= PLAN["descriptor_recovered_growth_limit"],
        "rss_recovered": valid_resources and resources_after["rss_bytes"] - resources_before["rss_bytes"]
            <= PLAN["rss_recovered_growth_limit_bytes"],
    }
    return {"target": target, "requested_observation_seconds": seconds,
            "observed_seconds": observed_seconds, "formal_30_second_window": seconds >= 30 and observed_seconds >= 30,
            "admission_window_seconds": 3, "full_network_target_at_seconds": full_started - began if full_started else None,
            "native_read_timeout_seconds": 40, "request_client_timeout_seconds": 40,
            "fixture_handler_timeout_seconds": 45, "maximum_http_client_sockets": target + 2,
            "peer_connection_limit": peer.limit, "peer_before": peer_before, "peer_after": peer_after,
            "peer_counters_are_cumulative": True, "client": completion, "samples": samples,
            "ordinary_probes": probes, "ordinary_p99_ms": ordinary_p99_ms,
            "resources_before": resources_before, "resource_samples_during_wait": resources,
            "resources_after": resources_after, "resource_sample_errors": sampler_errors,
            "waiting_cpu_seconds": waiting_cpu, "waiting_cpu_wall_seconds": waiting_wall,
            "waiting_one_core_cpu_percent": waiting_cpu_percent,
            "waiting_cpu_limit_percent": PLAN["idle_one_core_cpu_limit_percent"],
            "peak_native_threads_at_samples": peak("threads"),
            "peak_native_rss_at_samples": peak("rss_bytes"),
            "peak_native_descriptors_at_samples": peak(descriptor_key),
            "recovery": recovery, "checks": checks, "passed": all(checks.values()),
            "meaning": "Actual native Task TCP read waits, jointly witnessed by held K replies, parked Tasks and native net_wait. One peer selector thread; no thread per connection. Client responses are read to completion. Resource peaks are sampled, not continuous maxima; process CPU includes the event and fixed background threads."}


def _timer_level(service, target, seconds):
    began = time.monotonic()
    before = cpu_sample(service.child.pid)
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
        during = cpu_sample(service.child.pid)
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
    after = cpu_sample(service.child.pid)
    descriptor_key = "handles" if "handles" in before else "fds"
    resource_errors = [resource_sample_errors(row, descriptor_key, require_cpu=True) for row in (before, during, after)]
    resources_valid = not any(resource_errors)
    idle_wall = during["monotonic"] - before["monotonic"] if resources_valid else 0
    idle_cpu = during["cpu_seconds"] - before["cpu_seconds"] if resources_valid else None
    idle_percent = idle_cpu / idle_wall * 100 if idle_cpu is not None and idle_wall > 0 else None
    probe_times = sorted(row["seconds"] * 1000 for row in probes)
    ordinary_p99 = probe_times[max(0, math.ceil(len(probe_times) * .99)-1)] if probe_times else None
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
              "ordinary_requests_correct": bool(probes) and all(row["category"] == "success" for row in probes),
              "ordinary_p99_within_existing_limit": ordinary_p99 is not None and ordinary_p99 <= 250,
              "resource_samples_valid": resources_valid,
              "native_threads_fixed": resources_valid and max(during["threads"], after["threads"]) <= before["threads"],
              "waiting_cpu_within_existing_idle_limit": idle_percent is not None and 0 <= idle_percent <= 5,
              "descriptors_recovered": resources_valid and after[descriptor_key] - before[descriptor_key] <= 2,
              "rss_recovered": resources_valid and after["rss_bytes"] - before["rss_bytes"] <= 16777216}
    passed = all(checks[name] for name in ("ledger_balanced", "no_unfinished", "management_available", "recovered", "resource_samples_valid", "native_threads_fixed", "waiting_cpu_within_existing_idle_limit", "descriptors_recovered", "rss_recovered"))
    if required_capacity:
        passed = passed and complete_success and full_started is not None and minimum_timers >= target and checks["ordinary_requests_correct"] and checks["ordinary_p99_within_existing_limit"]
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
            "resources_after": after, "resource_sample_errors": resource_errors,
            "waiting_one_core_cpu_percent": idle_percent, "ordinary_p99_ms": ordinary_p99,
            "recovery": recovery,
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


def run_connection_occupancy(service, seconds=30, levels=(64, 128, 256)):
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
        next_sample = began
        try:
            while time.monotonic() < began + seconds:
                # Refill in finite batches, rather than spending the complete
                # 1s server body deadline gradually creating the first cohort.
                for _ in range(32):
                    if len(cohort.active) >= target or cohort.started + cohort.not_started >= attempt_limit:
                        break
                    cohort.launch()
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
        # Active slots and the pending FIFO are disjoint native counters. One
        # connection is conservatively deducted for the metrics request itself.
        # A 256-client tier probes the configured admission boundary; it does
        # not claim that 256 incomplete requests remain concurrently admitted.
        minimum_occupancy = math.ceil(min(target, 128) * .9)
        for row in valid:
            row["partial_peer_occupancy_lower_bound"] = max(
                0, row["connections"] + row["pending_connections"] - 1)
        occupied = [row for row in valid
                    if row["partial_peer_occupancy_lower_bound"] >= minimum_occupancy]
        checks = {
            "all_management_samples_valid": bool(valid) and len(valid) == len(samples),
            "client_started_connected_and_sent_target": completion["started"] >= target
                and completion["connected"] >= target
                and completion["request_prefix_fully_sent"] >= target
                and completion["not_started"] == 0,
            "observed_real_body_wait_completion": completion["counts"].get("http_body_timeout", 0) > 0,
            "half_samples_reach_declared_occupancy": len(occupied) >= max(1, math.ceil(len(valid) / 2)),
            "late_window_reaches_declared_occupancy": any(row["at_seconds"] >= seconds * .8 for row in occupied),
            "no_unexpected_outcomes": not unexpected,
            "client_ledger_balanced": completion["ledger_balanced"],
            "recovered": recovery["passed"],
        }
        results.append({"target_client_socket_limit": target, "window_seconds": pressure_ended - began,
                        "total_phase_seconds": time.monotonic() - began,
                        "requested_window_seconds": seconds, "maximum_attempts": attempt_limit,
                        "formal_30_second_window": seconds == 30 and pressure_ended - began >= 30,
                        "launch_policy": "Refill available client slots in batches of at most 32 per selector turn; finite attempt cap.",
                        "maximum_launch_batch": 32,
                        "actual_started_per_second": completion["started"] / (pressure_ended - began),
                        "client": completion, "samples": samples, "ordinary_probes": probes,
                        "minimum_partial_peer_occupancy": minimum_occupancy,
                        "occupancy_samples_reaching_minimum": len(occupied),
                        "valid_occupancy_samples": len(valid),
                        "peak_partial_peer_occupancy_lower_bound": max(
                            (row["partial_peer_occupancy_lower_bound"] for row in valid), default=0),
                        "peak_server_connections": max((row["connections"] for row in valid), default=0),
                        "peak_server_pending_connections": max((row["pending_connections"] for row in valid), default=0),
                        "complete_business_requests_from_partial_body_peers": 0,
                        "resources_before": before, "resources_waiting": during,
                        "resources_after": process_sample(service.child.pid), "recovery": recovery,
                        "unexpected_outcomes": unexpected,
                        "checks": checks, "passed": all(checks.values()),
                        "meaning": "Incomplete 4000-byte bodies (one byte sent), refilled around the fixture's unchanged 1s body deadline. At least half of management samples and one in the final 20% must witness the declared lower bound after deducting one management connection. The 64/128 tiers require at least 58/116; 256 is an admission-boundary probe requiring at least 116, not proof of 256 simultaneous peers. At least one complete 408 proves a real body wait; all-503 and zero-start runs fail. Phase-end generator closes are explicit and are not successes."})
    return {"levels": results, "selector_threads": 0, "maximum_client_sockets": max(levels),
            "formal_30_second_windows": all(row["formal_30_second_window"] for row in results),
            "passed": all(row["passed"] for row in results)}
