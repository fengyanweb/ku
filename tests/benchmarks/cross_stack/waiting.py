#!/usr/bin/env python3
"""Whole-service waiting capacity, using the shared Linux Harness API.

Selector/cohort and controlled-peer pattern derives from the original
tests/acceptance/capacity_wait.py SHA-256
3c48c454c434be03ccaaf3ce36e520d2574fd952496aef8434b0dbb83e9afca7.
The old driver is unchanged. No request is automatically replayed. This module
must execute on the benchmark Linux host, not through remote per-request calls.
"""
from __future__ import annotations

import argparse
import contextlib
import errno
import json
import math
import os
from pathlib import Path
import selectors
import socket
import threading
import time

LEVELS = (32, 64, 128, 512, 2048, 8192)
MAX_TARGET = 8192
MAX_RESPONSE = 65536
MAX_HEADER = 8192
MAX_SAMPLES = 96
PENDING_CONNECT = {0, errno.EINPROGRESS, errno.EWOULDBLOCK, errno.EALREADY}


class IDs:
    """One bounded, numeric request ID space shared by cohort and probes."""
    def __init__(self):
        self.prefix = time.time_ns() % 10000000
        self.index = 0
        self.lock = threading.Lock()

    def next(self):
        with self.lock:
            self.index += 1
            if self.index > 999999999:
                raise RuntimeError("request ID budget exceeded")
            return f"{self.prefix:07d}{self.index:09d}"


def address_tuple(address):
    host, port = address.rsplit(":", 1)
    return host, int(port)


def parse_response(data, eof=False):
    """Bounded complete Content-Length response; returns None while incomplete."""
    end = data.find(b"\r\n\r\n")
    if end < 0:
        if len(data) > MAX_HEADER or eof:
            raise ValueError("incomplete or oversized response header")
        return None
    if end > MAX_HEADER:
        raise ValueError("response header exceeds bound")
    lines = bytes(data[:end]).split(b"\r\n")
    protocol, status, *_ = lines[0].split(b" ")
    if protocol != b"HTTP/1.1" or len(status) != 3 or not status.isdigit():
        raise ValueError("invalid HTTP/1.1 status")
    headers = {}
    for line in lines[1:]:
        name, value = line.split(b":", 1)
        name = name.strip().lower()
        if name in headers:
            raise ValueError("duplicate response header")
        headers[name] = value.strip()
    if b"transfer-encoding" in headers:
        raise ValueError("waiting response must have explicit Content-Length")
    if headers.get(b"content-encoding", b"identity") != b"identity":
        raise ValueError("unexpected compressed response")
    length = int(headers[b"content-length"])
    if not 0 <= length <= MAX_RESPONSE:
        raise ValueError("response body length exceeds bound")
    total = end + 4 + length
    if len(data) < total:
        if eof:
            raise ValueError("incomplete response body")
        return None
    if len(data) != total:
        raise ValueError("extra response bytes without a second request")
    return int(status), bytes(data[end + 4:])


def payload(address, path, rid):
    return (f"GET {path}?rid={rid} HTTP/1.1\r\nHost: {address}\r\n"
            "Connection: keep-alive\r\nAccept-Encoding: identity\r\n\r\n").encode("ascii")


