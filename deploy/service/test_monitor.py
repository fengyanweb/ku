"""Permanent contracts for bounded observations; no native service is launched."""
import contextlib
import importlib.util
import json
from pathlib import Path
import signal
import tempfile
import unittest
from unittest.mock import patch

SPEC = importlib.util.spec_from_file_location("ku_monitor_test", Path(__file__).with_name("monitor.py"))
N = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(N)


class MonitorContracts(unittest.TestCase):
    def test_whitelist_excludes_tokens_errors_rids_and_payloads(self):
        raw = {"available": True, "generation": 2, "native": {
            "accepted": 17, "database": {"active_slots": 1, "waiters": 0,
                "pg_client_metrics_valid": False, "password": 123, "payload": "private"},
            "errors": {"counts": {"handler_500": 3}, "samples": [{"request_id": "private-rid", "sql": "private-sql"}]},
            "output": {"queued_bytes": 10, "token": 112233}, "secret": "private-token"}}
        result = N.safe_metrics(raw)
        text = json.dumps(result)
        self.assertNotIn("private", text)
        self.assertNotIn("password", text)
        self.assertNotIn("112233", text)
        self.assertFalse(result["native"]["database"]["pg_client_metrics_valid"])
        self.assertEqual(result["native"]["database"]["active_slots"], 1)
        self.assertFalse(N.safe_metrics({"available": True, "native": None})["available"])

    def test_rotation_has_fixed_file_bytes_and_visible_eviction_and_drop(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory); log = N.Log(root, 256, 3)
            for index in range(12): log.append({"sequence": index, "counts": list(range(30))})
            self.assertLessEqual(len(list(root.glob("metrics.*.jsonl"))), 3)
            self.assertTrue(all(path.stat().st_size <= 256 for path in root.glob("metrics.*.jsonl")))
            self.assertGreater(log.evicted, 0)
            log.append({"sequence": 13, "counts": "x"*1000})
            records = [json.loads(line) for path in root.glob("metrics.*.jsonl") for line in path.read_text().splitlines()]
            self.assertTrue(any(record.get("kind") == "snapshot_dropped" for record in records))
            self.assertEqual(log.dropped, 1)

    def test_proc_identity_and_units_are_checked_before_publish(self):
        fields = ["0"]*22
        for index, value in {1: 7, 7: 8, 9: 9, 11: 100, 12: 20, 17: 4, 19: 1234, 21: 6}.items(): fields[index] = str(value)
        text = "55 (worker name) "+" ".join(fields)
        def entries(_):
            return contextlib.nullcontext(iter(["0", "1", "2"]))
        with patch.object(N, "read_small", return_value=text), patch.object(N.os, "scandir", side_effect=entries), \
                patch.object(N.os, "sysconf", return_value=4096, create=True):
            record = N.proc_sample(55)
        self.assertEqual(record["starttime_ticks"], 1234)
        self.assertEqual(record["rss_bytes"], 6*4096)
        self.assertEqual(record["fd_count"], 3)
        with patch.object(N, "read_small", return_value=text), patch.object(N.os, "scandir", side_effect=PermissionError), \
                patch.object(N.os, "sysconf", return_value=4096, create=True):
            partial = N.proc_sample(55)
        self.assertIsNone(partial["fd_count"])
        self.assertEqual(partial["fd_error_type"], "PermissionError")
        self.assertIn("rss_bytes", partial)
        fields[19] = "1235"
        changed = "55 (worker name) "+" ".join(fields)
        with patch.object(N, "read_small", side_effect=[text, changed]), patch.object(N.os, "scandir", side_effect=entries), \
                patch.object(N.os, "sysconf", return_value=4096, create=True):
            with self.assertRaisesRegex(ValueError, "identity changed"): N.proc_sample(55)

    def test_status_deadline_does_not_leave_an_alarm_installed(self):
        if not hasattr(signal, "setitimer"): self.skipTest("POSIX deadline only")
        previous = signal.getsignal(signal.SIGALRM)
        with patch.object(N.M, "status", side_effect=lambda *_: signal.raise_signal(signal.SIGALRM)):
            with self.assertRaises(TimeoutError): N.status_once({}, {})
        self.assertEqual(signal.getsignal(signal.SIGALRM), previous)
        self.assertEqual(signal.getitimer(signal.ITIMER_REAL)[0], 0)

    def test_snapshot_deduplicates_unit_pids_and_requires_multiple_queue_samples(self):
        config = {"slots": [{"name": "a"}, {"name": "b"}], "resource_cgroups": {"a": "/sys/fs/cgroup/test", "b": "/sys/fs/cgroup/test"}}
        metric = {"available": True, "generation": 1, "native": {"loops": [{"pending": 2}],
            "tasks": [{"ready": 0}], "task_blocking": {"queued": 0},
            "database": {"pg_client_metrics_valid": False, "pg_client_waiters": 12}}}
        group = {"pids": [100, 200, 300, 110, 210, 310], "memory_current": 12, "memory_max": 24,
                 "pids_current": 12, "pids_max": 64, "memory_events": {"oom_kill": 0}, "cgroup_inode": 100}
        proxy = "# pxname,svname,qcur,scur,stot,econ,eresp,hrsp_5xx,status\nku,a,0,1,20,0,0,0,UP\n"
        clock = [10.0]
        def proc(pid): return {"pid": pid, "ppid": {200: 100, 300: 200, 210: 110, 310: 210}.get(pid, 1), "rss_bytes": 10, "starttime_ticks": 1}
        with tempfile.TemporaryDirectory() as directory, patch.object(N.platform, "system", return_value="Linux"), \
                patch.object(N, "status_once", side_effect=lambda _, slot: {"phase": "ready", "pid": 200 if slot["name"] == "a" else 210, "generation": 1, "restarts": 0}), \
                patch.object(N.M, "metrics", return_value=metric), patch.object(N, "cgroup_sample", return_value=group), \
                patch.object(N, "proc_sample", side_effect=proc), patch.object(N.M, "proxy_command", return_value=proxy), \
                patch.object(N.os, "sysconf", return_value=100, create=True), \
                patch.object(N.time, "monotonic", side_effect=lambda: clock[0]):
            monitor = N.Monitor(config, Path(directory), 1048576, 3)
            for index in range(3): clock[0] = 10+5*index; monitor.sample(clock[0], 0)
            rows = [json.loads(line) for line in (Path(directory)/"metrics.0.jsonl").read_text().splitlines()]
            self.assertEqual(len(rows[-1]["processes"]), 6)
            self.assertEqual(rows[-1]["sum_observed_RSS_bytes"], 60)
            self.assertFalse(any(a["kind"] == "queue_not_retiring_draft" for a in rows[0]["alerts"]))
            queues = [a for a in rows[-1]["alerts"] if a["kind"] == "queue_not_retiring_draft"]
            self.assertTrue(queues)
            self.assertTrue(all(a["queue"] != "database_pg_client_waiters" for a in queues))

    @unittest.skipUnless(hasattr(N.time, "clock_gettime_ns"), "Linux observer clock contract")
    def test_receipt_distinguishes_finite_continuous_gaps_and_stop(self):
        with tempfile.TemporaryDirectory() as directory:
            receipt = N.Receipt.__new__(N.Receipt)
            receipt.path = Path(directory)/"observer.json"; receipt.interval = 5
            receipt.record = {"schema": "ku-production-observer-v1", "identity": {"pid": 44}}
            receipt.continuous = False
            row = {"missed_intervals": 0, "collection_elapsed_ms": 10}
            receipt.publish(True, row)
            self.assertFalse(json.loads(receipt.path.read_text())["continuous_collection_running"])
            receipt.continuous = True; receipt.publish(True, row)
            self.assertTrue(json.loads(receipt.path.read_text())["continuous_collection_running"])
            receipt.publish(True, {"missed_intervals": 1, "collection_elapsed_ms": 10})
            self.assertFalse(json.loads(receipt.path.read_text())["continuous_collection_running"])
            receipt.publish(True, {"missed_intervals": 0, "collection_elapsed_ms": 5001})
            self.assertFalse(json.loads(receipt.path.read_text())["continuous_collection_running"])
            last = receipt.record["last_sample_clock_monotonic_ns"]; receipt.publish(False)
            self.assertFalse(json.loads(receipt.path.read_text())["continuous_collection_running"])
            self.assertEqual(receipt.record["last_sample_clock_monotonic_ns"], last)
            self.assertLess(receipt.path.stat().st_size, 8192)

    def test_cli_refuses_existing_output_instead_of_overwriting(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory); config = root/"config.json"; config.write_text("{}")
            output = root/"existing"; output.mkdir(); (output/"run.json").write_text("keep")
            with patch.object(N.M, "load", return_value={"slots": []}), \
                    patch("sys.argv", ["monitor.py", "--config", str(config), "--output-dir", str(output)]):
                with self.assertRaises(FileExistsError): N.main()
            self.assertEqual((output/"run.json").read_text(), "keep")


if __name__ == "__main__": unittest.main()
