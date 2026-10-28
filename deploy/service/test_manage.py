#!/usr/bin/env python3
"""Focused permanent contracts for deployment budgets and rolling failure."""

import importlib.util
import json
from pathlib import Path
import socket
import struct
import sys
import tempfile
import threading
import time
import unittest
from unittest.mock import Mock, patch

SPEC = importlib.util.spec_from_file_location("manage", Path(__file__).with_name("manage.py"))
M = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(M)

VERIFY_SPEC = importlib.util.spec_from_file_location("verify", Path(__file__).with_name("verify.py"))
V = importlib.util.module_from_spec(VERIFY_SPEC)
VERIFY_SPEC.loader.exec_module(V)


class ResourceIdentityDiagnostics(unittest.TestCase):
    def windows_resource_fixture(self, extra_created=134373228594974156):
        # The Caddy CI failure: an old compiler process retained PPID 8904,
        # which Windows had since assigned to this fixture's new native child.
        records = {
            2504: {"ppid": 3304, "image_name": "ku.exe"},
            3044: {"ppid": 2504, "image_name": "ku.exe"},
            8904: {"ppid": 3044, "image_name": "previous.exe"},
            2528: {"ppid": 8904, "image_name": "vctip.exe"},
            3304: {"ppid": 8980, "image_name": "python.exe"},
            7116: {"ppid": 3304, "image_name": "caddy.exe"},
        }
        created = {2504: 134373241869561700, 3044: 134373241912654862,
                   8904: 134373241912754210, 2528: extra_created,
                   3304: 134373241809413413, 7116: 134373241829426148}
        identities = {pid: {"identity_status": "observed", "creation_time_100ns": stamp,
                            "image_path": "C:\\fixture\\" + records[pid]["image_name"]}
                      for pid, stamp in created.items()}
        threads = {2504: 6, 3044: 6, 8904: 12, 2528: 15, 3304: 9, 7116: 6}
        samples = Mock()
        samples.windows_process_sample.side_effect = lambda pid: {
            "threads": threads[pid], "rss_bytes": 1}
        supervisor = Mock(pid=2504)
        supervisor.poll.return_value = None
        return records, identities, samples, supervisor

    def resource_fixture_snapshot(self, records, identities, samples, supervisor, birth_order=None):
        birth_order = birth_order or (lambda _pid, created: {
            "identity_status": "observed", "creation_time_100ns": created,
            "create_interrupt_time_100ns": created, "boot_id": 7})
        with patch.object(V.os, "name", "nt"), \
                patch.object(V.os, "getpid", return_value=3304), \
                patch.object(V.M, "OWNED_SUPERVISORS", {"a": supervisor}), \
                patch.object(V, "windows_process_records", return_value=records), \
                patch.object(V, "windows_process_identity", side_effect=lambda pid, **_kwargs: identities[pid].copy()), \
                patch.object(V, "windows_process_birth_order", side_effect=birth_order), \
                patch.object(V.importlib.util, "spec_from_file_location", return_value=Mock()), \
                patch.object(V.importlib.util, "module_from_spec", return_value=samples):
            return V.resource_snapshot(Mock(pid=7116), include_roles=True)

    def test_windows_snapshot_rejects_stale_ppid_before_resource_attribution(self):
        snapshot = self.resource_fixture_snapshot(*self.windows_resource_fixture())
        slot = [row for row in snapshot["processes"] if row.get("slot") == "a"]
        self.assertEqual(sum(row["threads"] for row in slot), 24)
        self.assertEqual({row["pid"] for row in slot}, {2504, 3044, 8904})
        excluded = snapshot.get("excluded_processes", [])
        edge = next(row for row in excluded if row.get("pid") == 2528)
        self.assertEqual(edge["ppid"], 8904)
        self.assertEqual(edge["reason"], "parent_pid_reused")
        self.assertEqual(edge["creation_time_100ns"], 134373228594974156)
        self.assertEqual(edge["parent_creation_time_100ns"], 134373241912754210)

    def test_windows_snapshot_counts_true_descendant_and_preserves_thread_gate(self):
        config = {"slots": [{"name": "a"}], "deployment_budget": {
            "management_threads": 34, "management_memory_bytes": 100,
            "total_threads": 133, "reserved_memory_bytes": 100,
            "per_instance": {"total_threads": 33, "reserved_memory_bytes": 100}}}
        for clock_reversed in (False, True):
            with self.subTest(clock_reversed=clock_reversed):
                birth_order = (lambda pid, created: {"identity_status": "observed",
                    "creation_time_100ns": created, "boot_id": 7,
                    "create_interrupt_time_100ns": 201 if pid == 2528 else 200}) if clock_reversed else \
                    Mock(side_effect=AssertionError("normal resource ancestry must not query cold telemetry"))
                snapshot = self.resource_fixture_snapshot(*self.windows_resource_fixture(
                    extra_created=134373228594974156 if clock_reversed else 134373241912754211),
                    birth_order=birth_order)
                slot = [row for row in snapshot["processes"] if row.get("slot") == "a"]
                self.assertEqual(sum(row["threads"] for row in slot), 39)
                self.assertEqual({row["pid"] for row in slot}, {2504, 3044, 8904, 2528})
                with patch.object(V.os, "getpid", return_value=3304):
                    with self.assertRaisesRegex(RuntimeError, "slot a threads 39 > 33"):
                        V.validate_caddy_resources(snapshot, config, 7116)

    def test_windows_snapshot_reversed_clock_without_birth_order_is_unverified(self):
        with self.assertRaisesRegex(RuntimeError, "birth order unknown"):
            self.resource_fixture_snapshot(*self.windows_resource_fixture(),
                birth_order=Mock(side_effect=RuntimeError("birth order unknown")))

    def test_windows_stale_edge_evidence_has_a_fixed_bound(self):
        records = {1: {"ppid": 0}, **{pid: {"ppid": 1} for pid in range(100, 140)}}
        identity = lambda pid: {"identity_status": "observed",
                               "creation_time_100ns": 100 if pid == 1 else 50}
        with patch.object(V, "windows_process_birth_order", side_effect=lambda _pid, created: {
                "identity_status": "observed", "creation_time_100ns": created,
                "create_interrupt_time_100ns": created, "boot_id": 7}):
            selected = V.windows_owned_processes(records, {1}, identity_reader=identity)
        self.assertEqual(set(selected["processes"]), {1})
        self.assertEqual(selected["excluded_process_count"], 40)
        self.assertEqual(len(selected["excluded_processes"]), 32)
        self.assertTrue(selected["excluded_processes_truncated"])

    def test_windows_snapshot_unknown_candidate_is_not_silently_excluded(self):
        records, identities, samples, supervisor = self.windows_resource_fixture()
        identities[2528] = {"identity_status": "unknown", "identity_error": "access denied"}
        with self.assertRaisesRegex(RuntimeError, "identity unknown"):
            self.resource_fixture_snapshot(records, identities, samples, supervisor)

    def test_windows_snapshot_rejects_identity_change_during_sampling(self):
        records, identities, samples, supervisor = self.windows_resource_fixture()

        def sample(pid):
            if pid == 8904:
                identities[pid]["creation_time_100ns"] += 1
            return {"threads": 1, "rss_bytes": 1}

        samples.windows_process_sample.side_effect = sample
        with self.assertRaisesRegex(RuntimeError, "identity.*changed|changed.*identity"):
            self.resource_fixture_snapshot(records, identities, samples, supervisor)

    def test_windows_snapshot_rejects_parent_change_between_snapshots(self):
        records, identities, samples, supervisor = self.windows_resource_fixture()
        changed = {pid: row.copy() for pid, row in records.items()}
        changed[8904]["ppid"] = 7116
        with patch.object(V.os, "name", "nt"), \
                patch.object(V.os, "getpid", return_value=3304), \
                patch.object(V.M, "OWNED_SUPERVISORS", {"a": supervisor}), \
                patch.object(V, "windows_process_records", side_effect=[records, changed, changed]), \
                patch.object(V, "windows_process_identity", side_effect=lambda pid, **_kwargs: identities[pid].copy()), \
                patch.object(V, "windows_process_birth_order", side_effect=lambda _pid, created: {
                    "identity_status": "observed", "creation_time_100ns": created,
                    "create_interrupt_time_100ns": created, "boot_id": 7}), \
                patch.object(V.importlib.util, "spec_from_file_location", return_value=Mock()), \
                patch.object(V.importlib.util, "module_from_spec", return_value=samples):
            with self.assertRaisesRegex(RuntimeError, "changed"):
                V.resource_snapshot(Mock(pid=7116), include_roles=True)

    def test_windows_native_child_rejects_stale_direct_ppid(self):
        records, identities, _samples, _supervisor = self.windows_resource_fixture()
        records[2528]["ppid"] = 3044
        with patch.object(V.os, "name", "nt"), \
                patch.object(V, "windows_process_records", return_value=records), \
                patch.object(V, "windows_process_identity", side_effect=lambda pid: identities[pid].copy()), \
                patch.object(V, "windows_process_birth_order", side_effect=lambda _pid, created: {
                    "identity_status": "observed", "creation_time_100ns": created,
                    "create_interrupt_time_100ns": created, "boot_id": 7}):
            self.assertEqual(V.native_child(3044), 8904)
            pid, identity = V.native_child(3044, return_identity=True)
            with self.assertRaisesRegex(RuntimeError, "launcher 3044 identity changed"):
                V.native_child(3044, expected_parent_creation_time_100ns=1)
        self.assertEqual(pid, 8904)
        self.assertEqual(identity["creation_time_100ns"], identities[8904]["creation_time_100ns"])

    def test_windows_native_child_unknown_candidate_cannot_be_ignored(self):
        records, identities, _samples, _supervisor = self.windows_resource_fixture()
        records[2528]["ppid"] = 3044
        identities[2528] = {"identity_status": "unknown"}
        with patch.object(V.os, "name", "nt"), \
                patch.object(V, "windows_process_records", return_value=records), \
                patch.object(V, "windows_process_identity", side_effect=lambda pid: identities[pid].copy()), \
                patch.object(V, "windows_process_birth_order", side_effect=lambda _pid, created: {
                    "identity_status": "observed", "creation_time_100ns": created,
                    "create_interrupt_time_100ns": created, "boot_id": 7}):
            with self.assertRaisesRegex(RuntimeError, "identity unknown"):
                V.native_child(3044)

    def test_windows_native_child_checks_birth_order_even_when_utc_looks_valid(self):
        records, identities, _samples, _supervisor = self.windows_resource_fixture(
            extra_created=134373241912754211)
        records[2528]["ppid"] = 3044
        for unknown_order in (False, True):
            with self.subTest(unknown_order=unknown_order):
                def birth_order(pid, created):
                    if unknown_order:
                        raise RuntimeError("birth order unknown")
                    return {"identity_status": "observed", "creation_time_100ns": created,
                            "create_interrupt_time_100ns": 1 if pid == 2528 else created, "boot_id": 7}

                with patch.object(V.os, "name", "nt"), \
                        patch.object(V, "windows_process_records", return_value=records), \
                        patch.object(V, "windows_process_identity", side_effect=lambda pid: identities[pid].copy()), \
                        patch.object(V, "windows_process_birth_order", side_effect=birth_order):
                    if unknown_order:
                        with self.assertRaisesRegex(RuntimeError, "birth order unknown"):
                            V.native_child(3044)
                    else:
                        self.assertEqual(V.native_child(3044), 8904)

    def test_windows_birth_order_validates_fixed_native_header_and_unknown_api(self):
        import ctypes

        cases = ("valid", "api_missing", "status", "short_return", "oversize_return",
                 "short_header", "oversize_header", "pid_mismatch", "creation_mismatch",
                 "missing_interrupt", "held_identity_mismatch")
        for case in cases:
            with self.subTest(case=case):
                kernel, query = Mock(), Mock()
                kernel.OpenProcess.return_value = 501
                kernel.CloseHandle.return_value = True

                def times(_process, created, _exited, _system, _user):
                    created._obj.dwLowDateTime = 101 if case == "held_identity_mismatch" else 100
                    return True

                def telemetry(process, information_class, buffer, capacity, returned):
                    self.assertEqual((process, information_class, capacity), (501, 64, 65536))
                    # The wire header has fixed 32/64-bit fields and trailing
                    # padding, independent of the host C unsigned-long size.
                    header = bytearray(96)
                    struct.pack_into("<IIQQQQQQII", header, 0,
                        95 if case == "short_header" else 97 if case == "oversize_header" else 96,
                        8905 if case == "pid_mismatch" else 8904, 19,
                        101 if case == "creation_mismatch" else 100,
                        0 if case == "missing_interrupt" else 200, 180, 1, 50, 3, 7)
                    ctypes.memmove(buffer, bytes(header), len(header))
                    returned._obj.value = 95 if case == "short_return" else \
                        65537 if case == "oversize_return" else 96
                    return 0xc0000003 if case == "status" else 0

                kernel.GetProcessTimes.side_effect = times
                query.side_effect = telemetry
                ntdll = object() if case == "api_missing" else Mock(NtQueryInformationProcess=query)
                with patch("ctypes.WinDLL", create=True,
                           side_effect=lambda name, **_kwargs: kernel if name == "kernel32" else ntdll), \
                        patch("ctypes.get_last_error", create=True, return_value=5):
                    if case == "valid":
                        order = V.windows_process_birth_order(8904, 100)
                        self.assertEqual(order, {"identity_status": "observed", "creation_time_100ns": 100,
                            "create_interrupt_time_100ns": 200, "boot_id": 7,
                            "telemetry_header_size": 96, "telemetry_return_length": 96})
                    else:
                        with self.assertRaisesRegex(RuntimeError, "birth order unknown"):
                            V.windows_process_birth_order(8904, 100)
                if case == "api_missing":
                    kernel.OpenProcess.assert_not_called()
                    kernel.CloseHandle.assert_not_called()
                else:
                    kernel.CloseHandle.assert_called_once_with(501)

    @unittest.skipUnless(sys.platform == "win32", "requires the real Windows process API")
    def test_windows_current_process_birth_order_native_api(self):
        pid = V.os.getpid()
        identity = V.windows_process_identity(pid)
        created = V.windows_creation_time(pid, identity)
        order = V.windows_process_birth_order(pid, created)
        self.assertEqual(order["identity_status"], "observed")
        self.assertEqual(order["creation_time_100ns"], created)
        self.assertGreater(order["create_interrupt_time_100ns"], 0)
        self.assertIs(type(order["boot_id"]), int)
        self.assertGreaterEqual(order["telemetry_header_size"], 96)
        self.assertGreaterEqual(order["telemetry_return_length"], order["telemetry_header_size"])
        self.assertLessEqual(order["telemetry_return_length"], 65536)

    def test_windows_cleanup_distinguishes_same_instance_and_reused_pid(self):
        expected = {(8904, 100): {"pid": 8904, "creation_time_100ns": 100}}
        records = {8904: {"ppid": 3044, "image_name": "previous.exe"}}
        for current_start in (100, 101):
            with self.subTest(current_start=current_start):
                receipt = V.windows_cleanup_receipt(expected, records=records,
                    identity_reader=lambda _pid: {"identity_status": "observed",
                                                  "creation_time_100ns": current_start})
                self.assertEqual([row["pid"] for row in receipt["remaining"]],
                                 [8904] if current_start == 100 else [])
                self.assertEqual([row["pid"] for row in receipt["reused"]],
                                 [8904] if current_start == 101 else [])
                self.assertEqual(receipt["unverified"], [])

    def test_windows_cleanup_unknown_identity_is_not_a_retirement_receipt(self):
        expected = {(8904, 100): {"pid": 8904, "creation_time_100ns": 100}}
        receipt = V.windows_cleanup_receipt(expected,
            records={8904: {"ppid": 3044, "image_name": "previous.exe"}},
            identity_reader=lambda _pid: {"identity_status": "unknown", "identity_error": "access denied"})
        self.assertEqual([row["pid"] for row in receipt["unverified"]], [8904])
        self.assertEqual(receipt["reused"], [])

    def test_windows_cleanup_absent_pid_has_no_remaining_instance(self):
        receipt = V.windows_cleanup_receipt(
            {(8904, 100): {"pid": 8904, "creation_time_100ns": 100}}, records={},
            identity_reader=Mock(side_effect=AssertionError("absent PID must not be reopened")))
        self.assertEqual(receipt["remaining"], [])
        self.assertEqual(receipt["reused"], [])
        self.assertEqual(receipt["unverified"], [])

    def test_windows_fault_injection_checks_creation_on_the_termination_handle(self):
        for current_start in (100, 101, None):
            with self.subTest(current_start=current_start):
                kernel = Mock()
                kernel.OpenProcess.return_value = 501
                kernel.CloseHandle.return_value = True
                kernel.TerminateProcess.return_value = True

                def times(_process, created, _exited, _system, _user):
                    if current_start is None:
                        return False
                    created._obj.dwHighDateTime = current_start >> 32
                    created._obj.dwLowDateTime = current_start & 0xffffffff
                    return True

                kernel.GetProcessTimes.side_effect = times
                with patch("ctypes.WinDLL", create=True, return_value=kernel), \
                        patch("ctypes.get_last_error", create=True, return_value=5):
                    if current_start == 100:
                        V.windows_terminate_process(8904, 100)
                    else:
                        with self.assertRaises((RuntimeError, OSError)):
                            V.windows_terminate_process(8904, 100)
                kernel.OpenProcess.assert_called_once_with(0x1001, False, 8904)
                if current_start == 100:
                    self.assertEqual(kernel.TerminateProcess.call_count, 1)
                    self.assertEqual(kernel.TerminateProcess.call_args.args[0], 501)
                else:
                    kernel.TerminateProcess.assert_not_called()
                kernel.CloseHandle.assert_called_once_with(501)

    def test_selected_windows_identity_retains_creation_time_and_closes_handle(self):
        kernel = Mock()
        kernel.OpenProcess.return_value = 501
        kernel.CloseHandle.return_value = True

        def times(_process, created, _exited, _system, _user):
            created._obj.dwHighDateTime = 7
            created._obj.dwLowDateTime = 11
            return True

        def image(_process, _flags, value, size):
            value.value = r"C:\fixture\new.exe"
            size._obj.value = len(value.value)
            return True

        kernel.GetProcessTimes.side_effect = times
        kernel.QueryFullProcessImageNameW.side_effect = image
        with patch("ctypes.WinDLL", create=True, return_value=kernel):
            identity = V.windows_process_identity(6592)
        self.assertEqual(identity["identity_status"], "observed")
        self.assertEqual(identity["creation_time_100ns"], (7 << 32) | 11)
        self.assertEqual(identity["image_path"], r"C:\fixture\new.exe")
        kernel.OpenProcess.assert_called_once_with(0x1000, False, 6592)
        kernel.CloseHandle.assert_called_once_with(501)

    def test_failed_identity_query_is_unknown_and_still_closes_opened_handle(self):
        kernel = Mock()
        kernel.OpenProcess.return_value = 501
        kernel.GetProcessTimes.return_value = False
        kernel.CloseHandle.return_value = True
        with patch("ctypes.WinDLL", create=True, return_value=kernel), \
                patch("ctypes.get_last_error", create=True, return_value=5):
            identity = V.windows_process_identity(1956)
        self.assertEqual(identity["identity_status"], "unknown")
        self.assertIn("GetProcessTimes", identity["identity_error"])
        self.assertNotIn("image_path", identity)
        kernel.CloseHandle.assert_called_once_with(501)

    def test_borrowed_supervisor_identity_never_closes_the_owned_handle(self):
        for readable in (True, False):
            with self.subTest(readable=readable):
                kernel = Mock()

                def times(_process, created, _exited, _system, _user):
                    created._obj.dwLowDateTime = 100
                    return readable

                def image(_process, _flags, value, size):
                    value.value = r"C:\fixture\ku.exe"
                    size._obj.value = len(value.value)
                    return True

                kernel.GetProcessTimes.side_effect = times
                kernel.QueryFullProcessImageNameW.side_effect = image
                with patch("ctypes.WinDLL", create=True, return_value=kernel), \
                        patch("ctypes.get_last_error", create=True, return_value=5):
                    identity = V.windows_process_identity(2504, process_handle=501)
                self.assertEqual(identity["identity_status"], "observed" if readable else "unknown")
                kernel.OpenProcess.assert_not_called()
                kernel.CloseHandle.assert_not_called()

    def test_identity_diagnostics_preserve_both_same_parent_candidates_and_gate(self):
        snapshot = {"processes": [
            {"pid": 9296, "ppid": 3816, "threads": 6, "rss_bytes": 1},
            {"pid": 9068, "ppid": 9296, "threads": 6, "rss_bytes": 1},
            {"pid": 6592, "ppid": 9068, "threads": 12, "rss_bytes": 1,
             "image_name": "new.exe", "identity_status": "observed", "creation_time_100ns": 20},
            {"pid": 1956, "ppid": 9068, "threads": 17, "rss_bytes": 1,
             "image_name": "unknown.exe", "identity_status": "unknown"},
            {"pid": 3816, "ppid": 1, "threads": 9, "rss_bytes": 1},
            {"pid": 8204, "ppid": 3816, "threads": 7, "rss_bytes": 1}],
            "thread_count": 57, "sum_rss_bytes": 6}
        V.annotate_resource_roles(snapshot, {9296: "spare"}, 3816, 8204)
        natives = [row for row in snapshot["processes"] if row["role"] == "native"]
        self.assertEqual({row["pid"] for row in natives}, {1956, 6592})
        config = {"slots": [{"name": "spare"}], "deployment_budget": {
            "management_threads": 34, "management_memory_bytes": 100,
            "total_threads": 133, "reserved_memory_bytes": 100,
            "per_instance": {"total_threads": 33, "reserved_memory_bytes": 100}}}
        with patch.object(V.os, "getpid", return_value=3816):
            with self.assertRaisesRegex(RuntimeError, "slot spare threads 41 > 33"):
                V.validate_caddy_resources(snapshot, config, 8204)
        self.assertEqual(natives[0]["image_name"], "new.exe")
        self.assertEqual(natives[1]["identity_status"], "unknown")