class Cohort:
    """One selector, never a thread per connection; one request per socket."""
    def __init__(self, address, target, kind, ids, timeout):
        if not 1 <= target <= MAX_TARGET or kind not in ("timer", "tcp"):
            raise ValueError("invalid waiting cohort")
        self.address, self.target, self.kind = address, target, kind
        self.ids, self.timeout = ids, timeout
        self.selector = selectors.DefaultSelector()
        self.active, self.counts, self.errors, self.timelines = {}, {}, [], []
        self.started = self.sent = self.not_started = self.peak = 0
        self.first_created = self.last_created = None

    def launch(self):
        for _ in range(self.target):
            began, peer = time.monotonic(), None
            self.first_created = began if self.first_created is None else self.first_created
            self.last_created = began
            rid = self.ids.next()
            try:
                peer = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
                peer.setblocking(False)
                row = dict(socket=peer, rid=rid, began=began, deadline=began + self.timeout,
                           connected=False, sent=None, offset=0, buffer=bytearray(),
                           payload=payload(self.address, "/wait" if self.kind == "timer" else "/tcp", rid))
                self.active[peer] = row
                self.started += 1
                self.peak = max(self.peak, len(self.active))
                code = peer.connect_ex(address_tuple(self.address))
                if code not in PENDING_CONNECT:
                    self.finish(row, "connection_error", f"connect errno {code}")
                else:
                    self.selector.register(peer, selectors.EVENT_WRITE, row)
            except OSError as error:
                if peer in self.active:
                    self.finish(self.active[peer], "connection_error", repr(error))
                else:
                    if peer is not None:
                        peer.close()
                    self.not_started += 1
                    self.error("not_started", rid, repr(error))

    def error(self, category, rid, detail):
        if len(self.errors) < 24:
            self.errors.append(dict(category=category, rid=rid, detail=detail[:256]))

    def finish(self, row, category, detail=""):
        peer = row["socket"]
        if self.active.pop(peer, None) is None:
            return
        completed = time.monotonic()
        self.counts[category] = self.counts.get(category, 0) + 1
        self.timelines.append(dict(rid=row["rid"], created=row["began"],
                                   sent=row["sent"], completed=completed,
                                   absolute_deadline=row["deadline"], terminal=category))
        if detail:
            self.error(category, row["rid"], detail)
        try:
            self.selector.unregister(peer)
        except (KeyError, ValueError):
            pass
        peer.close()

    def pump(self, seconds=.05):
        for key, mask in self.selector.select(max(0, min(.05, seconds))):
            row, peer = key.data, key.fileobj
            try:
                if mask & selectors.EVENT_WRITE:
                    if not row["connected"]:
                        code = peer.getsockopt(socket.SOL_SOCKET, socket.SO_ERROR)
                        if code:
                            self.finish(row, "connection_error", f"SO_ERROR {code}")
                            continue
                        row["connected"] = True
                    count = peer.send(row["payload"][row["offset"]:])
                    if count == 0:
                        self.finish(row, "connection_error", "zero-length send")
                        continue
                    if row["sent"] is None:
                        row["sent"] = time.monotonic()
                    row["offset"] += count
                    if row["offset"] == len(row["payload"]):
                        self.sent += 1
                        self.selector.modify(peer, selectors.EVENT_READ, row)
                if mask & selectors.EVENT_READ:
                    chunk = peer.recv(8192)
                    row["buffer"].extend(chunk)
                    if len(row["buffer"]) > MAX_RESPONSE + MAX_HEADER:
                        self.finish(row, "protocol_error", "response buffer exceeded bound")
                        continue
                    response = parse_response(row["buffer"], eof=not chunk)
                    if response is not None:
                        status, body = response
                        expected = ("wait:" if self.kind == "timer" else "tcp:") + row["rid"]
                        category = ("correct" if status == 200 and body == expected.encode() else
                                    "rejected" if status in (429, 503) else
                                    "body_error" if status == 200 else "other_http")
                        if time.monotonic() > row["deadline"]:
                            category = "client_timeout"
                        self.finish(row, category, "" if category == "correct" else f"status={status} body={body[:96]!r}")
            except (BlockingIOError, InterruptedError):
                pass
            except (OSError, ValueError, KeyError) as error:
                self.finish(row, "transport_error" if isinstance(error, OSError) else "protocol_error", repr(error))
        now = time.monotonic()
        for row in list(self.active.values()):
            if now >= row["deadline"]:
                self.finish(row, "client_timeout", "absolute socket-creation deadline")

    def close(self):
        for row in list(self.active.values()):
            self.finish(row, "generator_cleanup", "outstanding operation did not complete")
        self.selector.close()

    def result(self):
        return dict(planned=self.target, started=self.started, not_started=self.not_started,
                    fully_written=self.sent, counts=dict(self.counts), unfinished=len(self.active),
                    peak_sockets=self.peak, ledger_reconciled=self.started + self.not_started == self.target
                    and sum(self.counts.values()) + len(self.active) == self.started,
                    errors=list(self.errors), request_timelines=list(self.timelines),
                    socket_creation_absolute_timeout_seconds=self.timeout, no_retry=True,
                    response_buffer_bytes_per_socket=MAX_RESPONSE + MAX_HEADER,
                    success_requires="entire exact response body including unique rid; no client cancellation")


