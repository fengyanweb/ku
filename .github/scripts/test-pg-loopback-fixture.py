#!/usr/bin/env python3
"""Read-only unit checks for the opt-in PostgreSQL fixture's safety boundaries."""

from __future__ import annotations

import argparse
from contextlib import ExitStack
import hashlib
import importlib.util
import io
import json
import os
from pathlib import Path
import subprocess
import sys
import tarfile
import tempfile
from types import SimpleNamespace
import unittest
from unittest.mock import MagicMock, call, patch

sys.dont_write_bytecode = True
SPEC = importlib.util.spec_from_file_location(
    "pg_fixture", Path(__file__).with_name("pg-loopback-fixture.py")
)
assert SPEC is not None and SPEC.loader is not None
FIXTURE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(FIXTURE)


class PgFixtureTests(unittest.TestCase):
    @staticmethod
    def marker_input() -> SimpleNamespace:
        path = MagicMock(spec=Path)
        state = SimpleNamespace(present=False, streams=[], text="")

        def metadata():
            if not state.present:
                raise FileNotFoundError("inert marker absent")
            return SimpleNamespace(st_mode=0o100600)

        def open_marker(mode, **kwargs):
            if mode == "x" and state.present:
                raise FileExistsError("inert operation already owned")
            state.present = True
            state.text = ""
            stream = MagicMock(spec=io.TextIOBase)

            def write(text):
                state.text += text
                return len(text)

            stream.write.side_effect = write
            stream.__enter__.return_value = stream
            state.streams.append(stream)
            return stream

        def unlink(**kwargs):
            if not state.present:
                raise FileNotFoundError("inert marker absent")
            state.present = False

        path.lstat.side_effect = metadata
        path.exists.side_effect = lambda: state.present
        path.open.side_effect = open_marker
        path.unlink.side_effect = unlink
        return SimpleNamespace(path=path, state=state)

    @staticmethod
    def mocked_verification_inputs(record_present: bool = True) -> SimpleNamespace:
        root = MagicMock(spec=Path)
        root.name = FIXTURE.PREFIX + "0" * 32
        root.resolve.return_value = root
        target = MagicMock(spec=Path)
        target.resolve.return_value = root.parent
        record = MagicMock(spec=Path)
        manifest = MagicMock(spec=Path)
        state = {"record_present": record_present}

        def unlink(*, missing_ok: bool = False) -> None:
            if not state["record_present"] and not missing_ok:
                raise FileNotFoundError("no previous verification record")
            state["record_present"] = False

        def read_manifest(*, encoding: str) -> str:
            if state["record_present"]:
                raise AssertionError("previous success record survived into the new attempt")
            return "{invalid fixture manifest"

        record.unlink.side_effect = unlink
        manifest.read_text.side_effect = read_manifest
        lock = PgFixtureTests.marker_input()
        active = PgFixtureTests.marker_input()
        pid = MagicMock(spec=Path)
        pid.exists.return_value = False

        def pid_metadata():
            if not pid.exists():
                raise FileNotFoundError("inert PID absent")
            return SimpleNamespace(st_mode=0o100600)

        pid.lstat.side_effect = pid_metadata
        data = MagicMock(spec=Path)
        data.__truediv__.side_effect = {"postmaster.pid": pid}.__getitem__
        paths = {
            "verification.json": record,
            "fixture.json": manifest,
            "operation.lock": lock.path,
            "server.active": active.path,
            "data": data,
        }

        def publish(destination):
            if destination is not record or not lock.state.present:
                raise AssertionError("receipt publication requires this attempt's lease")
            if active.state.present:
                raise AssertionError("receipt publication preceded active marker release")
            if not lock.state.streams or lock.state.streams[-1].close.call_count != 1:
                raise AssertionError("receipt publication preceded closing its writer")
            state["receipt_text"] = lock.state.text
            state["record_present"] = True
            lock.state.present = False
            return record

        lock.path.replace.side_effect = publish
        root.__truediv__.side_effect = paths.__getitem__
        return SimpleNamespace(
            args=argparse.Namespace(fixture=root), target=target, record=record,
            manifest=manifest, state=state, lock=lock, active=active, pid=pid, paths=paths,
        )

    @classmethod
    def startup_inputs(cls) -> SimpleNamespace:
        # Reuse the existing invalidation contract without touching real fixtures.
        inputs = cls.mocked_verification_inputs()
        root = inputs.args.fixture
        password = MagicMock(spec=Path)
        password.read_text.return_value = "inert-contract-password"
        inputs.manifest.read_text.side_effect = None
        inputs.manifest.read_text.return_value = json.dumps({
            "format": 1, "version": FIXTURE.VERSION, "source_sha256": FIXTURE.SOURCE_SHA256,
            "port": 23456,
        })
        paths = inputs.paths
        paths["password.txt"] = password
        for name in ("portable", "db.conn", "startup.log", "server.log", "live-test.log"):
            paths[name] = MagicMock(spec=Path)
        root.__truediv__.side_effect = paths.__getitem__
        inputs.args.test_binary = MagicMock(spec=Path)
        inputs.args.ku_binary = MagicMock(spec=Path)
        return inputs

    def startup_scope(self, inputs: SimpleNamespace) -> ExitStack:
        stack = ExitStack()
        stack.enter_context(patch.object(FIXTURE, "os", SimpleNamespace(name="nt", environ={})))
        stack.enter_context(patch.object(FIXTURE, "TARGET", inputs.target))
        stack.enter_context(patch.object(FIXTURE, "SECRET", ""))
        stack.enter_context(patch.object(FIXTURE, "fixture_environment"))
        stack.enter_context(patch.object(FIXTURE.subprocess, "CREATE_NEW_PROCESS_GROUP", 0x200, create=True))
        stack.enter_context(patch.object(FIXTURE.subprocess, "CREATE_NO_WINDOW", 0x08000000, create=True))
        stack.enter_context(patch("sys.stdout", new_callable=io.StringIO))
        inputs.run = stack.enter_context(patch.object(FIXTURE, "run", side_effect=RuntimeError("inert live boundary")))
        stack.enter_context(patch.object(FIXTURE.BOUNDS, "os", SimpleNamespace(name="nt")))
        inputs.tree = stack.enter_context(patch.object(FIXTURE.BOUNDS, "kill_process_tree",
                                                      wraps=FIXTURE.BOUNDS.kill_process_tree))
        inputs.numeric = stack.enter_context(patch.object(FIXTURE.subprocess, "run",
                                                         side_effect=AssertionError("numeric tree cleanup forbidden")))
        return stack

    def test_pg_suspended_attach_resume_wait_order_reaches_inert_live_boundary(self) -> None:
        inputs = self.startup_inputs()
        process, job = MagicMock(), MagicMock()
        process.poll.return_value = 0
        events = []
        process.wait.side_effect = lambda **_: events.append("wait") or 0
        job.wait_empty.side_effect = lambda deadline: events.append("drain") or True
        with self.startup_scope(inputs), \
             patch.object(FIXTURE.subprocess, "Popen", side_effect=lambda *a, **k: events.append("popen") or process) as popen, \
             patch.object(FIXTURE.BOUNDS.WindowsJob, "attach", side_effect=lambda p: events.append("attach") or job), \
             patch.object(FIXTURE.BOUNDS, "resume_suspended_windows_process", side_effect=lambda p: events.append("resume")), \
             self.assertRaisesRegex(RuntimeError, "inert live boundary"):
            FIXTURE.verify(inputs.args)
        self.assertEqual(["popen", "attach", "resume", "wait", "drain", "wait"], events)
        self.assertTrue(popen.call_args.kwargs["creationflags"] & 4)
        self.assertEqual(2, process.wait.call_count)
        self.assertEqual(25, process.wait.call_args_list[0].kwargs["timeout"])
        self.assertGreater(process.wait.call_args.kwargs["timeout"], 0)
        self.assertLessEqual(process.wait.call_args.kwargs["timeout"], 5)
        job.terminate.assert_called_once_with()
        job.wait_empty.assert_called_once()
        job.close.assert_called_once_with()
        inputs.tree.assert_called_once_with(process, job)
        process.kill.assert_not_called()
        inputs.numeric.assert_not_called()
        inputs.record.open.assert_not_called()

    def test_pg_failed_attach_never_resumes_or_stops_by_later_pid_file(self) -> None:
        for failure in (None, OSError("Job setup failure")):
            with self.subTest(failure=failure):
                inputs = self.startup_inputs()
                # A late private PID-looking leaf cannot authorize pg_ctl stop.
                inputs.pid.exists.side_effect = [False, True, True]
                process = MagicMock()
                process.poll.return_value = None
                with self.startup_scope(inputs), \
                     patch.object(FIXTURE.subprocess, "Popen", return_value=process), \
                     patch.object(FIXTURE.BOUNDS.WindowsJob, "attach", return_value=None,
                                  side_effect=failure), \
                     patch.object(FIXTURE.BOUNDS, "resume_suspended_windows_process") as resume, \
                     self.assertRaises((OSError, RuntimeError)):
                    FIXTURE.verify(inputs.args)
                resume.assert_not_called()
                inputs.run.assert_not_called()
                inputs.tree.assert_called_once_with(process, None)
                inputs.numeric.assert_not_called()
                process.kill.assert_called_once_with()
                self.assertEqual(1, process.wait.call_count)
                self.assertGreater(process.wait.call_args.kwargs["timeout"], 0)
                self.assertLessEqual(process.wait.call_args.kwargs["timeout"], 5)
                inputs.record.open.assert_not_called()

    def test_pg_resume_startup_and_reap_failures_do_not_write_success(self) -> None:
        for stage in ("resume", "nonzero", "startup_timeout", "reap_timeout"):
            with self.subTest(stage=stage):
                inputs = self.startup_inputs()
                process, job = MagicMock(), MagicMock()
                job.wait_empty.return_value = True
                process.poll.return_value = None
                process.wait.return_value = 1 if stage == "nonzero" else 0
                if stage == "startup_timeout":
                    process.wait.side_effect = [subprocess.TimeoutExpired("inert start", 25), 0]
                elif stage == "reap_timeout":
                    process.wait.side_effect = [subprocess.TimeoutExpired("inert held root", 5)]
                resume_error = OSError("resume failed") if stage in ("resume", "reap_timeout") else None
                with self.startup_scope(inputs), \
                     patch.object(FIXTURE.subprocess, "Popen", return_value=process), \
                     patch.object(FIXTURE.BOUNDS.WindowsJob, "attach", return_value=job), \
                     patch.object(FIXTURE.BOUNDS, "resume_suspended_windows_process", side_effect=resume_error), \
                     self.assertRaises((OSError, RuntimeError, subprocess.TimeoutExpired)):
                    FIXTURE.verify(inputs.args)
                job.terminate.assert_called_once_with()
                job.wait_empty.assert_called_once()
                job.close.assert_called_once_with()
                inputs.tree.assert_called_once_with(process, job)
                process.kill.assert_called_once_with()
                inputs.run.assert_not_called()
                inputs.numeric.assert_not_called()
                inputs.record.open.assert_not_called()

    def cleanup_inputs(self, failure: str | None = None, *, live_failure: bool = False) -> SimpleNamespace:
        inputs = self.startup_inputs()
        inputs.events = []
        # A numeric PID is deliberately unavailable: only the retained owner
        # authorizes cleanup. All process and filesystem operations are inert.
        inputs.process = MagicMock(spec=["poll", "kill", "wait"])
        inputs.job = MagicMock(spec=["terminate", "wait_empty", "close"])
        pid_checks = waits = 0

        def pid_exists():
            nonlocal pid_checks
            pid_checks += 1
            stage = {1: "initial_pid", 2: "stop_pid", 3: "final_pid"}[pid_checks]
            inputs.events.append(stage)
            if failure == stage:
                raise OSError(stage + " injected failure")
            return pid_checks == 2

        def operation(stage):
            def perform(*args, **kwargs):
                inputs.events.append(stage)
                if failure == stage:
                    raise OSError(stage + " injected failure")
            return perform

        def wait(*, timeout):
            nonlocal waits
            waits += 1
            stage = "startup_wait" if waits == 1 else "root_wait"
            operation(stage)()
            return 0

        def run(command, *args, **kwargs):
            if command[1] == "stop":
                operation("stop")()
                return b""
            inputs.events.append("live")
            if live_failure:
                raise RuntimeError("original live failure")
            return b"test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured\n"

        inputs.pid.exists.side_effect = pid_exists
        inputs.process.poll.side_effect = operation("root_poll")
        inputs.process.kill.side_effect = operation("root_kill")
        inputs.process.wait.side_effect = wait
        inputs.job.terminate.side_effect = operation("job_terminate")
        inputs.job.wait_empty.side_effect = lambda deadline: operation("job_drain")() or True
        inputs.job.close.side_effect = operation("job_close")
        inputs.live_run = run
        return inputs

    def cleanup_scope(self, inputs: SimpleNamespace) -> ExitStack:
        stack = self.startup_scope(inputs)
        inputs.run.side_effect = inputs.live_run
        inputs.popen = stack.enter_context(patch.object(FIXTURE.subprocess, "Popen", return_value=inputs.process))
        stack.enter_context(patch.object(FIXTURE.BOUNDS.WindowsJob, "attach", return_value=inputs.job))
        stack.enter_context(patch.object(FIXTURE.BOUNDS, "resume_suspended_windows_process"))
        return stack

    @staticmethod
    def inject_marker_failure(marker: SimpleNamespace, stage: str, primary: BaseException,
                              *, mode: str | None = None) -> None:
        open_marker = marker.path.open.side_effect

        def fail_marker(actual_mode, **kwargs):
            if mode is not None and actual_mode != mode:
                return open_marker(actual_mode, **kwargs)
            if stage == "open":
                raise primary
            stream = open_marker(actual_mode, **kwargs)
            marker.failed_stream = stream
            if stage in ("write", "double"):
                stream.write.side_effect = primary
            elif stage == "short":
                stream.write.side_effect = lambda text: len(text) - 1
            if stage == "close":
                stream.close.side_effect = primary
            elif stage == "double":
                stream.close.side_effect = OSError("inert secondary marker close failure")
            return stream

        marker.path.open.side_effect = fail_marker

    def test_pg_marker_write_and_close_failure_preserves_primary_and_stream(self) -> None:
        for mode in ("x", "w"):
            with self.subTest(mode=mode):
                marker = self.marker_input()
                primary = OSError("inert first marker write failure")
                self.inject_marker_failure(marker, "double", primary)
                with self.assertRaises(FIXTURE.BOUNDS.CleanupFailure) as raised:
                    FIXTURE.write_marker(marker.path, "bounded marker\n", mode=mode)
                self.assertIs(primary, raised.exception.primary)
                self.assertIs(primary, raised.exception.__cause__)
                self.assertIs(marker.failed_stream, raised.exception.marker_stream)
                self.assertIn("marker close", raised.exception.cleanup_summary)
                marker.failed_stream.write.assert_called_once_with("bounded marker\n")
                marker.failed_stream.close.assert_called_once_with()
                marker.path.unlink.assert_not_called()
                marker.path.replace.assert_not_called()
                self.assertTrue(marker.state.present)

    def test_pg_marker_short_or_unknown_write_closes_without_claiming_success(self) -> None:
        for written in (0, 3, None, True, MagicMock()):
            with self.subTest(written=repr(written)):
                marker = self.marker_input()
                stream = MagicMock(spec=io.TextIOBase)
                stream.write.return_value = written
                marker.path.open.side_effect = None
                marker.path.open.return_value = stream
                with self.assertRaisesRegex(OSError, "Incomplete") as raised:
                    FIXTURE.write_marker(marker.path, "owned\n")
                self.assertIs(stream, raised.exception.marker_stream)
                stream.close.assert_called_once_with()
                marker.path.unlink.assert_not_called()
                marker.path.replace.assert_not_called()

    def test_pg_marker_utf8_byte_bound_precedes_open(self) -> None:
        marker = self.marker_input()
        with patch.object(FIXTURE, "MAX_RECEIPT_BYTES", 6):
            FIXTURE.write_marker(marker.path, "\u4e2d\u6587")
        marker.path.open.assert_called_once_with("x", encoding="utf-8", newline="\n")
        marker.state.streams[0].write.assert_called_once_with("\u4e2d\u6587")
        marker.state.streams[0].close.assert_called_once_with()
        oversized = self.marker_input()
        with patch.object(FIXTURE, "MAX_RECEIPT_BYTES", 5), self.assertRaisesRegex(ValueError, "byte bound"):
            FIXTURE.write_marker(oversized.path, "\u4e2d\u6587")
        oversized.path.open.assert_not_called()

    def test_pg_exclusive_lease_creation_failure_preserves_receipt_and_blocks_io(self) -> None:
        for stage in ("open", "write", "close", "double", "short"):
            with self.subTest(stage=stage):
                inputs = self.cleanup_inputs()
                primary = OSError("inert lease " + stage + " failure")
                self.inject_marker_failure(inputs.lock, stage, primary)
                with self.cleanup_scope(inputs), self.assertRaises((OSError, RuntimeError)) as raised:
                    FIXTURE.verify(inputs.args)
                if stage in ("open", "write"):
                    self.assertIs(primary, raised.exception)
                elif stage == "double":
                    self.assertIs(primary, raised.exception.primary)
                if stage != "open":
                    self.assertIs(inputs.lock.failed_stream, raised.exception.marker_stream)
                    inputs.lock.failed_stream.close.assert_called_once_with()
                self.assertEqual(stage != "open", inputs.lock.state.present)
                self.assertTrue(inputs.state["record_present"])
                self.assertFalse(inputs.active.state.present)
                inputs.lock.path.unlink.assert_not_called()
                inputs.lock.path.replace.assert_not_called()
                inputs.active.path.open.assert_not_called()
                inputs.record.unlink.assert_not_called()
                inputs.manifest.read_text.assert_not_called()
                inputs.paths["password.txt"].read_text.assert_not_called()
                inputs.paths["startup.log"].open.assert_not_called()
                inputs.popen.assert_not_called()
                inputs.run.assert_not_called()
                inputs.tree.assert_not_called()

    def test_pg_exclusive_active_creation_failure_keeps_lease_and_blocks_start(self) -> None:
        for stage in ("open", "write", "close", "double", "short"):
            with self.subTest(stage=stage):
                inputs = self.cleanup_inputs()
                primary = OSError("inert active " + stage + " failure")
                self.inject_marker_failure(inputs.active, stage, primary)
                with self.cleanup_scope(inputs), self.assertRaises((OSError, RuntimeError)) as raised:
                    FIXTURE.verify(inputs.args)
                if stage in ("open", "write"):
                    self.assertIs(primary, raised.exception)
                elif stage == "double":
                    self.assertIs(primary, raised.exception.primary)
                if stage != "open":
                    self.assertIs(inputs.active.failed_stream, raised.exception.marker_stream)
                    inputs.active.failed_stream.close.assert_called_once_with()
                self.assertTrue(inputs.lock.state.present)
                self.assertEqual(stage != "open", inputs.active.state.present)
                self.assertFalse(inputs.state["record_present"])
                inputs.lock.path.unlink.assert_not_called()
                inputs.lock.path.replace.assert_not_called()
                inputs.active.path.unlink.assert_not_called()
                inputs.record.unlink.assert_called_once_with(missing_ok=True)
                inputs.manifest.read_text.assert_called_once_with(encoding="utf-8")
                inputs.paths["startup.log"].open.assert_not_called()
                inputs.popen.assert_not_called()
                inputs.run.assert_not_called()
                inputs.tree.assert_not_called()

    def test_pg_exclusive_marker_lookup_uncertainty_preserves_old_receipt(self) -> None:
        for name in ("active", "pid"):
            with self.subTest(name=name):
                inputs = self.cleanup_inputs()
                primary = PermissionError("inert marker metadata failure")
                marker = inputs.active.path if name == "active" else inputs.pid
                marker.lstat.side_effect = primary
                with self.cleanup_scope(inputs), self.assertRaises(PermissionError) as raised:
                    FIXTURE.verify(inputs.args)
                self.assertIs(primary, raised.exception)
                self.assertTrue(inputs.lock.state.present)
                self.assertTrue(inputs.state["record_present"])
                inputs.lock.path.unlink.assert_not_called()
                inputs.lock.path.replace.assert_not_called()
                inputs.record.unlink.assert_not_called()
                inputs.manifest.read_text.assert_not_called()
                inputs.paths["password.txt"].read_text.assert_not_called()
                inputs.popen.assert_not_called()
                inputs.run.assert_not_called()

    def test_pg_exclusive_stop_failure_stays_sticky_after_confirmed_fallback(self) -> None:
        for live_failure in (False, True):
            with self.subTest(live_failure=live_failure):
                inputs = self.cleanup_inputs()
                primary = RuntimeError("inert first live failure") if live_failure else OSError("inert first stop failure")
                stop_error = OSError("inert later stop failure") if live_failure else primary
                with self.cleanup_scope(inputs):
                    inputs.run.side_effect = [primary, stop_error] if live_failure else [
                        b"test result: ok. 1 passed; 0 failed; 0 ignored;\n", stop_error,
                    ]
                    with self.assertRaises((OSError, RuntimeError)) as raised:
                        FIXTURE.verify(inputs.args)
                self.assertIs(primary, raised.exception.primary if live_failure else raised.exception)
                self.assertIs(inputs.job, raised.exception.job)
                self.assertIs(inputs.process, raised.exception.process)
                self.assertIs(inputs.args.fixture, raised.exception.fixture_root)
                inputs.job.wait_empty.assert_called_once()
                inputs.job.close.assert_called_once_with()
                self.assertIn("root_wait", inputs.events)
                self.assertIn("final_pid", inputs.events)
                self.assertTrue(inputs.lock.state.present)
                self.assertTrue(inputs.active.state.present)
                inputs.active.path.unlink.assert_not_called()
                inputs.lock.path.unlink.assert_not_called()
                inputs.lock.path.replace.assert_not_called()
                reads = inputs.manifest.read_text.call_count
                with self.cleanup_scope(inputs), self.assertRaises(FileExistsError):
                    FIXTURE.verify(inputs.args)
                self.assertEqual(reads, inputs.manifest.read_text.call_count)
                inputs.popen.assert_not_called()
                inputs.run.assert_not_called()
                inputs.tree.assert_not_called()

    def test_pg_exclusive_active_release_failure_preserves_primary_and_owners(self) -> None:
        for live_failure in (False, True):
            with self.subTest(live_failure=live_failure):
                inputs = self.cleanup_inputs()
                primary = RuntimeError("inert first live failure") if live_failure else None
                inputs.active.path.unlink.side_effect = OSError("inert active unlink failure")
                with self.cleanup_scope(inputs):
                    if primary is not None:
                        inputs.run.side_effect = [primary, b""]
                    with self.assertRaises(FIXTURE.BOUNDS.CleanupFailure) as raised:
                        FIXTURE.verify(inputs.args)
                self.assertIs(primary, raised.exception.primary)
                self.assertIs(inputs.job, raised.exception.job)
                self.assertIs(inputs.process, raised.exception.process)
                self.assertIs(inputs.args.fixture, raised.exception.fixture_root)
                self.assertIn("active marker release", raised.exception.cleanup_summary)
                inputs.active.path.unlink.assert_called_once_with()
                self.assertTrue(inputs.lock.state.present)
                self.assertTrue(inputs.active.state.present)
                self.assertFalse(inputs.state["record_present"])
                inputs.lock.path.unlink.assert_not_called()
                inputs.lock.path.replace.assert_not_called()
                self.assertIn("final_pid", inputs.events)

    def test_pg_exclusive_receipt_writer_failures_retain_lease_and_stream(self) -> None:
        for stage in ("open", "write", "close", "double", "short"):
            with self.subTest(stage=stage):
                inputs = self.cleanup_inputs()
                primary = OSError("inert receipt " + stage + " failure")
                self.inject_marker_failure(inputs.lock, stage, primary, mode="w")
                with self.cleanup_scope(inputs), self.assertRaises((OSError, RuntimeError)) as raised:
                    FIXTURE.verify(inputs.args)
                if stage in ("open", "write"):
                    self.assertIs(primary, raised.exception)
                elif stage == "double":
                    self.assertIs(primary, raised.exception.primary)
                if stage != "open":
                    self.assertIs(inputs.lock.failed_stream, raised.exception.marker_stream)
                    inputs.lock.failed_stream.close.assert_called_once_with()
                inputs.active.path.unlink.assert_called_once_with()
                self.assertFalse(inputs.active.state.present)
                self.assertTrue(inputs.lock.state.present)
                self.assertFalse(inputs.state["record_present"])
                inputs.record.open.assert_not_called()
                inputs.lock.path.replace.assert_not_called()
                inputs.lock.path.unlink.assert_not_called()
                self.assertIn("final_pid", inputs.events)

    def test_pg_exclusive_receipt_replace_failure_never_unlinks_or_signs_success(self) -> None:
        inputs = self.cleanup_inputs()
        primary = OSError("inert receipt rename failure")
        inputs.lock.path.replace.side_effect = primary
        with self.cleanup_scope(inputs), self.assertRaises(OSError) as raised:
            FIXTURE.verify(inputs.args)
        self.assertIs(primary, raised.exception)
        self.assertTrue(inputs.lock.state.present)
        self.assertFalse(inputs.active.state.present)
        self.assertFalse(inputs.state["record_present"])
        inputs.lock.path.replace.assert_called_once_with(inputs.record)
        inputs.lock.path.unlink.assert_not_called()
        inputs.record.unlink.assert_called_once_with(missing_ok=True)
        inputs.record.open.assert_not_called()
        self.assertEqual(2, len(inputs.lock.state.streams))
        for stream in inputs.lock.state.streams:
            stream.close.assert_called_once_with()

    def test_pg_exclusive_operation_release_failure_preserves_body_primary(self) -> None:
        inputs = self.mocked_verification_inputs()
        primary = RuntimeError("inert first operation failure")
        inputs.lock.path.unlink.side_effect = OSError("inert lease release failure")
        with self.assertRaises(FIXTURE.BOUNDS.CleanupFailure) as raised:
            with FIXTURE.operation(inputs.args.fixture):
                raise primary
        self.assertIs(primary, raised.exception.primary)
        self.assertIs(primary, raised.exception.__cause__)
        self.assertIn("operation release", raised.exception.cleanup_summary)
        self.assertTrue(inputs.lock.state.present)
        inputs.lock.path.unlink.assert_called_once_with()
        inputs.lock.path.replace.assert_not_called()
        inputs.record.unlink.assert_not_called()

    def test_pg_exclusive_post_effect_rename_error_never_rolls_back_new_owner(self) -> None:
        inputs = self.cleanup_inputs()
        primary = OSError("inert interruption after rename committed")
        publish = inputs.lock.path.replace.side_effect

        def committed_then_interrupted(destination):
            publish(destination)
            # Model a cooperating next invocation acquiring the freed leaf
            # before the previous caller observes its interrupted return.
            inputs.lock.state.present = True
            inputs.lock.state.text = "next operation owns this leaf\n"
            raise primary

        inputs.lock.path.replace.side_effect = committed_then_interrupted
        with self.cleanup_scope(inputs), self.assertRaises(OSError) as raised:
            FIXTURE.verify(inputs.args)
        self.assertIs(primary, raised.exception)
        self.assertTrue(inputs.lock.state.present)
        self.assertEqual("next operation owns this leaf\n", inputs.lock.state.text)
        self.assertTrue(inputs.state["record_present"])
        inputs.lock.path.unlink.assert_not_called()
        inputs.lock.path.replace.assert_called_once_with(inputs.record)
        inputs.record.unlink.assert_called_once_with(missing_ok=True)
        inputs.record.open.assert_not_called()

    def test_pg_exclusive_dangling_fence_is_present_even_when_exists_is_false(self) -> None:
        for name in ("active", "pid"):
            with self.subTest(name=name):
                inputs = self.cleanup_inputs()
                marker = inputs.active.path if name == "active" else inputs.pid
                marker.exists.side_effect = None
                marker.exists.return_value = False
                marker.lstat.side_effect = None
                marker.lstat.return_value = SimpleNamespace(st_mode=0o120777)
                with self.cleanup_scope(inputs), self.assertRaisesRegex(ValueError, "active|running"):
                    FIXTURE.verify(inputs.args)
                self.assertTrue(inputs.lock.state.present)
                self.assertTrue(inputs.state["record_present"])
                inputs.record.unlink.assert_not_called()
                inputs.manifest.read_text.assert_not_called()
                inputs.popen.assert_not_called()
                inputs.run.assert_not_called()

    def test_pg_exclusive_nested_contender_cannot_disturb_admitted_verification(self) -> None:
        inputs = self.cleanup_inputs()
        live_run = inputs.live_run
        nested = []

        def contend(command, *args, **kwargs):
            if command[1] != "stop":
                self.assertTrue(inputs.lock.state.present)
                self.assertTrue(inputs.active.state.present)
                before = (inputs.manifest.read_text.call_count, inputs.record.unlink.call_count,
                          inputs.paths["password.txt"].read_text.call_count, inputs.popen.call_count)
                with self.assertRaises(FileExistsError):
                    FIXTURE.verify(inputs.args)
                after = (inputs.manifest.read_text.call_count, inputs.record.unlink.call_count,
                         inputs.paths["password.txt"].read_text.call_count, inputs.popen.call_count)
                self.assertEqual(before, after)
                inputs.lock.path.unlink.assert_not_called()
                inputs.active.path.unlink.assert_not_called()
                inputs.lock.path.replace.assert_not_called()
                nested.append("rejected")
            return live_run(command, *args, **kwargs)

        inputs.live_run = contend
        with self.cleanup_scope(inputs):
            FIXTURE.verify(inputs.args)
        self.assertEqual(["rejected"], nested)
        inputs.popen.assert_called_once()
        self.assertEqual(2, inputs.run.call_count)
        self.assertEqual(1, inputs.events.count("stop"))
        inputs.record.unlink.assert_called_once_with(missing_ok=True)
        inputs.active.path.unlink.assert_called_once_with()
        inputs.lock.path.replace.assert_called_once_with(inputs.record)
        self.assertFalse(inputs.lock.state.present)
        self.assertFalse(inputs.active.state.present)
        self.assertTrue(inputs.state["record_present"])

    def test_pg_exclusive_unknown_popen_constructor_keeps_fences_without_pid(self) -> None:
        for primary in (OSError("inert interrupted constructor"), KeyboardInterrupt("inert interrupted constructor")):
            with self.subTest(primary=type(primary).__name__):
                inputs = self.cleanup_inputs()
                inputs.pid.exists.side_effect = None
                inputs.pid.exists.return_value = False
                with self.cleanup_scope(inputs):
                    inputs.popen.side_effect = primary
                    with self.assertRaises(FIXTURE.BOUNDS.CleanupFailure) as raised:
                        FIXTURE.verify(inputs.args)
                self.assertIs(primary, raised.exception.primary)
                self.assertIsNone(raised.exception.process)
                self.assertIsNone(raised.exception.job)
                self.assertIs(inputs.args.fixture, raised.exception.fixture_root)
                self.assertTrue(inputs.lock.state.present)
                self.assertTrue(inputs.active.state.present)
                self.assertFalse(inputs.state["record_present"])
                inputs.popen.assert_called_once()
                inputs.run.assert_not_called()
                inputs.tree.assert_not_called()
                inputs.job.terminate.assert_not_called()
                inputs.job.close.assert_not_called()
                inputs.active.path.unlink.assert_not_called()
                inputs.lock.path.unlink.assert_not_called()
                inputs.lock.path.replace.assert_not_called()

    def test_pg_exclusive_startup_log_open_failure_releases_known_prelaunch_fences(self) -> None:
        inputs = self.cleanup_inputs()
        primary = OSError("inert startup log open failure")
        inputs.paths["startup.log"].open.side_effect = primary
        inputs.pid.exists.side_effect = None
        inputs.pid.exists.return_value = False
        with self.cleanup_scope(inputs), self.assertRaises(OSError) as raised:
            FIXTURE.verify(inputs.args)
        self.assertIs(primary, raised.exception)
        inputs.popen.assert_not_called()
        inputs.run.assert_not_called()
        inputs.tree.assert_not_called()
        inputs.active.path.unlink.assert_called_once_with()
        inputs.lock.path.unlink.assert_called_once_with()
        inputs.lock.path.replace.assert_not_called()
        self.assertFalse(inputs.active.state.present)
        self.assertFalse(inputs.lock.state.present)
        self.assertFalse(inputs.state["record_present"])

    @staticmethod
    def file_only_scope() -> ExitStack:
        stack = ExitStack()
        for owner, name in ((FIXTURE.subprocess, "Popen"), (FIXTURE.subprocess, "run"),
                            (FIXTURE.socket, "socket"), (FIXTURE.BOUNDS, "run_bounded")):
            stack.enter_context(patch.object(owner, name, side_effect=AssertionError("file-only contract forbids processes and network")))
        stack.enter_context(patch.object(FIXTURE, "SECRET", ""))
        return stack

    def test_pg_exclusive_real_files_publish_receipt_and_release_in_one_rename(self) -> None:
        with tempfile.TemporaryDirectory(prefix="ku-pg-lease-files-") as raw, self.file_only_scope():
            root = Path(raw)
            lock, record = root / "operation.lock", root / "verification.json"
            FIXTURE.write_marker(record, "previous receipt\n")
            with FIXTURE.operation(root) as owned:
                self.assertTrue(lock.is_file())
                with self.assertRaises(FileExistsError):
                    with FIXTURE.operation(root):
                        self.fail("a nested file lease entered the protected body")
                self.assertEqual("previous receipt\n", record.read_text(encoding="utf-8"))
                owned.receipt = {"live_test_passed": True, "server_stopped": True}
                self.assertTrue(lock.is_file())
            self.assertFalse(lock.exists())
            self.assertEqual({"live_test_passed": True, "server_stopped": True},
                             json.loads(record.read_text(encoding="utf-8")))
            self.assertEqual({"verification.json"}, {entry.name for entry in root.iterdir()})

    def test_pg_exclusive_real_directory_lock_cannot_be_adopted(self) -> None:
        with tempfile.TemporaryDirectory(prefix="ku-pg-lease-directory-") as raw, self.file_only_scope():
            root = Path(raw)
            lock = root / "operation.lock"
            lock.mkdir()
            record = root / "verification.json"
            FIXTURE.write_marker(record, "previous receipt\n")
            with self.assertRaises(OSError):
                with FIXTURE.operation(root):
                    self.fail("a directory is not a newly acquired lease")
            self.assertTrue(lock.is_dir())
            self.assertEqual("previous receipt\n", record.read_text(encoding="utf-8"))
            self.assertEqual({"operation.lock", "verification.json"}, {path.name for path in root.iterdir()})

    def test_pg_exclusive_real_files_distinguish_clean_failure_from_uncertainty(self) -> None:
        for uncertain in (False, True):
            with self.subTest(uncertain=uncertain), tempfile.TemporaryDirectory(prefix="ku-pg-lease-failure-") as raw, self.file_only_scope():
                root = Path(raw)
                lock, active, record = (root / name for name in ("operation.lock", "server.active", "verification.json"))
                primary = RuntimeError("inert file operation failure")
                with self.assertRaises(RuntimeError) as raised:
                    with FIXTURE.operation(root) as owned:
                        owned.uncertain = uncertain
                        if uncertain:
                            FIXTURE.write_marker(active, "unknown cleanup\n")
                        raise primary
                self.assertIs(primary, raised.exception)
                self.assertEqual(uncertain, lock.exists())
                self.assertEqual(uncertain, active.exists())
                self.assertFalse(record.exists())
                if uncertain:
                    original = lock.read_bytes()
                    with self.assertRaises(FileExistsError):
                        with FIXTURE.operation(root):
                            self.fail("uncertain file owner was silently adopted")
                    self.assertEqual(original, lock.read_bytes())

    def test_pg_exclusive_receipt_bound_and_secret_refusal_leave_lease(self) -> None:
        for secret, value, message in (("", "x" * (FIXTURE.MAX_RECEIPT_BYTES + 1), "byte bound"),
                                       ("inert-private-credential", "inert-private-credential", "credential-bearing")):
            with self.subTest(message=message):
                inputs = self.mocked_verification_inputs()
                with patch.object(FIXTURE, "SECRET", secret), self.assertRaisesRegex(ValueError, message) as raised:
                    with FIXTURE.operation(inputs.args.fixture) as owned:
                        owned.receipt = {"value": value}
                self.assertTrue(inputs.lock.state.present)
                self.assertTrue(inputs.state["record_present"])
                self.assertNotIn("inert-private-credential", str(raised.exception))
                inputs.lock.path.open.assert_called_once_with("x", encoding="utf-8", newline="\n")
                inputs.lock.path.replace.assert_not_called()
                inputs.lock.path.unlink.assert_not_called()
                inputs.record.unlink.assert_not_called()

    @staticmethod
    def prepare_inputs() -> argparse.Namespace:
        installed, perl = MagicMock(spec=Path), MagicMock(spec=Path)
        installed.resolve.return_value = Path("inert-installed")
        perl.resolve.return_value = Path("inert-perl")
        return argparse.Namespace(installed_root=installed, perl=perl, source_archive=None)

    def test_pg_prepare_conservative_assembly_failure_retains_lease_and_blocks_verify(self) -> None:
        for phase in ("assembly", "initializer_unknown", "manifest"):
            with self.subTest(phase=phase), tempfile.TemporaryDirectory(prefix="ku-pg-prepare-failure-") as raw, self.file_only_scope():
                target = Path(raw)
                root = target / (FIXTURE.PREFIX + "1" * 32)
                root.mkdir()
                args = self.prepare_inputs()
                primary = RuntimeError("inert " + phase + " failure")
                if phase == "initializer_unknown":
                    primary = FIXTURE.BOUNDS.CleanupFailure("inert initializer owner unconfirmed")
                    primary.job = MagicMock()
                    primary.process = MagicMock()

                def assemble(actual_args, actual_root, installed, perl):
                    self.assertIs(args, actual_args)
                    self.assertEqual(root, actual_root)
                    self.assertEqual(args.installed_root.resolve.return_value, installed)
                    self.assertEqual(args.perl.resolve.return_value, perl)
                    self.assertTrue((root / "operation.lock").is_file())
                    if phase == "manifest":
                        FIXTURE.write_marker(root / "fixture.json", "{inert partial manifest")
                    raise primary

                with patch.object(FIXTURE, "os", SimpleNamespace(name="nt")), \
                     patch.object(FIXTURE, "TARGET", target), \
                     patch.object(FIXTURE, "private_fixture", return_value=root), \
                     patch.object(FIXTURE, "prepare_owned", side_effect=assemble) as assembler, \
                     patch.object(FIXTURE, "run", return_value=b"postgres (PostgreSQL) 17.10\n") as run, \
                     patch("sys.stdout", new_callable=io.StringIO):
                    with self.assertRaises(RuntimeError) as raised:
                        FIXTURE.prepare(args)
                    self.assertIs(primary, raised.exception)
                    if phase == "initializer_unknown":
                        self.assertIs(primary.job, raised.exception.job)
                        self.assertIs(primary.process, raised.exception.process)
                    assembler.assert_called_once()
                    self.assertTrue((root / "operation.lock").is_file())
                    self.assertEqual(phase == "manifest", (root / "fixture.json").exists())
                    self.assertFalse((root / "verification.json").exists())
                    with self.assertRaises(FileExistsError):
                        FIXTURE.verify(argparse.Namespace(fixture=root))
                    run.assert_called_once_with(
                        [str(args.installed_root.resolve.return_value / "bin" / "postgres.exe"), "--version"],
                        FIXTURE.REPO, "check PostgreSQL version")

    def test_pg_prepare_success_releases_lease_only_after_manifest_closes(self) -> None:
        with tempfile.TemporaryDirectory(prefix="ku-pg-prepare-success-") as raw, self.file_only_scope():
            root = Path(raw)
            args = self.prepare_inputs()
            events = []
            manifest = {"format": 1, "version": FIXTURE.VERSION}
            original_unlink = Path.unlink

            def assemble(actual_args, actual_root, installed, perl):
                self.assertIs(args, actual_args)
                self.assertEqual(root, actual_root)
                self.assertTrue((root / "operation.lock").is_file())
                events.append("assembly")
                FIXTURE.write_marker(root / "fixture.json", json.dumps(manifest) + "\n")
                events.append("manifest_closed")

            def release(path, *args, **kwargs):
                self.assertEqual(root / "operation.lock", path)
                self.assertEqual(["assembly", "manifest_closed"], events)
                self.assertEqual(manifest, json.loads((root / "fixture.json").read_text(encoding="utf-8")))
                events.append("lease_release")
                return original_unlink(path, *args, **kwargs)

            with patch.object(FIXTURE, "os", SimpleNamespace(name="nt")), \
                 patch.object(FIXTURE, "private_fixture", return_value=root), \
                 patch.object(FIXTURE, "prepare_owned", side_effect=assemble) as assembler, \
                 patch.object(FIXTURE, "run", return_value=b"postgres (PostgreSQL) 17.10\n") as run, \
                 patch.object(Path, "unlink", autospec=True, side_effect=release), \
                 patch("sys.stdout", new_callable=io.StringIO):
                self.assertEqual(root, FIXTURE.prepare(args))
            assembler.assert_called_once()
            run.assert_called_once()
            self.assertEqual(["assembly", "manifest_closed", "lease_release"], events)
            self.assertFalse((root / "operation.lock").exists())
            self.assertFalse((root / "verification.json").exists())
            self.assertEqual(manifest, json.loads((root / "fixture.json").read_text(encoding="utf-8")))

    def test_pg_prepare_release_failure_retains_complete_manifest_but_blocks_verify(self) -> None:
        with tempfile.TemporaryDirectory(prefix="ku-pg-prepare-release-") as raw, self.file_only_scope():
            target = Path(raw)
            root = target / (FIXTURE.PREFIX + "2" * 32)
            root.mkdir()
            args = self.prepare_inputs()
            manifest = {"format": 1, "version": FIXTURE.VERSION}

            def assemble(*_):
                self.assertTrue((root / "operation.lock").is_file())
                FIXTURE.write_marker(root / "fixture.json", json.dumps(manifest) + "\n")

            with patch.object(FIXTURE, "os", SimpleNamespace(name="nt")), \
                 patch.object(FIXTURE, "TARGET", target), \
                 patch.object(FIXTURE, "private_fixture", return_value=root), \
                 patch.object(FIXTURE, "prepare_owned", side_effect=assemble), \
                 patch.object(FIXTURE, "run", return_value=b"postgres (PostgreSQL) 17.10\n") as run, \
                 patch.object(Path, "unlink", autospec=True, side_effect=OSError("inert prepare lease release failure")), \
                 patch("sys.stdout", new_callable=io.StringIO):
                with self.assertRaisesRegex(FIXTURE.BOUNDS.CleanupFailure, "operation release"):
                    FIXTURE.prepare(args)
                self.assertTrue((root / "operation.lock").is_file())
                self.assertEqual(manifest, json.loads((root / "fixture.json").read_text(encoding="utf-8")))
                with self.assertRaises(FileExistsError):
                    FIXTURE.verify(argparse.Namespace(fixture=root))
                run.assert_called_once()
            self.assertFalse((root / "verification.json").exists())

    def test_pg_private_fixture_uuid_collision_never_overwrites_existing_owner(self) -> None:
        with tempfile.TemporaryDirectory(prefix="ku-pg-uuid-collision-") as raw, self.file_only_scope():
            target = Path(raw)
            token = "3" * 32
            root = target / (FIXTURE.PREFIX + token)
            root.mkdir()
            contents = {"operation.lock": "existing owner\n", "server.active": "unknown server\n",
                        "fixture.json": "existing manifest\n", "verification.json": "existing receipt\n"}
            for name, value in contents.items():
                FIXTURE.write_marker(root / name, value)
            with patch.object(FIXTURE, "TARGET", target), \
                 patch.object(FIXTURE.uuid, "uuid4", return_value=SimpleNamespace(hex=token)) as identity, \
                 patch.object(FIXTURE, "run", side_effect=AssertionError("UUID collision must not reach ACL/process calls")) as run:
                with self.assertRaises(FileExistsError):
                    FIXTURE.private_fixture()
            identity.assert_called_once_with()
            run.assert_not_called()
            self.assertEqual(set(contents), {entry.name for entry in root.iterdir()})
            for name, value in contents.items():
                self.assertEqual(value, (root / name).read_text(encoding="utf-8"))

    def test_pg_exclusive_contender_does_not_mutate_or_start(self) -> None:
        inputs = self.cleanup_inputs()
        inputs.lock.state.present = True
        with self.cleanup_scope(inputs), self.assertRaises(FileExistsError):
            FIXTURE.verify(inputs.args)
        self.assertTrue(inputs.lock.state.present)
        self.assertTrue(inputs.state["record_present"])
        inputs.record.unlink.assert_not_called()
        inputs.manifest.read_text.assert_not_called()
        inputs.paths["password.txt"].read_text.assert_not_called()
        inputs.paths["startup.log"].open.assert_not_called()
        inputs.run.assert_not_called()
        inputs.tree.assert_not_called()

    def test_pg_exclusive_active_without_pid_refuses_before_receipt_invalidation(self) -> None:
        inputs = self.cleanup_inputs()
        inputs.active.state.present = True
        with self.cleanup_scope(inputs), self.assertRaisesRegex(ValueError, "active|running"):
            FIXTURE.verify(inputs.args)
        self.assertTrue(inputs.active.state.present)
        self.assertTrue(inputs.state["record_present"])
        inputs.record.unlink.assert_not_called()
        inputs.manifest.read_text.assert_not_called()
        inputs.run.assert_not_called()
        inputs.tree.assert_not_called()

    def test_pg_exclusive_failed_start_with_late_pid_never_authorizes_stop(self) -> None:
        for startup in (1, subprocess.TimeoutExpired("inert startup", 25)):
            with self.subTest(startup=type(startup).__name__):
                inputs = self.cleanup_inputs()
                inputs.pid.exists.side_effect = [False, True, True]
                inputs.process.wait.side_effect = [startup, 0]
                with self.cleanup_scope(inputs), self.assertRaises((RuntimeError, subprocess.TimeoutExpired)):
                    FIXTURE.verify(inputs.args)
                inputs.run.assert_not_called()
                inputs.tree.assert_called_once_with(inputs.process, inputs.job)
                inputs.job.wait_empty.assert_called_once()
                inputs.job.close.assert_called_once_with()
                inputs.record.open.assert_not_called()

    def test_pg_exclusive_cleanup_uncertainty_fences_rerun_without_pid(self) -> None:
        for failure in ("job_drain", "job_close", "root_wait"):
            with self.subTest(failure=failure):
                inputs = self.cleanup_inputs(failure)
                inputs.pid.exists.side_effect = None
                inputs.pid.exists.return_value = False
                with self.cleanup_scope(inputs), self.assertRaises(RuntimeError):
                    FIXTURE.verify(inputs.args)
                self.assertTrue(inputs.lock.state.present)
                self.assertTrue(inputs.active.state.present)
                inputs.record.open.assert_not_called()
                with self.cleanup_scope(inputs), self.assertRaises(FileExistsError):
                    FIXTURE.verify(inputs.args)
                inputs.tree.assert_not_called()
                inputs.run.assert_not_called()

    def test_pg_cleanup_failures_attempt_independent_steps_and_forbid_receipt(self) -> None:
        for failure in ("stop_pid", "stop", "job_terminate", "job_drain", "job_close", "root_poll", "root_kill", "root_wait", "final_pid"):
            with self.subTest(failure=failure):
                inputs = self.cleanup_inputs(failure)
                with self.cleanup_scope(inputs), self.assertRaises((OSError, RuntimeError)):
                    FIXTURE.verify(inputs.args)
                for step in ("job_terminate", "job_drain", "job_close", "root_kill", "root_wait", "final_pid"):
                    self.assertIn(step, inputs.events, f"{failure} skipped independent {step}")
                self.assertEqual(1, inputs.events.count("job_close"))
                self.assertEqual(1, inputs.events.count("root_wait"))
                self.assertEqual(1, inputs.events.count("final_pid"))
                inputs.numeric.assert_not_called()
                inputs.record.open.assert_not_called()

                self.assertTrue(inputs.lock.state.present)
                self.assertTrue(inputs.active.state.present)
                inputs.lock.path.replace.assert_not_called()

    def test_pg_cleanup_failure_preserves_original_live_error(self) -> None:
        for failure in ("stop", "job_terminate", "job_drain", "job_close", "root_wait", "final_pid"):
            with self.subTest(failure=failure):
                inputs = self.cleanup_inputs(failure, live_failure=True)
                with self.cleanup_scope(inputs), self.assertRaisesRegex(RuntimeError, "original live failure"):
                    FIXTURE.verify(inputs.args)
                inputs.record.open.assert_not_called()
                inputs.numeric.assert_not_called()

    def test_pg_cleanup_mock_success_receipt_follows_all_checks(self) -> None:
        inputs = self.cleanup_inputs()
        expected = [
            "initial_pid", "startup_wait", "live", "stop_pid", "stop", "job_terminate",
            "root_poll", "root_kill", "job_drain", "job_close", "root_wait", "final_pid",
        ]
        publish = inputs.lock.path.replace.side_effect

        def publish_after_cleanup(destination):
            self.assertEqual(expected, inputs.events)
            inputs.active.path.unlink.assert_called_once_with()
            self.assertFalse(inputs.active.state.present)
            return publish(destination)

        inputs.lock.path.replace.side_effect = publish_after_cleanup
        with self.cleanup_scope(inputs):
            FIXTURE.verify(inputs.args)
        self.assertEqual(expected, inputs.events)
        self.assertEqual([
            call("x", encoding="utf-8", newline="\n"),
            call("w", encoding="utf-8", newline="\n"),
        ], inputs.lock.path.open.call_args_list)
        inputs.lock.path.replace.assert_called_once_with(inputs.record)
        inputs.lock.path.unlink.assert_not_called()
        inputs.record.open.assert_not_called()
        self.assertFalse(inputs.lock.state.present)
        self.assertTrue(inputs.state["record_present"])
        receipt = json.loads(inputs.state["receipt_text"])
        self.assertIs(True, receipt["live_test_passed"])
        self.assertIs(True, receipt["server_stopped"])
        self.assertEqual(FIXTURE.SOURCE_SHA256, receipt["source_sha256"])
        self.assertNotIn("inert-contract-password", inputs.state["receipt_text"])
        inputs.numeric.assert_not_called()

    def test_pg_cleanup_held_root_wait_uses_remaining_five_second_budget(self) -> None:
        inputs = self.cleanup_inputs()
        with self.cleanup_scope(inputs), patch.object(FIXTURE.time, "monotonic", side_effect=[10.0, 13.0]):
            FIXTURE.verify(inputs.args)
        self.assertEqual(2.0, inputs.process.wait.call_args.kwargs["timeout"])

    def test_pg_cleanup_expired_fallback_budget_does_not_reset_wait_timeout(self) -> None:
        inputs = self.cleanup_inputs()
        with self.cleanup_scope(inputs), patch.object(FIXTURE.time, "monotonic", side_effect=[10.0, 16.0]):
            FIXTURE.verify(inputs.args)
        self.assertEqual(0.0, inputs.process.wait.call_args.kwargs["timeout"])

    def test_pg_clean_cleanup_preserves_original_failure_identity(self) -> None:
        inputs = self.cleanup_inputs()
        primary = RuntimeError("original live failure")
        with self.cleanup_scope(inputs):
            inputs.run.side_effect = [primary, b""]
            with self.assertRaises(RuntimeError) as raised:
                FIXTURE.verify(inputs.args)
        self.assertIs(primary, raised.exception)
        inputs.record.open.assert_not_called()

    def test_pg_first_stop_failure_remains_primary_after_independent_cleanup(self) -> None:
        for close_failure in (False, True):
            with self.subTest(close_failure=close_failure):
                inputs = self.cleanup_inputs()
                primary = OSError("original stop failure")
                if close_failure:
                    inputs.job.close.side_effect = OSError("inert close failure")
                with self.cleanup_scope(inputs):
                    inputs.run.side_effect = [
                        b"test result: ok. 1 passed; 0 failed; 0 ignored;\n", primary,
                    ]
                    with self.assertRaisesRegex((OSError, RuntimeError), "original stop failure") as raised:
                        FIXTURE.verify(inputs.args)
                self.assertIs(primary, raised.exception.primary if close_failure else raised.exception)
                inputs.job.terminate.assert_called_once_with()
                inputs.job.close.assert_called_once_with()
                self.assertIn("root_wait", inputs.events)
                self.assertIn("final_pid", inputs.events)
                inputs.record.open.assert_not_called()

    def test_pg_multiple_cleanup_failures_preserve_primary_and_all_attempts(self) -> None:
        inputs = self.cleanup_inputs(live_failure=True)
        for operation in (inputs.job.terminate, inputs.job.close, inputs.process.kill):
            operation.side_effect = OSError("inert cleanup failure")
        with self.cleanup_scope(inputs), self.assertRaisesRegex(RuntimeError, "original live failure") as raised:
            FIXTURE.verify(inputs.args)
        for operation in (inputs.job.terminate, inputs.job.close, inputs.process.kill):
            operation.assert_called_once_with()
        self.assertIn("root_wait", inputs.events)
        self.assertIn("final_pid", inputs.events)
        self.assertIn("PostgreSQL process termination", str(raised.exception))
        self.assertIn("PostgreSQL Job close", str(raised.exception))
        inputs.record.open.assert_not_called()

    def test_pg_failed_setup_retains_actual_job_owner_without_resuming(self) -> None:
        for failure in (None, "terminate", "wait_empty", "close"):
            with self.subTest(failure=failure):
                inputs = self.startup_inputs()
                process = MagicMock(spec=["poll", "kill", "wait"])
                process.poll.return_value = None
                process.wait.return_value = 0
                job = MagicMock(spec=["terminate", "wait_empty", "close"])
                job.wait_empty.return_value = True
                if failure is not None:
                    getattr(job, failure).side_effect = OSError("inert retained owner failure")
                primary = FIXTURE.BOUNDS.WindowsJobSetupError(job, OSError("inert assignment"))
                with self.startup_scope(inputs), \
                     patch.object(FIXTURE.subprocess, "Popen", return_value=process), \
                     patch.object(FIXTURE.BOUNDS.WindowsJob, "attach", side_effect=primary), \
                     patch.object(FIXTURE.BOUNDS, "resume_suspended_windows_process") as resume, \
                     self.assertRaisesRegex(RuntimeError, "Windows Job setup failed"):
                    FIXTURE.verify(inputs.args)
                resume.assert_not_called()
                inputs.run.assert_not_called()
                inputs.tree.assert_called_once_with(process, job)
                job.terminate.assert_called_once_with()
                job.wait_empty.assert_called_once()
                job.close.assert_called_once_with()
                process.kill.assert_called_once_with()
                self.assertEqual(1, process.wait.call_count)
                self.assertGreater(process.wait.call_args.kwargs["timeout"], 0)
                self.assertLessEqual(process.wait.call_args.kwargs["timeout"], 5)
                inputs.numeric.assert_not_called()
                inputs.record.open.assert_not_called()

    def test_pg_retained_pid_forbids_receipt_after_other_checks_pass(self) -> None:
        inputs = self.cleanup_inputs()
        inputs.pid.exists.side_effect = [False, True, True]
        with self.cleanup_scope(inputs), self.assertRaisesRegex(RuntimeError, "shutdown was not confirmed"):
            FIXTURE.verify(inputs.args)
        self.assertIn("root_wait", inputs.events)
        inputs.record.open.assert_not_called()

    def test_pg_job_drain_unknown_result_forbids_receipt_and_preserves_cleanup(self) -> None:
        for result in (None, False, 0, 1, "true", MagicMock()):
            with self.subTest(result=repr(result)):
                inputs = self.cleanup_inputs()
                inputs.job.wait_empty.side_effect = None
                inputs.job.wait_empty.return_value = result
                with self.cleanup_scope(inputs), self.assertRaisesRegex(RuntimeError, "Job drain"):
                    FIXTURE.verify(inputs.args)
                inputs.job.wait_empty.assert_called_once()
                inputs.job.close.assert_called_once_with()
                self.assertIn("root_wait", inputs.events)
                self.assertIn("final_pid", inputs.events)
                inputs.record.open.assert_not_called()
                inputs.numeric.assert_not_called()

    def test_pg_job_drain_errors_forbid_receipt_even_after_close(self) -> None:
        for failure in (OSError("inert query failure"), TimeoutError("inert drain deadline")):
            with self.subTest(failure=type(failure).__name__):
                inputs = self.cleanup_inputs()
                inputs.job.wait_empty.side_effect = failure
                with self.cleanup_scope(inputs), self.assertRaisesRegex(RuntimeError, "Job drain") as raised:
                    FIXTURE.verify(inputs.args)
                self.assertIn(type(failure).__name__, str(raised.exception))
                inputs.job.wait_empty.assert_called_once()
                inputs.job.close.assert_called_once_with()
                self.assertIn("root_wait", inputs.events)
                self.assertIn("final_pid", inputs.events)
                inputs.record.open.assert_not_called()
                inputs.numeric.assert_not_called()

    def test_pg_job_drain_true_does_not_erase_termination_failure(self) -> None:
        inputs = self.cleanup_inputs("job_terminate")
        with self.cleanup_scope(inputs), self.assertRaisesRegex(RuntimeError, "process termination"):
            FIXTURE.verify(inputs.args)
        inputs.job.wait_empty.assert_called_once()
        inputs.job.close.assert_called_once_with()
        self.assertIn("root_wait", inputs.events)
        self.assertIn("final_pid", inputs.events)
        inputs.record.open.assert_not_called()

    def test_pg_job_drain_failure_preserves_first_live_or_stop_error(self) -> None:
        for stage in ("live", "stop"):
            with self.subTest(stage=stage):
                inputs = self.cleanup_inputs()
                primary = RuntimeError("original " + stage + " failure")
                inputs.job.wait_empty.side_effect = OSError("inert query failure")
                with self.cleanup_scope(inputs):
                    inputs.run.side_effect = [primary, b""] if stage == "live" else [
                        b"test result: ok. 1 passed; 0 failed; 0 ignored;\n", primary,
                    ]
                    with self.assertRaisesRegex(RuntimeError, "original " + stage) as raised:
                        FIXTURE.verify(inputs.args)
                self.assertIsInstance(raised.exception, FIXTURE.BOUNDS.CleanupFailure)
                self.assertIs(primary, raised.exception.primary)
                self.assertIn("Job drain", str(raised.exception))
                inputs.job.close.assert_called_once_with()
                self.assertIn("root_wait", inputs.events)
                self.assertIn("final_pid", inputs.events)
                inputs.record.open.assert_not_called()

    def test_pg_job_drain_uses_existing_deadline_and_precedes_close(self) -> None:
        inputs = self.cleanup_inputs()
        with self.cleanup_scope(inputs), patch.object(FIXTURE.time, "monotonic", side_effect=[10.0, 13.0]):
            FIXTURE.verify(inputs.args)
        inputs.job.wait_empty.assert_called_once_with(15.0)
        self.assertLess(inputs.events.index("root_kill"), inputs.events.index("job_drain"))
        self.assertLess(inputs.events.index("job_drain"), inputs.events.index("job_close"))
        self.assertEqual(2.0, inputs.process.wait.call_args.kwargs["timeout"])
        self.assertEqual(25, inputs.run.call_args.args[3])
        inputs.record.open.assert_not_called()
        inputs.lock.path.replace.assert_called_once_with(inputs.record)
        inputs.lock.path.unlink.assert_not_called()
        self.assertFalse(inputs.lock.state.present)
        self.assertTrue(inputs.state["record_present"])

    @unittest.skipUnless(os.name == "nt", "real harmless Windows process / Job contract")
    def test_pg_real_suspended_root_job_order_and_failed_assignment(self) -> None:
        # No DB, compiler, socket, PID lookup or descendant is used here. The
        # production call site supplies flags; only its executable is replaced.
        real_popen = subprocess.Popen
        real_attach = FIXTURE.BOUNDS.WindowsJob.attach
        real_resume = FIXTURE.BOUNDS.resume_suspended_windows_process
        for reject in (False, True):
            with self.subTest(reject=reject), tempfile.TemporaryDirectory(prefix="ku-pg-held-root-") as raw:
                directory = Path(raw)
                marker = directory / "executed"
                inputs = self.startup_inputs()
                held = []
                flags = []
                jobs = []
                child = "from pathlib import Path; Path(" + repr(str(marker)) + ").write_text('inert', encoding='ascii')"

                def launch(*args, **kwargs):
                    flags.append(kwargs["creationflags"])
                    process = real_popen([sys.executable, "-B", "-c", child], cwd=directory,
                                         stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL,
                                         stderr=subprocess.DEVNULL, creationflags=flags[-1])
                    held.append(process)
                    return process

                def attach(process):
                    if not flags[-1] & 4:
                        # Old unsuspended call site: force its harmless action
                        # before assignment, retaining its exact process handle.
                        process.wait(timeout=5)
                    self.assertFalse(marker.exists(), "child executed before Job assignment")
                    if reject:
                        import ctypes
                        from ctypes import wintypes
                        kernel = ctypes.WinDLL("kernel32", use_last_error=True)
                        kernel.AssignProcessToJobObject.argtypes = [wintypes.HANDLE, wintypes.HANDLE]
                        kernel.AssignProcessToJobObject.restype = wintypes.BOOL
                        # A real Win32 failed assignment, not a fake process or
                        # mocked return from AssignProcessToJobObject. Inject
                        # invalid Job authority; retain the real child handle.
                        self.assertFalse(kernel.AssignProcessToJobObject(
                            wintypes.HANDLE(0), wintypes.HANDLE(process._handle)))
                        return None
                    job = real_attach(process)
                    self.assertIsNotNone(job, "host cannot establish required Job containment")
                    jobs.append(job)
                    return job

                try:
                    with self.startup_scope(inputs), \
                         patch.object(FIXTURE.subprocess, "Popen", side_effect=launch), \
                         patch.object(FIXTURE.BOUNDS.WindowsJob, "attach", side_effect=attach), \
                         patch.object(FIXTURE.BOUNDS, "resume_suspended_windows_process", wraps=real_resume) as resume, \
                         self.assertRaisesRegex(RuntimeError, "contain" if reject else "inert live boundary"):
                        FIXTURE.verify(inputs.args)
                    self.assertEqual(1, len(held))
                    self.assertTrue(flags[0] & 4)
                    self.assertIsNotNone(held[0].poll(), "retained root handle has not signalled exit")
                    self.assertEqual(not reject, marker.exists())
                    self.assertEqual(0 if reject else 1, resume.call_count)
                    if reject:
                        inputs.run.assert_not_called()
                        inputs.tree.assert_called_once_with(held[0], None)
                    else:
                        inputs.tree.assert_called_once_with(held[0], jobs[0])
                    inputs.numeric.assert_not_called()
                    inputs.record.open.assert_not_called()
                finally:
                    # Even a red candidate never authorizes taskkill/PID discovery.
                    # There is no descendant to escape this independent handle cleanup.
                    for job in jobs:
                        try:
                            job.terminate()
                        finally:
                            job.close()
                    for process in held:
                        if process.poll() is None:
                            process.kill()
                        process.wait(timeout=5)

    def test_failed_rerun_invalidates_previous_success_before_validation(self) -> None:
        inputs = self.mocked_verification_inputs()
        with patch.object(FIXTURE, "os", SimpleNamespace(name="nt")), patch.object(FIXTURE, "TARGET", inputs.target), patch.object(FIXTURE.subprocess, "Popen") as process:
            with self.assertRaises(ValueError):
                FIXTURE.verify(inputs.args)
        self.assertFalse(inputs.state["record_present"])
        inputs.record.unlink.assert_called_once_with(missing_ok=True)
        inputs.record.resolve.assert_not_called()
        inputs.manifest.read_text.assert_called_once_with(encoding="utf-8")
        process.assert_not_called()

    def test_verification_without_previous_record_reaches_validation(self) -> None:
        inputs = self.mocked_verification_inputs(record_present=False)
        with patch.object(FIXTURE, "os", SimpleNamespace(name="nt")), patch.object(FIXTURE, "TARGET", inputs.target), patch.object(FIXTURE.subprocess, "Popen") as process:
            with self.assertRaises(ValueError):
                FIXTURE.verify(inputs.args)
        inputs.record.unlink.assert_called_once_with(missing_ok=True)
        inputs.record.resolve.assert_not_called()
        inputs.manifest.read_text.assert_called_once_with(encoding="utf-8")
        process.assert_not_called()

    def test_verification_invalid_root_does_not_delete_records(self) -> None:
        for wrong_parent in (False, True):
            inputs = self.mocked_verification_inputs()
            if wrong_parent:
                inputs.args.fixture.parent = object()
            else:
                inputs.args.fixture.name = "not-a-pg-fixture"
            with self.subTest(wrong_parent=wrong_parent), patch.object(FIXTURE, "os", SimpleNamespace(name="nt")), patch.object(FIXTURE, "TARGET", inputs.target), patch.object(FIXTURE.subprocess, "Popen") as process:
                with self.assertRaisesRegex(ValueError, "dedicated fixture"):
                    FIXTURE.verify(inputs.args)
            self.assertTrue(inputs.state["record_present"])
            inputs.record.unlink.assert_not_called()
            inputs.record.resolve.assert_not_called()
            inputs.manifest.read_text.assert_not_called()
            process.assert_not_called()

    def test_verification_unlink_failure_prevents_startup(self) -> None:
        for error in (PermissionError("record is not writable"), IsADirectoryError("record is a directory")):
            inputs = self.mocked_verification_inputs()
            inputs.record.unlink.side_effect = error
            with self.subTest(error=type(error).__name__), patch.object(FIXTURE, "os", SimpleNamespace(name="nt")), patch.object(FIXTURE, "TARGET", inputs.target), patch.object(FIXTURE.subprocess, "Popen") as process:
                with self.assertRaises(type(error)):
                    FIXTURE.verify(inputs.args)
            self.assertTrue(inputs.state["record_present"])
            inputs.record.unlink.assert_called_once_with(missing_ok=True)
            inputs.record.resolve.assert_not_called()
            inputs.manifest.read_text.assert_not_called()
            process.assert_not_called()

    def test_bounded_runner_includes_stderr_to_detect_skips(self) -> None:
        completed = subprocess.CompletedProcess([], 0, b"1 passed\n", b"skip: no compiler\n")
        original = FIXTURE.BOUNDS.COMMAND_TIMEOUT_SECONDS
        with patch.object(FIXTURE.BOUNDS, "run_bounded", return_value=completed):
            combined = FIXTURE.run([], FIXTURE.REPO, "fake", 2, include_stderr=True)
            with self.assertRaisesRegex(RuntimeError, "skipped"):
                FIXTURE.require_live_pass(combined)
        self.assertEqual(original, FIXTURE.BOUNDS.COMMAND_TIMEOUT_SECONDS)

    def test_bounded_runner_redacts_secret_and_restores_timeout(self) -> None:
        original = FIXTURE.BOUNDS.COMMAND_TIMEOUT_SECONDS
        for error_type in (SystemExit, RuntimeError, OSError):
            with self.subTest(error_type=error_type), patch.object(FIXTURE, "SECRET", "private-credential"), \
                 patch.object(FIXTURE.BOUNDS, "run_bounded", side_effect=error_type("private-credential")):
                with self.assertRaisesRegex(RuntimeError, "^<redacted>$"):
                    FIXTURE.run([], FIXTURE.REPO, "fake", 2)
            self.assertEqual(original, FIXTURE.BOUNDS.COMMAND_TIMEOUT_SECONDS)

    def test_bounded_runner_redacts_primary_in_combined_cleanup_failure(self) -> None:
        secret = "private-credential-" + "a" * 2048
        primary = RuntimeError("command failed: " + secret)
        failure = FIXTURE.BOUNDS.CleanupFailure("Job close (OSError)", primary)
        original = FIXTURE.BOUNDS.COMMAND_TIMEOUT_SECONDS
        with patch.object(FIXTURE, "SECRET", secret), \
             patch.object(FIXTURE.BOUNDS, "run_bounded", side_effect=failure):
            with self.assertRaisesRegex(RuntimeError, "command failed: <redacted>; cleanup failed") as raised:
                FIXTURE.run([], FIXTURE.REPO, "fake", 2)
        self.assertNotIn("private-credential", str(raised.exception))
        self.assertNotIn("a" * 32, str(raised.exception))
        self.assertEqual(original, FIXTURE.BOUNDS.COMMAND_TIMEOUT_SECONDS)

    def test_live_requires_exactly_one_executed_test(self) -> None:
        success = b"test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured\n"
        self.assertIn("1 passed", FIXTURE.require_live_pass(success))
        for output in (b"0 passed; 0 failed", success + b"SKIP: non-loopback", success.replace(b"1 passed", b"11 passed"), success.replace(b"0 ignored", b"1 ignored"), b""):
            with self.subTest(output=output), self.assertRaises(RuntimeError):
                FIXTURE.require_live_pass(output)

    def test_successful_output_also_redacts_credentials(self) -> None:
        with patch.object(FIXTURE, "SECRET", "private-credential"):
            output = b"test result: ok. 1 passed; 0 failed; 0 ignored; private-credential"
            self.assertNotIn("private-credential", FIXTURE.require_live_pass(output))
            self.assertIn("<redacted>", FIXTURE.require_live_pass(output))

    def test_archive_digest_is_streamed_and_bounded(self) -> None:
        source = MagicMock()
        source.open.return_value = io.BytesIO(b"abc")
        self.assertEqual(hashlib.sha256(b"abc").hexdigest(), FIXTURE.digest(source))
        source.open.return_value = io.BytesIO(b"abcd")
        with patch.object(FIXTURE, "MAX_ARCHIVE_BYTES", 3), self.assertRaisesRegex(ValueError, "bounds"):
            FIXTURE.digest(source)

    def test_source_archive_must_match_official_checksum(self) -> None:
        source = MagicMock()
        source.resolve.return_value = source
        source.is_file.return_value = True
        source.stat.return_value = SimpleNamespace(st_size=12)
        with patch.object(FIXTURE, "digest", return_value="0" * 64):
            with self.assertRaisesRegex(ValueError, "SHA-256 mismatch"):
                FIXTURE.validate_archive(source)
        with patch.object(FIXTURE, "digest", return_value=FIXTURE.SOURCE_SHA256):
            self.assertIs(source, FIXTURE.validate_archive(source))
        source.stat.return_value = SimpleNamespace(st_size=FIXTURE.MAX_ARCHIVE_BYTES + 1)
        with patch.object(FIXTURE, "digest") as digest, self.assertRaises(ValueError):
            FIXTURE.validate_archive(source)
        digest.assert_not_called()

    def test_selected_source_scope_excludes_unneeded_files(self) -> None:
        for name in ("src/include/catalog/pg_proc.h", "src/backend/catalog/genbki.pl", "src/pl/plpgsql/src/plpgsql.control"):
            self.assertTrue(FIXTURE.selected_source(name), name)
        for name in ("doc/src/sgml/index.sgml", "src/test/regress/sql/test.sql", "configure", "contrib/README"):
            self.assertFalse(FIXTURE.selected_source(name), name)

    def test_archive_rejects_traversal_links_negative_size_and_oversized_entry(self) -> None:
        traversal = tarfile.TarInfo("postgresql-17.10/src/include/../../escape")
        symlink = tarfile.TarInfo("postgresql-17.10/src/include/link")
        symlink.type = tarfile.SYMTYPE
        negative = tarfile.TarInfo("postgresql-17.10/src/include/bad")
        negative.size = -1
        oversized = tarfile.TarInfo("postgresql-17.10/src/include/big")
        oversized.size = 4 * 1024 * 1024 + 1
        for entry in (traversal, symlink, negative, oversized):
            source = MagicMock()
            source.__enter__.return_value = [entry]
            destination = MagicMock()
            with self.subTest(name=entry.name), patch.object(FIXTURE.tarfile, "open", return_value=source):
                with self.assertRaises(ValueError):
                    FIXTURE.extract_source(Path("unused"), destination)
            destination.joinpath.assert_not_called()

    def test_pg_environment_cannot_redirect_fixture_connections(self) -> None:
        original = {"PATH": "original", "PGHOST": "remote", "PGSERVICE": "production", "PGPASSFILE": "unknown", "KU_BIN": "ku"}
        with patch.dict(os.environ, original, clear=True):
            FIXTURE.fixture_environment(Path("portable"))
            self.assertFalse(any(key.upper().startswith("PG") for key in os.environ))
            self.assertEqual("ku", os.environ["KU_BIN"])
            self.assertEqual("GMT", os.environ["TZ"])
            self.assertTrue(os.environ["PATH"].startswith(str(Path("portable/bin"))))

    @unittest.skipUnless(os.name == "nt", "Windows fixture")
    def test_verify_refuses_a_non_fixture_before_starting_processes(self) -> None:
        args = argparse.Namespace(fixture=FIXTURE.REPO)
        with patch.object(FIXTURE.subprocess, "Popen") as process:
            with self.assertRaisesRegex(ValueError, "dedicated fixture"):
                FIXTURE.verify(args)
            process.assert_not_called()


if __name__ == "__main__":
    unittest.main()