class MetricsCollectorContracts(unittest.TestCase):
    token = "metricsdeploytoken0123456789abcdef"

    class Peer:
        def __init__(self, response, fragment=4096, tick=None):
            self.response = response
            self.fragment = fragment
            self.tick = tick
            self.request = bytearray()
            self.closed = False
        def __enter__(self): return self
        def __exit__(self, *_): self.closed = True
        def settimeout(self, timeout): self.timeout = timeout
        def send(self, data):
            count = min(7, len(data))  # Partial writes must retain the deadline.
            self.request.extend(data[:count])
            return count
        def recv(self, count):
            if self.tick is not None: self.tick[0] += .03
            count = min(count, self.fragment)
            chunk, self.response = self.response[:count], self.response[count:]
            return chunk

    def collect(self, body, code=200, headers=None, fragment=4096):
        raw = body.encode("utf-8")
        header = headers or f"Content-Length: {len(raw)}\r\nConnection: close\r\n"
        peer = self.Peer(f"HTTP/1.1 {code} Control\r\n{header}\r\n".encode()+raw, fragment)
        with patch.object(M.socket, "create_connection", return_value=peer):
            result = M.metrics({"_token": self.token}, {"control_addr": "127.0.0.1:19001"})
        self.assertTrue(peer.closed)
        return result, peer

    def test_metrics_reads_large_current_generation_without_changing_call_bound(self):
        body = json.dumps({"available": True, "generation": 7, "native": {"padding": "x"*10000}})
        result, peer = self.collect(body)
        self.assertTrue(result["available"])
        self.assertEqual(result["generation"], 7)
        self.assertEqual(len(result["native"]["padding"]), 10000)
        self.assertTrue(bytes(peer.request).startswith(b"GET /metrics HTTP/1.1\r\n"))
        self.assertNotIn(self.token, json.dumps(result))
        connection = Mock()
        connection.getresponse.return_value.read.return_value = b"x"*8193
        with patch.object(M.http.client, "HTTPConnection", return_value=connection):
            with self.assertRaises(ValueError):
                M.call("127.0.0.1:19001", "GET", "/status", self.token)
        connection.getresponse.return_value.read.assert_called_once_with(8193)

    def test_unavailable_native_is_unknown_and_recollected_not_cached(self):
        available = json.dumps({"available": True, "generation": 2, "native": {"requests": 12}})
        missing = json.dumps({"available": False, "generation": 3, "native": None, "reason": "no_native_instance"})
        self.assertTrue(self.collect(available)[0]["available"])
        actual, _ = self.collect(missing, code=503)
        self.assertFalse(actual["available"])
        self.assertIsNone(actual["native"])
        self.assertEqual(actual["generation"], 3)
        self.assertEqual(actual["reason"], "no_native_instance")

    def test_metrics_rejects_oversize_framing_invalid_json_and_token_echo(self):
        for body, headers in [("{}", "Content-Length: 65536\r\n"),
                              ("{}", "Content-Length: 2\r\nContent-Length: 2\r\n"),
                              ("{}", "Content-Length: 2\r\nTransfer-Encoding: chunked\r\n"),
                              ("{invalid}", None),
                              (json.dumps({"available": True, "generation": 1, "native": {"secret": self.token}}), None)]:
            with self.subTest(headers=headers):
                actual, _ = self.collect(body, headers=headers)
                self.assertFalse(actual["available"])
                self.assertIsNone(actual["native"])
                self.assertNotIn(self.token, json.dumps(actual))

    def test_invalid_operator_token_is_reported_as_unavailable(self):
        result, _ = self.collect("control token required\n", code=403)
        self.assertFalse(result["available"])
        self.assertEqual(result["http_status"], 403)

    def test_drip_read_has_one_absolute_collector_deadline(self):
        clock = [1.0]
        body = json.dumps({"available": True, "generation": 1, "native": {}}).encode()
        peer = self.Peer(b"HTTP/1.1 200 Control\r\nContent-Length: "+str(len(body)).encode()+b"\r\n\r\n"+body, 1, clock)
        with patch.object(M.time, "monotonic", side_effect=lambda: clock[0]), \
                patch.object(M.socket, "create_connection", return_value=peer):
            actual = M.metrics({"_token": self.token}, {"control_addr": "127.0.0.1:19001"}, timeout=.08)
        self.assertFalse(actual["available"])
        self.assertEqual(actual["error_type"], "TimeoutError")
        self.assertLess(clock[0], 1.2)
        self.assertTrue(peer.closed)