class NetworkPeer:
    """One selector thread retains one-byte K per connection until release."""
    def __init__(self, limit, port):
        if not 1 <= limit <= MAX_TARGET:
            raise ValueError("invalid peer limit")
        self.limit, self.port = limit, port
        self.lock = threading.Lock()
        self.stopping = threading.Event()
        self.release_pending = False
        self.ready = threading.Event()
        self.stats = dict(accepted=0, held=0, peak_held=0, active=0, received_k=0,
                          replied_k=0, closed_before_release=0, rejected=0)
        self.errors = []
        self.thread = None

    def __enter__(self):
        self.listener = socket.socket()
        self.listener.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        try:
            self.listener.bind(("127.0.0.1", self.port))
            self.port = self.listener.getsockname()[1]
            self.listener.listen(min(16384, self.limit + 64))
            self.listener.setblocking(False)
            self.thread = threading.Thread(target=self._run, name="cross-stack-controlled-peer", daemon=True)
            self.thread.start()
            if not self.ready.wait(2) or not self.thread.is_alive():
                raise RuntimeError("controlled peer did not start")
            return self
        except BaseException:
            self.listener.close()
            raise

    def snapshot(self):
        with self.lock:
            return dict(self.stats, errors=list(self.errors), selector_threads=1,
                        connection_limit=self.limit, input_bytes_per_connection=1)

    def release(self):
        with self.lock:
            self.release_pending = True

    def _run(self):
        peers, selector = {}, selectors.DefaultSelector()

        def close(peer):
            row = peers.pop(peer)
            try:
                selector.unregister(peer)
            except (KeyError, ValueError):
                pass
            peer.close()
            with self.lock:
                self.stats["active"] -= 1
                if row["held"]:
                    self.stats["held"] -= 1
                    self.stats["closed_before_release"] += 1

        try:
            selector.register(self.listener, selectors.EVENT_READ, None)
            self.ready.set()
            while not self.stopping.is_set():
                for key, mask in selector.select(.05):
                    if key.data is None:
                        for _ in range(64):
                            try:
                                peer, _ = self.listener.accept()
                            except (BlockingIOError, InterruptedError):
                                break
                            with self.lock:
                                self.stats["accepted"] += 1
                            if len(peers) >= self.limit:
                                peer.close()
                                with self.lock:
                                    self.stats["rejected"] += 1
                                continue
                            peer.setblocking(False)
                            peers[peer] = dict(held=False, received=False, released=False)
                            selector.register(peer, selectors.EVENT_READ, peers[peer])
                            with self.lock:
                                self.stats["active"] += 1
                    else:
                        peer, row = key.fileobj, key.data
                        try:
                            if mask & selectors.EVENT_READ:
                                data = peer.recv(1)
                                if not data:
                                    close(peer)
                                    continue
                                if row["received"] or data != b"K":
                                    raise ValueError("peer expected exactly one K")
                                row["received"] = row["held"] = True
                                with self.lock:
                                    self.stats["received_k"] += 1
                                    self.stats["held"] += 1
                                    self.stats["peak_held"] = max(self.stats["held"], self.stats["peak_held"])
                            if mask & selectors.EVENT_WRITE and row["released"]:
                                if peer.send(b"K") != 1:
                                    raise ValueError("peer failed to send one K")
                                with self.lock:
                                    self.stats["replied_k"] += 1
                                close(peer)
                        except (BlockingIOError, InterruptedError):
                            pass
                        except (OSError, ValueError) as error:
                            with self.lock:
                                if len(self.errors) < 24:
                                    self.errors.append(repr(error)[:160])
                            close(peer)
                with self.lock:
                    release = self.release_pending
                    self.release_pending = False
                if release:
                    for peer, row in peers.items():
                        if row["held"]:
                            row["held"], row["released"] = False, True
                            selector.modify(peer, selectors.EVENT_READ | selectors.EVENT_WRITE, row)
                            with self.lock:
                                self.stats["held"] -= 1
        except BaseException as error:
            with self.lock:
                self.errors.append(repr(error)[:160])
        finally:
            self.ready.set()
            for peer in list(peers):
                close(peer)
            selector.close()
            self.listener.close()

    def __exit__(self, *unused):
        self.stopping.set()
        self.thread.join(3)
        if self.thread.is_alive():
            raise RuntimeError("controlled peer did not retire within 3 seconds")


def probe(address, path, rid, timeout=3):
    began, peer = time.monotonic(), None
    deadline, data = began + timeout, bytearray()
    try:
        peer = socket.create_connection(address_tuple(address), timeout=timeout)
        peer.settimeout(max(.000001, deadline - time.monotonic()))
        peer.sendall(payload(address, path, rid))
        while True:
            left = deadline - time.monotonic()
            if left <= 0:
                raise TimeoutError("absolute probe deadline")
            peer.settimeout(left)
            chunk = peer.recv(8192)
            data.extend(chunk)
            if len(data) > MAX_RESPONSE + MAX_HEADER:
                raise ValueError("probe response exceeds bound")
            response = parse_response(data, eof=not chunk)
            if response is not None:
                status, body = response
                expected = ("ok:" if path == "/ok" else "timer:") + rid
                correct = status == 200 and body == expected.encode() and time.monotonic() <= deadline
                return dict(rid=rid, path=path, status=status, correct=correct,
                            category="correct" if correct else "rejected" if status in (429, 503) else "wrong_response",
                            started_monotonic=began, completed_monotonic=time.monotonic(),
                            seconds=time.monotonic() - began)
    except (OSError, ValueError, KeyError) as error:
        return dict(rid=rid, path=path, correct=False, category="transport_or_protocol",
                    started_monotonic=began, completed_monotonic=time.monotonic(),
                    error=repr(error)[:160], seconds=time.monotonic() - began)
    finally:
        if peer is not None:
            peer.close()


def normalized_metrics(raw, kind):
    if "waiting_timer" in raw:
        return dict(waiting=int(raw.get("waiting_timer" if kind == "timer" else "waiting_tcp", 0)),
                    timer=int(raw.get("waiting_timer", 0)), tcp=int(raw.get("waiting_tcp", 0)),
                    active=int(raw.get("active", 0)))
    tasks = raw.get("tasks", [])
    timers = sum(int(row.get("timers", 0)) for row in tasks)
    net_wait = int(raw.get("task_blocking", {}).get("net_wait", 0))
    return dict(waiting=timers if kind == "timer" else net_wait, timer=timers, tcp=net_wait,
                active=sum(int(row.get("resident", 0)) for row in tasks),
                task_waiting=sum(int(row.get("waiting", 0)) for row in tasks))


class Observer:
    """Bounded periodic sampler and probes, independent of socket pumping."""
    def __init__(self, service, kind, ids, peer):
        self.service, self.kind, self.ids, self.peer = service, kind, ids, peer
        self.lock = threading.Lock()
        self.stop_event, self.probe_enabled = threading.Event(), threading.Event()
        self.samples, self.probes = [], []
        self.overflow = False
        self.threads = []

    def __enter__(self):
        def sample():
            while not self.stop_event.is_set():
                began = time.monotonic()
                row = dict(started=began)
                try:
                    raw = self.service.metrics()
                    row.update(metrics=normalized_metrics(raw, self.kind), raw_metrics=raw,
                               resources=self.service.domain.snapshot(), available=True)
                except Exception as error:
                    row.update(available=False, error=repr(error)[:256])
                if self.peer is not None:
                    row["peer"] = self.peer.snapshot()
                row["completed"] = time.monotonic()
                with self.lock:
                    if len(self.samples) < MAX_SAMPLES:
                        self.samples.append(row)
                    else:
                        self.overflow = True
                self.stop_event.wait(max(0, .5 - (time.monotonic() - began)))

        def probes():
            while not self.stop_event.is_set():
                if not self.probe_enabled.wait(.1):
                    continue
                if self.stop_event.is_set():
                    break
                for path in ("/ok", "/timer"):
                    row = probe(self.service.address, path, self.ids.next())
                    with self.lock:
                        if len(self.probes) < MAX_SAMPLES * 2:
                            self.probes.append(row)
                        else:
                            self.overflow = True
                self.stop_event.wait(.5)

        for name, target in (("waiting-resources", sample), ("waiting-probes", probes)):
            thread = threading.Thread(name=name, target=target, daemon=True)
            thread.start()
            self.threads.append(thread)
        return self

    def latest(self):
        with self.lock:
            return dict(self.samples[-1]) if self.samples else {}

    def __exit__(self, *unused):
        self.stop_event.set()
        self.probe_enabled.set()
        for thread in self.threads:
            thread.join(7)
            if thread.is_alive():
                raise RuntimeError("waiting sampler/probe did not stop")


def held(row, target, kind, peer):
    return (row.get("available", False) and row["metrics"]["waiting"] >= target
            and (kind != "tcp" or row.get("peer", {}).get("held", 0) == target)
            and (peer is None or not peer.snapshot()["errors"]))