class DeploymentContracts(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(prefix="ku-deploy-regression-")
        self.root = Path(self.temporary.name)
        self.config = json.loads(Path(__file__).with_name("cluster.example.json").read_text())
        token = self.root / "token"
        token.write_text("a" * 32)
        token.chmod(0o600)
        self.config["control_token_file"] = str(token)
        if sys.platform == "win32":
            budget = self.config["deployment_budget"]
            budget["per_instance"]["platform_thread_reserve"] = 16
            budget["per_instance"]["total_threads"] += 16
            budget["total_threads"] = budget["max_parallel_instances"]*budget["per_instance"]["total_threads"]+budget["management_threads"]
        self.path = self.root / "cluster.json"
        self.path.write_text(json.dumps(self.config))

    def tearDown(self):
        self.temporary.cleanup()

    def test_rolling_spare_is_in_the_same_total_database_budget(self):
        self.config["deployment_budget"]["database_connections"] = 23
        self.path.write_text(json.dumps(self.config))
        with self.assertRaisesRegex(ValueError, "database_connections"):
            M.load(self.path)

    def test_background_and_supervision_threads_cannot_disappear_from_budget(self):
        self.config["deployment_budget"]["per_instance"]["total_threads"] = 8
        self.path.write_text(json.dumps(self.config))
        with self.assertRaisesRegex(ValueError, "supervisor/launcher"):
            M.load(self.path)

    def test_existing_environment_cannot_silently_override_declared_capacity(self):
        config = M.load(self.path)
        slot = config["slots"][0]
        slot["env"]["KU_DB_MAX_CONNECTIONS"] = "100000"
        with self.assertRaisesRegex(ValueError, "conflicts"):
            M.environment(config, slot)

    def test_windows_requires_explicit_platform_thread_reservation(self):
        for value in (None, 0, 15, "16", True):
            budget = self.config["deployment_budget"]["per_instance"]
            budget.pop("platform_thread_reserve", None)
            if value is not None:
                budget["platform_thread_reserve"] = value
            self.path.write_text(json.dumps(self.config))
            with patch.object(M.sys, "platform", "win32"):
                with self.assertRaisesRegex(ValueError, "platform_thread_reserve"):
                    M.load(self.path)

    def test_platform_threads_must_fit_both_each_slot_and_rolling_coexistence(self):
        budget = self.config["deployment_budget"]
        per = budget["per_instance"]
        per["platform_thread_reserve"] = 16
        required = per["event_threads"]+per["blocking_threads"]+9+16
        budget["total_threads"] = 3*required+budget["management_threads"]
        per["total_threads"] = required-1
        self.path.write_text(json.dumps(self.config))
        with patch.object(M.sys, "platform", "win32"):
            with self.assertRaisesRegex(ValueError, "supervisor/launcher.*platform_thread_reserve"):
                M.load(self.path)
            per["total_threads"] = required
            budget["total_threads"] -= 1
            self.path.write_text(json.dumps(self.config))
            with self.assertRaisesRegex(ValueError, "rolling coexistence.*total_threads"):
                M.load(self.path)

    def test_platform_reserve_does_not_increase_native_thread_budget(self):
        budget = self.config["deployment_budget"]
        per = budget["per_instance"]
        with patch.object(M.sys, "platform", "win32"):
            for reserve in (16, 32):
                per["platform_thread_reserve"] = reserve
                per["total_threads"] = per["event_threads"]+per["blocking_threads"]+9+reserve
                budget["total_threads"] = 3*per["total_threads"]+budget["management_threads"]
                self.path.write_text(json.dumps(self.config))
                config = M.load(self.path)
                environment = M.environment(config, config["slots"][0])
                self.assertEqual(environment["KU_THREAD_BUDGET"], str(per["event_threads"]+per["blocking_threads"]+2))
                self.assertEqual(environment["KU_EVENT_LOOPS"], str(per["event_threads"]))
                self.assertEqual(environment["KU_BLOCKING_THREADS"], str(per["blocking_threads"]))

    def test_reference_proxy_disables_replay_and_hides_control_routes(self):
        rendered = M.render(M.load(self.path))
        self.assertIn("retries 0", rendered)
        self.assertIn("no option redispatch", rendered)
        self.assertIn("http-request del-header X-Ku-Control-Token", rendered)
        self.assertIn("deny_status 404 if { path_beg /__ku/ }", rendered)

    def caddy_config(self):
        self.config.update(proxy_kind="caddy", caddy_admin_addr="127.0.0.1:19991", caddy_health_addr="127.0.0.1:19992")
        budget = self.config["deployment_budget"]
        budget.update(management_threads=34, management_memory_bytes=128*1024*1024)
        budget["total_threads"] = 3*budget["per_instance"]["total_threads"]+34
        budget["reserved_memory_bytes"] = 3*budget["per_instance"]["reserved_memory_bytes"]+budget["management_memory_bytes"]
        self.path.write_text(json.dumps(self.config))
        return M.load(self.path)

    def test_caddy_admin_is_loopback_unique_and_separate_from_business(self):
        for address in ("0.0.0.0:19991", "127.0.0.1:18081"):
            self.caddy_config()
            self.config["caddy_admin_addr"] = address
            self.path.write_text(json.dumps(self.config))
            with self.assertRaisesRegex(ValueError, "127.0.0.1|distinct"):
                M.load(self.path)

    def test_caddy_proxy_cost_cannot_disappear_from_deployment_reservations(self):
        for field in ("management_threads", "management_memory_bytes", "reserved_memory_bytes"):
            self.caddy_config()
            self.config["deployment_budget"][field] = 0
            self.path.write_text(json.dumps(self.config))
            with self.assertRaisesRegex(ValueError, "Caddy|reserved_memory_bytes"):
                M.load(self.path)

    def test_caddy_render_preserves_control_health_and_disables_both_retry_layers(self):
        config = self.caddy_config()
        rendered = json.loads(M.render(config))
        servers = rendered["apps"]["http"]["servers"]
        public = servers["ku_public"]
        self.assertEqual(public["routes"][0]["handle"][0]["status_code"], 404)
        self.assertIn("/__ku/*", public["routes"][0]["match"][0]["path"])
        proxy = public["routes"][1]["handle"][1]
        self.assertEqual(proxy["load_balancing"]["retries"], 0)
        self.assertEqual(proxy["load_balancing"]["try_duration"], 0)
        self.assertFalse(proxy["transport"]["keep_alive"]["enabled"])
        self.assertEqual(proxy["health_checks"]["active"]["upstream"], config["caddy_health_addr"])
        for slot, route in zip(config["slots"], servers["ku_health"]["routes"]):
            self.assertEqual(route["match"][0]["header"]["X-Ku-Proxy-Upstream"], [slot["listen_addr"]])
            self.assertEqual(route["handle"][0]["upstreams"][0]["dial"], slot["control_addr"])
            self.assertEqual(route["handle"][0]["headers"]["request"]["set"]["X-Ku-Control-Token"], [config["_token"]])

    def test_caddy_maintenance_keeps_fixed_state_and_removes_only_selected_backend(self):
        config = self.caddy_config()
        config["proxy_slot_states"] = {"a": "maint"}
        proxy = json.loads(M.render(config))["apps"]["http"]["servers"]["ku_public"]["routes"][1]["handle"][1]
        self.assertEqual([u["dial"] for u in proxy["upstreams"]], [s["listen_addr"] for s in config["slots"][1:]])
        self.config["proxy_slot_states"] = {"unknown": "ready"}
        self.path.write_text(json.dumps(self.config))
        with self.assertRaisesRegex(ValueError, "fixed slot"):
            M.load(self.path)

    def test_caddy_failed_reload_does_not_claim_state_changed_or_overwrite_foreign_config(self):
        config = self.caddy_config()
        with patch.object(M, "caddy_admin", return_value="{}") as admin:
            with self.assertRaisesRegex(RuntimeError, "owned deployment"):
                M.proxy_state(config, config["slots"][0], "maint")
            admin.assert_called_once_with(config, "GET", "/config/")
        with patch.object(M, "caddy_admin", side_effect=[M.render(config), RuntimeError("rejected load")]):
            with self.assertRaisesRegex(RuntimeError, "rejected load"):
                M.proxy_state(config, config["slots"][0], "maint")
        self.assertNotIn("proxy_slot_states", config)
        self.assertNotIn("proxy_slot_states", json.loads(self.path.read_text()))

    def test_caddy_registration_and_initial_optimistic_metric_do_not_count_as_ready(self):
        config = self.caddy_config()
        config["operation_timeout_seconds"] = .01
        config["_caddy_loaded_at"] = time.monotonic()
        with patch.object(M, "caddy_admin") as admin:
            with self.assertRaisesRegex(TimeoutError, "active health"):
                M.wait_proxy_ready(config, config["slots"][0])
            admin.assert_not_called()
        config["_caddy_loaded_at"] = time.monotonic()-11
        address = config["slots"][0]["listen_addr"]
        responses = [json.dumps([{"address": address}]), f'caddy_reverse_proxy_upstreams_healthy{{upstream="{address}"}} 0']
        with patch.object(M, "caddy_admin", side_effect=lambda _config, _method, path, **_: responses[0 if path.endswith("upstreams") else 1]):
            with self.assertRaisesRegex(TimeoutError, "active health"):
                M.wait_proxy_ready(config, config["slots"][0])

    def test_caddy_admin_absolute_deadline_includes_slow_headers(self):
        config = self.caddy_config()
        finished = threading.Event()
        with socket.socket() as listener:
            listener.bind(("127.0.0.1", 0))
            listener.listen(1)
            config["caddy_admin_addr"] = "127.0.0.1:"+str(listener.getsockname()[1])

            def drip():
                with listener.accept()[0] as stream:
                    stream.recv(4096)
                    for byte in b"HTTP/1.0 200 OK\r\n":
                        if finished.wait(.03):
                            break
                        try:
                            stream.sendall(bytes([byte]))
                        except OSError:
                            break

            thread = threading.Thread(target=drip, daemon=True)
            thread.start()
            started = time.monotonic()
            try:
                with self.assertRaises((TimeoutError, socket.timeout)):
                    M.caddy_admin(config, "GET", "/config/", deadline=started+.08)
                self.assertLess(time.monotonic()-started, .5)
            finally:
                finished.set()
                thread.join(timeout=1)
            self.assertFalse(thread.is_alive())

    def test_control_timeout_does_not_claim_the_supervisor_has_retired(self):
        config = M.load(self.path)
        with patch.object(M, "call", side_effect=TimeoutError("bounded status timeout")), \
                patch.object(M, "windows_control_port_absent", return_value=(False, {"result": "inconclusive"})):
            self.assertEqual(M.status(config, config["slots"][0])["phase"], "unreachable")
        with patch.object(M, "call", side_effect=ConnectionRefusedError("no listener")):
            self.assertEqual(M.status(config, config["slots"][0])["phase"], "absent")

    def test_windows_timeout_releases_reservation_only_after_exclusive_vacancy_proof(self):
        config = M.load(self.path)
        slot = config["slots"][0]
        with patch.object(M.sys, "platform", "win32"), \
                patch.object(M, "call", side_effect=TimeoutError("connect expired")), \
                patch.object(M, "windows_control_port_absent", return_value=(True, {"result": "vacant"})) as proof:
            state = M.status(config, slot)
        self.assertEqual(state["phase"], "absent")
        self.assertEqual(state["control_failure"]["error_type"], "TimeoutError")
        self.assertEqual(state["control_presence"]["result"], "vacant")
        proof.assert_called_once_with(slot["control_addr"])

    def test_exclusive_probe_never_reuses_or_listens_and_retains_inconclusive_port(self):
        probe = Mock()
        probe.bind.side_effect = OSError(98, "address occupied or lingering connection")
        with patch.object(M.socket, "SO_EXCLUSIVEADDRUSE", -5, create=True), \
                patch.object(M.socket, "socket") as factory:
            factory.return_value.__enter__.return_value = probe
            vacant, evidence = M.windows_control_port_absent("127.0.0.1:12345")
        self.assertFalse(vacant)
        self.assertEqual(evidence["result"], "inconclusive")
        probe.setsockopt.assert_called_once_with(socket.SOL_SOCKET, -5, 1)
        probe.bind.assert_called_once_with(("127.0.0.1", 12345))
        probe.listen.assert_not_called()
        factory.return_value.__exit__.assert_called_once()

    def test_live_owned_supervisor_without_listener_still_prevents_duplicate_start(self):
        config = M.load(self.path)
        slot = config["slots"][0]
        process = Mock(pid=12345)
        process.poll.return_value = None
        with patch.dict(M.OWNED_SUPERVISORS, {slot["name"]: process}, clear=True), \
                patch.object(M, "call", side_effect=ConnectionRefusedError("not bound yet")), \
                patch.object(M, "windows_control_port_absent") as proof, \
                patch.object(M.subprocess, "Popen") as spawn:
            state = M.status(config, slot)
            self.assertEqual(state["phase"], "unreachable")
            self.assertEqual(state["pid"], 12345)
            self.assertTrue(state["owned_supervisor_alive"])
            with self.assertRaisesRegex(RuntimeError, "cannot prove absence"):
                M.start(config, slot)
            proof.assert_not_called()
            spawn.assert_not_called()

    def test_listening_unresponsive_control_stays_unreachable_with_actual_failure_stage(self):
        config = M.load(self.path)
        slot = config["slots"][0]
        with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as listener:
            listener.bind(("127.0.0.1", 0))
            listener.listen(1)
            slot["control_addr"] = "127.0.0.1:" + str(listener.getsockname()[1])
            started = time.monotonic()
            state = M.status(config, slot)
            elapsed = time.monotonic()-started
        self.assertEqual(state["phase"], "unreachable", state)
        self.assertEqual(state["control_failure"]["stage"], "response_headers", state)
        self.assertLess(elapsed, 3)
        if sys.platform == "win32":
            self.assertEqual(state["control_presence"]["result"], "inconclusive", state)

    @unittest.skipUnless(sys.platform == "win32", "actual Winsock exclusive-bind contract")
    def test_actual_windows_vacant_endpoint_does_not_reserve_a_phantom_supervisor(self):
        config = M.load(self.path)
        slot = config["slots"][0]
        with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as allocation:
            allocation.bind(("127.0.0.1", 0))
            slot["control_addr"] = "127.0.0.1:" + str(allocation.getsockname()[1])
        state = M.status(config, slot)
        self.assertEqual(state["phase"], "absent", state)
        self.assertEqual(state["control_failure"]["stage"], "connect", state)
        if state["control_failure"]["error_type"] != "ConnectionRefusedError":
            self.assertEqual(state["control_presence"]["result"], "vacant", state)

    def test_failed_new_instance_never_stops_old_or_loses_previous_program(self):
        config = M.load(self.path)
        old, _, new = config["slots"]
        binary = self.root / "new-program"
        binary.write_bytes(b"fixture")
        previous = new["program"]

        def current(_config, slot):
            return {"phase": "absent" if slot["name"] == "spare" else "ready"}

        stopped = []
        with patch.object(M, "status", side_effect=current), \
                patch.object(M, "start", side_effect=TimeoutError("new never ready")), \
                patch.object(M, "stop", side_effect=lambda _config, slot: stopped.append(slot["name"])), \
                patch.object(M, "proxy_state"):
            with self.assertRaisesRegex(TimeoutError, "new never ready"):
                M.roll(config, old["name"], new["name"], binary)
        self.assertEqual(stopped, ["spare"])
        self.assertEqual(new["program"], previous)
        saved = json.loads(self.path.read_text())
        self.assertEqual(saved["slots"][2]["program"], previous)
        self.assertNotIn("_token", saved)


if __name__ == "__main__":
    unittest.main()