def one_level(harness, stack, kind, target, output, cores=4, memory=1073741824,
              observation_seconds=30, peer_port=None):
    """Fresh complete service for one tier; returns real failures unchanged."""
    if kind not in ("timer", "tcp") or not 1 <= target <= MAX_TARGET:
        raise ValueError("invalid waiting level")
    if observation_seconds <= 0 or observation_seconds > 30:
        raise ValueError("held observation must be positive and <=30 seconds")
    output = Path(output)
    output.mkdir(parents=True, exist_ok=True)
    ids, service, cohort, observer, stop_result = IDs(), None, None, None, None
    begin, held_begin, held_end, recovery, failure = time.monotonic(), None, None, None, None
    baseline, final_resources, final_metrics = None, None, None
    old_affinity, driver_affinity = None, None
    selected_port = peer_port if peer_port is not None else int(harness.manifest.get("peer_port", 19091))
    peer_context = NetworkPeer(target, selected_port) if kind == "tcp" else contextlib.nullcontext(None)
    try:
        with peer_context as peer:
            env = dict(BENCH_MAX_ACTIVE=str(target + 64), BENCH_MAX_PENDING="128",
                       BENCH_MAX_CONNECTIONS=str(target + 128),
                       KU_HTTP_REQUEST_TIMEOUT_MS="45000")
            service = harness.start(stack, cores=cores, memory=memory, output=output,
                                    extra_env=env, peer_port=selected_port)
            warmup = harness.client(service, 'timer', 32, 30, output/'warmup',
                                    concurrency=32, events=False)
            if (not warmup.get('valid_driver') or
                    warmup['ledger']['correct_total'] != warmup['ledger']['planned']):
                raise RuntimeError('uniform waiting prewarm did not return all correct timer responses')
            # Pin only after service creation; its child must inherit its own
            # service-domain CPU set rather than this observer's affinity.
            if hasattr(os, "sched_setaffinity"):
                old_affinity = os.sched_getaffinity(0)
                driver_affinity = set()
                for item in harness.manifest.get("driver_cpuset", "4-6").split(","):
                    ends = item.split("-")
                    driver_affinity.update(range(int(ends[0]), int(ends[-1])+1))
                os.sched_setaffinity(0, driver_affinity)
            baseline = service.domain.snapshot()
            cohort = Cohort(service.address, target, kind, ids, 50 if kind == "timer" else 40)
            with Observer(service, kind, ids, peer) as observer:
                cohort.launch()
                # A 35-second timer cannot prove a 30-second simultaneous hold
                # if admitting this cohort takes more than the 5-second margin.
                admission_end = cohort.first_created + (4 if kind == "timer" else 5)
                while time.monotonic() < admission_end:
                    cohort.pump()
                    row = observer.latest()
                    if cohort.sent == target and len(cohort.active) == target and held(row, target, kind, peer):
                        held_begin = max(time.monotonic(), row["completed"])
                        break
                    if cohort.counts or cohort.not_started:
                        break
                if held_begin is None:
                    failure = "target_not_simultaneously_waiting_before_admission_deadline"
                else:
                    observer.probe_enabled.set()
                    held_deadline = held_begin + observation_seconds
                    while time.monotonic() < held_deadline:
                        cohort.pump(min(.05, held_deadline - time.monotonic()))
                        row = observer.latest()
                        if (cohort.counts or len(cohort.active) != target
                                or not held(row, target, kind, peer)
                                or time.monotonic() - row.get("completed", 0) > 2):
                            failure = "simultaneous_wait_was_lost_or_observation_unavailable"
                            break
                    held_end = time.monotonic()
                observer.probe_enabled.clear()
                release_began = time.monotonic()
                if peer is not None:
                    peer.release()
                # Each socket keeps its own original creation deadline. No
                # renewal on progress and no closing a wait as a success.
                while cohort.active and time.monotonic() < cohort.first_created + (51 if kind == "timer" else 41):
                    cohort.pump()
                if cohort.active:
                    failure = failure or "waiting_client_did_not_complete_by_absolute_deadline"
                recovery = dict(seconds_after_release=time.monotonic() - release_began,
                                applicable=kind == "tcp")
            # No probes are now active; observe actual per-service wait records
            # retire, rather than subtracting a presumed probe count.
            retire_deadline = release_began + 5 if kind == "tcp" else time.monotonic() + 5
            while time.monotonic() < retire_deadline:
                raw = service.metrics()
                final_metrics = normalized_metrics(raw, kind)
                if final_metrics["timer"] == 0 and final_metrics["tcp"] == 0 and final_metrics["active"] == 0:
                    recovery["resources_zero_seconds_after_release"] = time.monotonic() - release_began
                    break
                time.sleep(.05)
            final_resources = service.domain.snapshot()
            peer_final = peer.snapshot() if peer else None
    except Exception as error:
        failure = failure or "execution_error: " + repr(error)[:320]
        peer_final = None
    finally:
        if cohort is not None:
            cohort.close()
        if service is not None:
            try:
                stop_result = service.stop()
            except Exception as error:
                stop_result = dict(cleanup_error=repr(error)[:320])
                failure = failure or "service_cleanup_failed"
        if old_affinity is not None:
            os.sched_setaffinity(0, old_affinity)
    ledger = cohort.result() if cohort else None
    samples = observer.samples if observer else []
    probes = observer.probes if observer else []
    held_samples = [row for row in samples if held_begin is not None
                    and row["started"] >= held_begin and row["completed"] <= (held_end or held_begin)]
    probe_summary = {}
    for path in ("/ok", "/timer"):
        rows = [row for row in probes if row["path"] == path and held_begin is not None
                and held_begin <= row["started_monotonic"] < (held_end or held_begin)]
        correct = [row["seconds"] * 1000 for row in rows if row["correct"]]
        correct.sort()
        probe_summary[path] = dict(planned=len(rows), correct=len(correct),
                                   errors=[row for row in rows if not row["correct"]][:24],
                                   all_results={name: sum(row["category"] == name for row in rows)
                                                for name in ("correct", "rejected", "wrong_response", "transport_or_protocol")},
                                   observed_max_ms=max(correct) if correct else None,
                                   p99_ms=correct[max(0, math.ceil(len(correct) * .99)-1)] if len(correct) >= 100 else None,
                                   quantile_note="insufficient p99 tail sample below 100; observed max used as conservative gate")
    full_hold = held_begin is not None and held_end is not None and held_end - held_begin >= observation_seconds
    all_responses = ledger is not None and ledger["counts"].get("correct", 0) == target and ledger["ledger_reconciled"]
    timer_duration_ok = (kind != "timer" or all_responses and
                         all(row["completed"]-row["created"] >= 34.99
                             for row in ledger["request_timelines"] if row["terminal"] == "correct"))
    all_zero = final_metrics is not None and not any(final_metrics[name] for name in ("timer", "tcp", "active"))
    probes_ok = all(row["planned"] >= max(1, int(observation_seconds // 2)) and row["correct"] == row["planned"]
                    and row["observed_max_ms"] <= 100 for row in probe_summary.values())
    samples_ok = (len(held_samples) >= max(1, int(observation_seconds))
                  and all(held(row, target, kind, None) for row in held_samples))
    tcp_ok = kind != "tcp" or (peer_final is not None and peer_final["received_k"] == target
                               and peer_final["replied_k"] == target and peer_final["active"] == 0
                               and peer_final["closed_before_release"] == 0 and not peer_final["errors"])
    tcp_recovery_ok = (kind != "tcp" or recovery is not None
                       and recovery["seconds_after_release"] <= 5
                       and recovery.get("resources_zero_seconds_after_release", float("inf")) <= 5)
    cleanup_ok = (stop_result is not None and stop_result.get("cgroup_removed") is True
                  and stop_result.get("after", {}).get("process_count") == 0
                  and stop_result.get("exit_code") == 0 and not stop_result.get("forced"))
    resource_rows = [row["resources"] for row in held_samples if row.get("resources")]
    resources_ok = (bool(resource_rows) and baseline is not None and final_resources is not None
                    and all(isinstance(row.get("memory_current"), int)
                            and isinstance(row.get("threads"), int) and row.get("process_count", 0) > 0
                            and row.get("memory_events", {}).get("oom_kill", 0) == baseline.get("memory_events", {}).get("oom_kill", 0)
                            and row.get("memory_events", {}).get("oom", 0) == baseline.get("memory_events", {}).get("oom", 0)
                            for row in resource_rows))
    passed = all((not failure, full_hold, all_responses, timer_duration_ok, all_zero, probes_ok,
                  samples_ok, tcp_ok, tcp_recovery_ok, cleanup_ok, resources_ok, observer is not None and not observer.overflow))
    peak_memory = max((row["memory_current"] for row in resource_rows
                       if isinstance(row.get("memory_current"), int)), default=None)
    baseline_memory = baseline.get("memory_current") if baseline else None
    cost = dict(actual_simultaneous_waiting=target if full_hold and samples_ok else None,
                whole_service_memory_peak_observed=peak_memory,
                baseline_memory_current=baseline_memory,
                additional_memory_per_waiting=(peak_memory-baseline_memory)/target
                if full_hold and samples_ok and peak_memory is not None and baseline_memory is not None else None,
                note="whole service cgroup; subtracting baseline is observational, not exact per-Task heap accounting; never compare a rejected target with a fully held target")
    cost.update(threads_before=baseline.get("threads") if baseline else None,
                threads_peak_observed=max((row.get("threads", 0) for row in resource_rows), default=None),
                processes_peak_observed=max((row.get("process_count", 0) for row in resource_rows), default=None),
                fd_peak_observed=max((row.get("fd", 0) for row in resource_rows), default=None),
                thread_scaling_note="compare observed whole-service thread trend across successful actual-held tiers; one tier alone is not an extrapolation")
    if held_samples:
        first, last = held_samples[0]["resources"], held_samples[-1]["resources"]
        wall = (last.get("monotonic_ns", 0)-first.get("monotonic_ns", 0))/1e9
        cpu = (last.get("cpu_stat", {}).get("usage_usec", 0)-first.get("cpu_stat", {}).get("usage_usec", 0))/1e6
        cost.update(held_observed_wall_seconds=wall, service_cpu_seconds=cpu,
                    service_one_core_cpu_percent=100*cpu/wall if wall > 0 else None)
    result = dict(schema="ku-cross-stack-waiting-v1", stack=stack, kind=kind, target=target,
                  requested_observation_seconds=observation_seconds, formal_30s=observation_seconds == 30,
                  passed=passed, failure=failure, held_begin_monotonic=held_begin, held_end_monotonic=held_end,
                  actual_held_seconds=held_end-held_begin if held_begin is not None and held_end is not None else 0,
                  resources_before=baseline, resources_after=final_resources, samples=samples,
                  final_metrics=final_metrics, cohort=ledger, probes=probe_summary, probe_records=probes,
                  peer=peer_final, recovery=recovery, resource_cost=cost, stop=stop_result,
                  driver=dict(pid=os.getpid(), cpuset=sorted(driver_affinity) if driver_affinity else None,
                              cohort_selector_threads=0, sampler_threads=1, probe_threads=1,
                              tcp_peer_threads=1 if kind == "tcp" else 0),
                  seconds=time.monotonic()-begin,
                  admission=dict(requested_active=target+64, ordinary_reserve=64,
                                 requested_connections=target+128,
                                 Ku_actual_active_limit=1024 if stack == "ku" else None,
                                 limits_note="other runtimes use public BENCH_* parameters; Ku retains actual fixed 1024 active/Task bound"),
                  checks=dict(full_simultaneous_hold=full_hold, complete_correct_responses=all_responses,
                              sampled_wait_evidence=samples_ok, ordinary_probes=probes_ok,
                              wait_records_retired=all_zero, controlled_peer=tcp_ok,
                              tcp_release_and_zero_within_5_seconds=tcp_recovery_ok,
                              real_35_second_timer=timer_duration_ok, resources_valid=resources_ok,
                              normal_exit_and_cgroup_removed=cleanup_ok,
                              bounded_sampler=observer is not None and not observer.overflow),
                  scope="each target is actual executed waiting, not only an open socket; driver/peer/database outside service cgroup; all errors retained")
    path = output / "waiting.json"
    with path.open("x", encoding="utf-8") as stream:
        json.dump(result, stream, ensure_ascii=False, separators=(",", ":"))
        stream.write("\n")
    return result


def run_waiting(harness, output, stacks=("ku", "go", "rust", "node"), levels=LEVELS,
                kinds=("timer", "tcp"), cores=4, memory=1073741824, observation_seconds=30):
    """Stop higher tiers on a concrete boundary; report unexecuted tiers plainly."""
    results = []
    for stack in stacks:
        for kind in kinds:
            boundary = None
            for target in levels:
                if boundary:
                    results.append(dict(stack=stack, kind=kind, target=target, executed=False,
                                        reason="higher_than_observed_boundary", boundary=boundary))
                    continue
                if stack == "ku" and target + 64 > 1024:
                    boundary = dict(target=target, reason="documented_Ku_active_Task_limit_1024_with_64_probe_reserve")
                    results.append(dict(stack=stack, kind=kind, target=target, executed=False,
                                        reason=boundary["reason"], qualified=False))
                    continue
                row = one_level(harness, stack, kind, target, Path(output)/stack/kind/str(target),
                                cores, memory, observation_seconds)
                row["executed"] = True
                results.append(row)
                if not row["passed"]:
                    boundary = dict(target=target, reason=row.get("failure") or "waiting_SLO_or_resource_gate_failed")
    return dict(schema="ku-cross-stack-waiting-matrix-v1", levels=list(levels), results=results,
                common_tiers=[32,64,128], capacity_tiers=[512,2048,8192],
                no_thread_per_connection=True, no_retry=True,
                note="highest passed tier only; a rejected/unheld tier has no per-waiting resource superiority claim")


def run(harness, output=None):
    """Common runner phase entry point; writes one matrix and tier evidence."""
    output = Path(output) if output is not None else harness.output / "waiting"
    output.mkdir(parents=True, exist_ok=True)
    result = run_waiting(harness, output)
    with (output / "waiting-matrix.json").open("x", encoding="utf-8") as stream:
        json.dump(result, stream, ensure_ascii=False, separators=(",", ":"))
        stream.write("\n")
    return result


def self_test():
    """Necessary local contracts; short mocks do not claim 30-second acceptance."""
    import unittest

    class WaitingContracts(unittest.TestCase):
        def test_parser_requires_full_body_and_unique_framing(self):
            head=b"HTTP/1.1 200 OK\r\nContent-Length: 6\r\n\r\n"
            self.assertIsNone(parse_response(head+b"wait:"))
            self.assertEqual(parse_response(head+b"wait:1"), (200,b"wait:1"))
            for data in (head+b"wait:1extra", head.replace(b"Content-Length: 6",b"Content-Length: 6\r\nContent-Length: 6"), b"HTTP/1.1 200 OK\r\n\r\n"):
                with self.assertRaises((ValueError,KeyError)):
                    parse_response(data)

        def test_ids_and_native_normalization(self):
            ids=IDs();values=[ids.next() for _ in range(1000)]
            self.assertEqual(len(set(values)),1000)
            self.assertTrue(all(len(value)==16 and value.isdecimal() for value in values))
            raw=dict(tasks=[dict(timers=16,waiting=16,resident=16),dict(timers=16,waiting=16,resident=16)],task_blocking=dict(net_wait=32))
            self.assertEqual(normalized_metrics(raw,"timer")["waiting"],32)
            self.assertEqual(normalized_metrics(raw,"tcp")["waiting"],32)

        def test_peer_holds_then_releases_only_K(self):
            with NetworkPeer(2,0) as peer:
                with socket.create_connection(("127.0.0.1",peer.port),timeout=1) as connection:
                    connection.sendall(b"K")
                    deadline=time.monotonic()+1
                    while peer.snapshot()["held"]!=1 and time.monotonic()<deadline:
                        time.sleep(.01)
                    self.assertEqual(peer.snapshot()["held"],1)
                    connection.settimeout(.02)
                    with self.assertRaises((socket.timeout, TimeoutError)):
                        connection.recv(1)
                    peer.release();connection.settimeout(1)
                    self.assertEqual(connection.recv(1),b"K")
                self.assertEqual(peer.snapshot()["replied_k"],1)

        def test_cohort_absolute_deadline_does_not_renew(self):
            listener=socket.socket();listener.bind(("127.0.0.1",0));listener.listen(1)
            address="127.0.0.1:"+str(listener.getsockname()[1]);done=threading.Event()
            def slow():
                conn,_=listener.accept()
                try:
                    conn.settimeout(1);conn.recv(4096)
                    for byte in b"HTTP/1.1 200 OK\r\nContent-Length: 21\r\n\r\nwait:0000000000000001":
                        conn.send(bytes([byte]));time.sleep(.01)
                except OSError:
                    pass
                finally:
                    conn.close();done.set()
            thread=threading.Thread(target=slow);thread.start()
            cohort=Cohort(address,1,"timer",IDs(),.06)
            try:
                began=time.monotonic();cohort.launch()
                while cohort.active and time.monotonic()-began<1:
                    cohort.pump(.01)
                self.assertEqual(cohort.counts,{"client_timeout":1})
                self.assertLess(time.monotonic()-began,.3)
            finally:
                cohort.close();listener.close();thread.join(2)
                self.assertTrue(done.is_set())

    suite=unittest.defaultTestLoader.loadTestsFromTestCase(WaitingContracts)
    return unittest.TextTestRunner(verbosity=2).run(suite).wasSuccessful()


def main():
    parser=argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--manifest-root",type=Path)
    parser.add_argument("--output",type=Path)
    parser.add_argument("--stacks",nargs="+",default=["ku","go","rust","node"])
    parser.add_argument("--kinds",nargs="+",default=["timer","tcp"])
    parser.add_argument("--levels",nargs="+",type=int,default=list(LEVELS))
    parser.add_argument("--self-test",action="store_true")
    args=parser.parse_args()
    if args.self_test:
        return 0 if self_test() else 1
    if args.manifest_root is None or args.output is None:
        parser.error("--manifest-root and --output are required")
    from linux_harness import Harness
    args.output.mkdir(parents=True,exist_ok=True)
    result=run_waiting(Harness(args.manifest_root,args.output),args.output,args.stacks,args.levels,args.kinds)
    with (args.output/"waiting-matrix.json").open("x",encoding="utf-8") as stream:
        json.dump(result,stream,ensure_ascii=False,separators=(",",":"))
        stream.write("\n")
    print(json.dumps(dict(results=len(result["results"]),output=str(args.output/"waiting-matrix.json"))))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
