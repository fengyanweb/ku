#!/usr/bin/env python3
"""Regression tests for the three-OS verifier's bounded child-process harness."""

from __future__ import annotations

from contextlib import ExitStack, contextmanager, redirect_stderr
import importlib.util
import io
import os
from pathlib import Path
import socket
import subprocess
import sys
import tempfile
import threading
import time
from types import SimpleNamespace
import unittest
from unittest import mock


sys.dont_write_bytecode = True
SCRIPT = Path(__file__).with_name("verify-native-three-os.py")
SPEC = importlib.util.spec_from_file_location("ku_native_verifier", SCRIPT)
assert SPEC is not None and SPEC.loader is not None
VERIFIER = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(VERIFIER)


class BoundedProcessTests(unittest.TestCase):
    @contextmanager
    def inert_toolhelp(self):
        # Integer values below are fake handles/PIDs. Only this mocked kernel
        # sees them; no process, OS thread, snapshot or socket is created.
        kernel = mock.MagicMock()
        process = SimpleNamespace(pid=301)
        state = SimpleNamespace(now=100.0, last_error=0)
        events = []
        snapshot_handle, thread_handle, thread_id = 73, 83, 201

        def snapshot(flags, pid):
            self.assertEqual((4, 0), (flags, pid))
            events.append("snapshot")
            return snapshot_handle

        def first(handle, entry_pointer):
            self.assertEqual(snapshot_handle, handle)
            entry = entry_pointer._obj
            self.assertEqual(VERIFIER.ctypes.sizeof(entry), entry.size)
            entry.thread_id = thread_id
            entry.owner_process_id = process.pid
            events.append("first")
            return 1

        def next_entry(handle, entry_pointer):
            self.assertEqual(snapshot_handle, handle)
            events.append("next")
            state.last_error = 18  # Explicit ERROR_NO_MORE_FILES.
            return 0

        def open_thread(access, inherit, identity):
            self.assertEqual((0x0802, False, thread_id), (access, inherit, identity))
            events.append("open")
            return thread_handle

        def owner(handle):
            self.assertEqual(thread_handle, handle)
            events.append("owner")
            return process.pid

        def resume(handle):
            self.assertEqual(thread_handle, handle)
            events.append("resume")
            return 1

        def close(handle):
            self.assertIn(handle, (thread_handle, snapshot_handle))
            events.append("close_thread" if handle == thread_handle else "close_snapshot")
            state.last_error = 0
            return 1

        kernel.CreateToolhelp32Snapshot.side_effect = snapshot
        kernel.Thread32First.side_effect = first
        kernel.Thread32Next.side_effect = next_entry
        kernel.OpenThread.side_effect = open_thread
        kernel.GetProcessIdOfThread.side_effect = owner
        kernel.ResumeThread.side_effect = resume
        kernel.CloseHandle.side_effect = close
        with ExitStack() as stack:
            dll = stack.enter_context(mock.patch.object(VERIFIER.ctypes, "WinDLL", return_value=kernel))
            stack.enter_context(mock.patch.object(VERIFIER.ctypes, "get_last_error", side_effect=lambda: state.last_error))
            stack.enter_context(mock.patch.object(VERIFIER.ctypes, "set_last_error",
                                                 side_effect=lambda value: setattr(state, "last_error", value)))
            clock = stack.enter_context(mock.patch.object(VERIFIER.time, "monotonic", side_effect=lambda: state.now))
            forbidden = []
            for module, name in ((VERIFIER.subprocess, "Popen"), (VERIFIER.subprocess, "run"),
                                 (VERIFIER.threading, "Thread"), (socket, "socket")):
                forbidden.append(stack.enter_context(mock.patch.object(
                    module, name, side_effect=AssertionError("ToolHelp mock contract forbids real process/thread/network work"))))
            try:
                yield SimpleNamespace(kernel=kernel, process=process, state=state, events=events,
                                      snapshot=snapshot_handle, thread=thread_handle, close=close,
                                      resume=resume, clock=clock)
            finally:
                dll.assert_called_once_with("kernel32", use_last_error=True)
                for action in forbidden:
                    action.assert_not_called()

    @unittest.skipUnless(os.name == "nt", "Windows ToolHelp structure with pure mocked kernel")
    def test_toolhelp_close_false_retains_only_unconfirmed_slots(self) -> None:
        for failed in ({"thread"}, {"snapshot"}, {"thread", "snapshot"}):
            with self.subTest(failed=sorted(failed)), self.inert_toolhelp() as run:
                def close(handle):
                    run.close(handle)
                    slot = "thread" if handle == run.thread else "snapshot"
                    if slot in failed:
                        run.state.last_error = 5
                        return 0
                    return 1

                run.kernel.CloseHandle.side_effect = close
                with self.assertRaises((OSError, RuntimeError)) as caught:
                    VERIFIER.resume_suspended_windows_process(run.process)
                owner = caught.exception.toolhelp_handles
                self.assertEqual(run.thread if "thread" in failed else None, owner.thread)
                self.assertEqual(run.snapshot if "snapshot" in failed else None, owner.snapshot)
                self.assertEqual([mock.call(run.thread), mock.call(run.snapshot)], run.kernel.CloseHandle.call_args_list)
                run.kernel.ResumeThread.assert_called_once_with(run.thread)
                self.assertEqual(["snapshot", "first", "next", "open", "owner", "resume",
                                  "close_thread", "close_snapshot"], run.events)

    @unittest.skipUnless(os.name == "nt", "Windows ToolHelp structure with pure mocked kernel")
    def test_toolhelp_thread_close_exception_still_closes_snapshot_and_preserves_primary(self) -> None:
        for body_fails, snapshot_fails in ((False, False), (True, False), (True, True)):
            with self.subTest(body_fails=body_fails, snapshot_fails=snapshot_fails), self.inert_toolhelp() as run:
                close_error = OSError("inert first thread close failure")
                body_error = RuntimeError("inert first resume failure")
                snapshot_error = OSError("inert later snapshot close failure")

                def close(handle):
                    run.close(handle)
                    if handle == run.thread:
                        raise close_error
                    if snapshot_fails:
                        raise snapshot_error
                    return 1

                def resume(handle):
                    run.resume(handle)
                    raise body_error

                run.kernel.CloseHandle.side_effect = close
                if body_fails:
                    run.kernel.ResumeThread.side_effect = resume
                with self.assertRaises((OSError, RuntimeError)) as caught:
                    VERIFIER.resume_suspended_windows_process(run.process)
                primary = body_error if body_fails else close_error
                if isinstance(caught.exception, VERIFIER.CleanupFailure):
                    self.assertIs(primary, caught.exception.primary)
                    self.assertIs(primary, caught.exception.__cause__)
                else:
                    self.assertIs(primary, caught.exception)
                self.assertEqual([mock.call(run.thread), mock.call(run.snapshot)], run.kernel.CloseHandle.call_args_list)
                self.assertEqual(run.thread, caught.exception.toolhelp_handles.thread)
                self.assertEqual(run.snapshot if snapshot_fails else None, caught.exception.toolhelp_handles.snapshot)
                run.kernel.ResumeThread.assert_called_once_with(run.thread)

    @unittest.skipUnless(os.name == "nt", "Windows ToolHelp structure with pure mocked kernel")
    def test_toolhelp_clock_exception_still_closes_independent_owned_handles(self) -> None:
        for stage in ("snapshot", "close_thread"):
            with self.subTest(stage=stage), self.inert_toolhelp() as run:
                primary = OSError("inert first " + stage + " clock failure")
                interrupted = False

                def clock():
                    nonlocal interrupted
                    if run.events and run.events[-1] == stage and not interrupted:
                        interrupted = True
                        raise primary
                    return run.state.now

                run.clock.side_effect = clock
                with self.assertRaises(OSError) as caught:
                    VERIFIER.resume_suspended_windows_process(run.process)
                self.assertIs(primary, caught.exception)
                self.assertTrue(interrupted)
                if stage == "snapshot":
                    run.kernel.CloseHandle.assert_called_once_with(run.snapshot)
                    run.kernel.Thread32First.assert_not_called()
                    run.kernel.OpenThread.assert_not_called()
                    run.kernel.ResumeThread.assert_not_called()
                    self.assertEqual(["snapshot", "close_snapshot"], run.events)
                else:
                    self.assertEqual([mock.call(run.thread), mock.call(run.snapshot)], run.kernel.CloseHandle.call_args_list)
                    run.kernel.ResumeThread.assert_called_once_with(run.thread)
                    self.assertEqual(["close_thread", "close_snapshot"], run.events[-2:])
                owner = getattr(caught.exception, "toolhelp_handles", None)
                if owner is not None:
                    self.assertIsNone(owner.thread)
                    self.assertIsNone(owner.snapshot)

    @unittest.skipUnless(os.name == "nt", "Windows ToolHelp structure with pure mocked kernel")
    def test_toolhelp_final_close_overrun_fails_after_independent_closes(self) -> None:
        for slow_slot in ("thread", "snapshot"):
            with self.subTest(slow_slot=slow_slot), self.inert_toolhelp() as run:
                def close(handle):
                    run.close(handle)
                    slot = "thread" if handle == run.thread else "snapshot"
                    if slot == slow_slot:
                        run.state.now = 100.0 + VERIFIER.WINDOWS_PROCESS_SETUP_TIMEOUT_SECONDS + 1.0
                    return 1

                run.kernel.CloseHandle.side_effect = close
                with self.assertRaisesRegex(RuntimeError, "bound|deadline") as caught:
                    VERIFIER.resume_suspended_windows_process(run.process)
                run.kernel.ResumeThread.assert_called_once_with(run.thread)
                self.assertEqual([mock.call(run.thread), mock.call(run.snapshot)], run.kernel.CloseHandle.call_args_list)
                self.assertEqual(["close_thread", "close_snapshot"], run.events[-2:])
                owner = getattr(caught.exception, "toolhelp_handles", None)
                if owner is not None:
                    self.assertIsNone(owner.thread)
                    self.assertIsNone(owner.snapshot)

    @unittest.skipUnless(os.name == "nt", "Windows ToolHelp structure with pure mocked kernel")
    def test_toolhelp_success_resumes_once_then_closes_both_handles(self) -> None:
        with self.inert_toolhelp() as run:
            self.assertIsNone(VERIFIER.resume_suspended_windows_process(run.process))
            self.assertEqual(["snapshot", "first", "next", "open", "owner", "resume",
                              "close_thread", "close_snapshot"], run.events)
            run.kernel.Thread32First.assert_called_once()
            run.kernel.Thread32Next.assert_called_once()
            run.kernel.OpenThread.assert_called_once_with(0x0802, False, 201)
            run.kernel.GetProcessIdOfThread.assert_called_once_with(run.thread)
            run.kernel.ResumeThread.assert_called_once_with(run.thread)
            self.assertEqual([mock.call(run.thread), mock.call(run.snapshot)], run.kernel.CloseHandle.call_args_list)

    @unittest.skipUnless(os.name == "nt", "Windows ToolHelp structure with pure mocked kernel")
    def test_toolhelp_invalid_snapshot_never_claims_or_closes_a_handle(self) -> None:
        for invalid in (0, VERIFIER.ctypes.c_void_p(-1).value):
            with self.subTest(invalid=invalid), self.inert_toolhelp() as run:
                create_snapshot = run.kernel.CreateToolhelp32Snapshot.side_effect

                def snapshot(flags, pid):
                    create_snapshot(flags, pid)
                    run.state.last_error = 5
                    return invalid

                run.kernel.CreateToolhelp32Snapshot.side_effect = snapshot
                with self.assertRaisesRegex(RuntimeError, "could not enumerate") as caught:
                    VERIFIER.resume_suspended_windows_process(run.process)
                run.kernel.Thread32First.assert_not_called()
                run.kernel.Thread32Next.assert_not_called()
                run.kernel.OpenThread.assert_not_called()
                run.kernel.GetProcessIdOfThread.assert_not_called()
                run.kernel.ResumeThread.assert_not_called()
                run.kernel.CloseHandle.assert_not_called()
                self.assertIsNone(getattr(caught.exception, "toolhelp_handles", None))
                self.assertEqual(["snapshot"], run.events)

    @unittest.skipUnless(os.name == "nt", "Windows ToolHelp structure with pure mocked kernel")
    def test_toolhelp_lookup_rejections_keep_scan_finite_and_close_acquired_handles(self) -> None:
        cases = {
            "no_thread": "no resumable", "multiple_threads": "multiple threads",
            "scan_cap": "bound", "next_error": "could not continue",
            "open_zero": "could not open", "owner_changed": "owner changed",
            "resume_zero": "single CREATE_SUSPENDED", "resume_failed": "could not resume",
        }
        for stage, message in cases.items():
            with self.subTest(stage=stage), self.inert_toolhelp() as run:
                first = run.kernel.Thread32First.side_effect
                open_thread = run.kernel.OpenThread.side_effect
                query_owner = run.kernel.GetProcessIdOfThread.side_effect

                def enumerate_first(handle, pointer):
                    first(handle, pointer)
                    if stage == "no_thread":
                        run.state.last_error = 18
                        return 0
                    if stage == "scan_cap":
                        pointer._obj.owner_process_id = run.process.pid + 1
                    return 1

                def next_entry(handle, pointer):
                    self.assertEqual(run.snapshot, handle)
                    self.assertEqual(VERIFIER.ctypes.sizeof(pointer._obj), pointer._obj.size)
                    run.events.append("next")
                    if stage == "next_error":
                        run.state.last_error = 5
                        return 0
                    pointer._obj.thread_id += 1
                    pointer._obj.owner_process_id = run.process.pid + (1 if stage == "scan_cap" else 0)
                    return 1

                def open_zero(access, inherit, identity):
                    open_thread(access, inherit, identity)
                    run.state.last_error = 5
                    return 0

                def wrong_owner(handle):
                    query_owner(handle)
                    return run.process.pid + 1

                def wrong_resume_count(handle):
                    run.resume(handle)
                    run.state.last_error = 5
                    return 0 if stage == "resume_zero" else 0xFFFFFFFF

                run.kernel.Thread32First.side_effect = enumerate_first
                if stage in ("multiple_threads", "scan_cap", "next_error"):
                    run.kernel.Thread32Next.side_effect = next_entry
                elif stage == "open_zero":
                    run.kernel.OpenThread.side_effect = open_zero
                elif stage == "owner_changed":
                    run.kernel.GetProcessIdOfThread.side_effect = wrong_owner
                elif stage in ("resume_zero", "resume_failed"):
                    run.kernel.ResumeThread.side_effect = wrong_resume_count
                with mock.patch.object(VERIFIER, "MAX_WINDOWS_THREAD_SCAN", 2), \
                     self.assertRaisesRegex(RuntimeError, message) as caught:
                    VERIFIER.resume_suspended_windows_process(run.process)
                expected_next = 0 if stage == "no_thread" else 2 if stage == "scan_cap" else 1
                self.assertEqual(expected_next, run.kernel.Thread32Next.call_count)
                acquired_thread = stage in ("owner_changed", "resume_zero", "resume_failed")
                expected_close = [mock.call(run.thread)] if acquired_thread else []
                expected_close.append(mock.call(run.snapshot))
                self.assertEqual(expected_close, run.kernel.CloseHandle.call_args_list)
                if stage in ("resume_zero", "resume_failed"):
                    run.kernel.ResumeThread.assert_called_once_with(run.thread)
                else:
                    run.kernel.ResumeThread.assert_not_called()
                if stage in ("no_thread", "multiple_threads", "scan_cap", "next_error"):
                    run.kernel.OpenThread.assert_not_called()
                else:
                    run.kernel.OpenThread.assert_called_once_with(0x0802, False, 201)
                self.assertIsNone(getattr(caught.exception, "toolhelp_handles", None))

    @unittest.skipUnless(os.name == "nt", "Windows ToolHelp structure with pure mocked kernel")
    def test_toolhelp_early_snapshot_timeout_preserves_primary_when_close_fails(self) -> None:
        for close_raises in (False, True):
            with self.subTest(close_raises=close_raises), self.inert_toolhelp() as run:
                create_snapshot = run.kernel.CreateToolhelp32Snapshot.side_effect
                secondary = OSError("inert snapshot close failure")

                def snapshot(flags, pid):
                    handle = create_snapshot(flags, pid)
                    run.state.now += VERIFIER.WINDOWS_PROCESS_SETUP_TIMEOUT_SECONDS + 1
                    return handle

                def close(handle):
                    run.close(handle)
                    if close_raises:
                        raise secondary
                    run.state.last_error = 5
                    return 0

                run.kernel.CreateToolhelp32Snapshot.side_effect = snapshot
                run.kernel.CloseHandle.side_effect = close
                with self.assertRaises(VERIFIER.CleanupFailure) as caught:
                    VERIFIER.resume_suspended_windows_process(run.process)
                self.assertIsInstance(caught.exception.primary, RuntimeError)
                self.assertRegex(str(caught.exception.primary), "thread lookup exceeded its bound")
                self.assertIs(caught.exception.primary, caught.exception.__cause__)
                self.assertIsNot(secondary, caught.exception.primary)
                self.assertIn("snapshot close", caught.exception.cleanup_summary)
                self.assertEqual(run.snapshot, caught.exception.toolhelp_handles.snapshot)
                self.assertIsNone(caught.exception.toolhelp_handles.thread)
                run.kernel.CloseHandle.assert_called_once_with(run.snapshot)
                run.kernel.Thread32First.assert_not_called()
                run.kernel.OpenThread.assert_not_called()
                run.kernel.ResumeThread.assert_not_called()

    @unittest.skipUnless(os.name == "nt", "Windows ToolHelp structure with pure mocked kernel")
    def test_toolhelp_keyboard_interrupt_keeps_primary_and_independent_cleanup(self) -> None:
        for stage in ("snapshot_clock", "thread_close", "post_thread_clock"):
            with self.subTest(stage=stage), self.inert_toolhelp() as run:
                primary = KeyboardInterrupt("inert first ToolHelp interruption")
                secondary = OSError("inert later snapshot close failure")
                interrupted = False

                def clock():
                    nonlocal interrupted
                    event = "snapshot" if stage == "snapshot_clock" else "close_thread"
                    if stage != "thread_close" and run.events and run.events[-1] == event and not interrupted:
                        interrupted = True
                        raise primary
                    return run.state.now

                def close(handle):
                    run.close(handle)
                    if handle == run.snapshot:
                        raise secondary
                    if stage == "thread_close":
                        raise primary
                    return 1

                run.clock.side_effect = clock
                run.kernel.CloseHandle.side_effect = close
                with self.assertRaises(VERIFIER.CleanupFailure) as caught:
                    VERIFIER.resume_suspended_windows_process(run.process)
                self.assertIs(primary, caught.exception.primary)
                self.assertIs(primary, caught.exception.__cause__)
                self.assertEqual(run.snapshot, caught.exception.toolhelp_handles.snapshot)
                self.assertEqual(run.thread if stage == "thread_close" else None, caught.exception.toolhelp_handles.thread)
                expected_close = [] if stage == "snapshot_clock" else [mock.call(run.thread)]
                expected_close.append(mock.call(run.snapshot))
                self.assertEqual(expected_close, run.kernel.CloseHandle.call_args_list)
                if stage == "snapshot_clock":
                    run.kernel.ResumeThread.assert_not_called()
                else:
                    run.kernel.ResumeThread.assert_called_once_with(run.thread)

    @unittest.skipUnless(os.name == "nt", "Windows ToolHelp structure with pure mocked kernel")
    def test_toolhelp_thread_close_timeout_precedes_later_snapshot_close_error(self) -> None:
        with self.inert_toolhelp() as run:
            secondary = OSError("inert later snapshot close failure")

            def close(handle):
                run.close(handle)
                if handle == run.thread:
                    run.state.now += VERIFIER.WINDOWS_PROCESS_SETUP_TIMEOUT_SECONDS + 1
                    return 1
                raise secondary

            run.kernel.CloseHandle.side_effect = close
            with self.assertRaises(VERIFIER.CleanupFailure) as caught:
                VERIFIER.resume_suspended_windows_process(run.process)
            self.assertIsInstance(caught.exception.primary, RuntimeError)
            self.assertRegex(str(caught.exception.primary), "thread lookup exceeded its bound")
            self.assertIs(caught.exception.primary, caught.exception.__cause__)
            self.assertIsNot(secondary, caught.exception.primary)
            self.assertIn("snapshot close", caught.exception.cleanup_summary)
            self.assertIsNone(caught.exception.toolhelp_handles.thread)
            self.assertEqual(run.snapshot, caught.exception.toolhelp_handles.snapshot)
            self.assertEqual([mock.call(run.thread), mock.call(run.snapshot)], run.kernel.CloseHandle.call_args_list)
            run.kernel.ResumeThread.assert_called_once_with(run.thread)

    @unittest.skipUnless(os.name == "nt", "Windows ToolHelp structure with pure mocked kernel")
    def test_toolhelp_holder_retains_kernel_and_cleared_slot_is_not_closed_again(self) -> None:
        for failed_slot in ("thread", "snapshot"):
            with self.subTest(failed_slot=failed_slot), self.inert_toolhelp() as run:
                def close(handle):
                    run.close(handle)
                    slot = "thread" if handle == run.thread else "snapshot"
                    if slot == failed_slot:
                        run.state.last_error = 5
                        return 0
                    return 1

                run.kernel.CloseHandle.side_effect = close
                with self.assertRaises(OSError) as caught:
                    VERIFIER.resume_suspended_windows_process(run.process)
                owner = caught.exception.toolhelp_handles
                self.assertIsInstance(owner, VERIFIER._ToolhelpHandles)
                self.assertIs(run.kernel, owner._kernel32)
                self.assertIs(VERIFIER.wintypes.BOOL, owner._kernel32.CloseHandle.restype)
                cleared_slot = "snapshot" if failed_slot == "thread" else "thread"
                self.assertIsNone(getattr(owner, cleared_slot))
                owner.close_slot(cleared_slot)
                owner.close_slot(cleared_slot)
                self.assertEqual([mock.call(run.thread), mock.call(run.snapshot)], run.kernel.CloseHandle.call_args_list)
                self.assertEqual(getattr(run, failed_slot), getattr(owner, failed_slot))

    @unittest.skipUnless(os.name == "nt", "Windows ToolHelp structure with pure mocked kernel")
    def test_toolhelp_owner_survives_clean_and_failed_outer_runner_cleanup(self) -> None:
        for outer_failure in (None, "job_close", "root_wait"):
            with self.subTest(outer_failure=outer_failure):
                with self.inert_toolhelp() as setup:
                    def close(handle):
                        setup.close(handle)
                        if handle == setup.thread:
                            setup.state.last_error = 5
                            return 0
                        return 1

                    setup.kernel.CloseHandle.side_effect = close
                    with self.assertRaises(OSError) as original:
                        VERIFIER.resume_suspended_windows_process(setup.process)
                    primary = original.exception
                    owner = primary.toolhelp_handles
                with self.inert_runner() as run, \
                     mock.patch.object(VERIFIER.subprocess, "run", side_effect=AssertionError("numeric cleanup forbidden")) as numeric, \
                     mock.patch.object(socket, "socket", side_effect=AssertionError("socket forbidden")) as network:
                    run.resume.side_effect = primary
                    if outer_failure == "job_close":
                        run.job.close.side_effect = OSError("inert outer Job close failure")
                    elif outer_failure == "root_wait":
                        run.process.wait.side_effect = OSError("inert outer root wait failure")
                    with self.assertRaises(OSError if outer_failure is None else SystemExit) as caught:
                        VERIFIER.run_bounded(["never executed"], SCRIPT.parent, "ToolHelp owner propagation")
                    if outer_failure is None:
                        self.assertIs(primary, caught.exception)
                        retained_primary = caught.exception
                    else:
                        self.assertIsInstance(caught.exception.__cause__, VERIFIER.CleanupFailure)
                        retained_primary = caught.exception.__cause__.primary
                        self.assertIs(primary, retained_primary)
                        self.assertIs(run.job, caught.exception.job)
                        self.assertIs(run.process, caught.exception.process)
                    self.assertIs(owner, retained_primary.toolhelp_handles)
                    self.assertIs(setup.kernel, owner._kernel32)
                    self.assertEqual(setup.thread, owner.thread)
                    self.assertIsNone(owner.snapshot)
                    run.job.terminate.assert_called_once_with()
                    run.job.wait_empty.assert_called_once()
                    run.job.close.assert_called_once_with()
                    run.process.wait.assert_called_once()
                    for reader in run.readers:
                        reader.join.assert_called_once()
                    run.process.stdout.close.assert_called_once_with()
                    run.process.stderr.close.assert_called_once_with()
                    numeric.assert_not_called()
                    network.assert_not_called()

    @contextmanager
    def inert_runner(self):
        process = mock.Mock(spec=["stdout", "stderr", "wait", "poll", "kill"])
        process.poll.return_value = None
        process.wait.return_value = 0
        job = mock.Mock()
        job.wait_empty.return_value = True
        readers = [mock.Mock(), mock.Mock()]
        for index, reader in enumerate(readers):
            reader.ident = None
            reader.is_alive.return_value = False
            reader.start.side_effect = lambda index=index: setattr(readers[index], "ident", 1)
        with ExitStack() as stack:
            stack.enter_context(mock.patch.object(VERIFIER, "os", SimpleNamespace(name="nt")))
            stack.enter_context(mock.patch.object(VERIFIER.subprocess, "Popen", return_value=process))
            stack.enter_context(mock.patch.object(VERIFIER.WindowsJob, "attach", return_value=job))
            resume = stack.enter_context(mock.patch.object(VERIFIER, "resume_suspended_windows_process"))
            constructor = stack.enter_context(mock.patch.object(VERIFIER.threading, "Thread", side_effect=readers))
            yield SimpleNamespace(process=process, job=job, readers=readers, resume=resume,
                                  constructor=constructor)

    def test_job_drain_unknown_or_failure_cannot_return_success(self) -> None:
        for result in (None, False, 1, mock.Mock(), OSError("inert query denial")):
            with self.subTest(result=type(result).__name__), self.inert_runner() as run:
                if isinstance(result, BaseException):
                    run.job.wait_empty.side_effect = result
                else:
                    run.job.wait_empty.return_value = result
                with self.assertRaisesRegex(SystemExit, "Job drain") as caught:
                    VERIFIER.run_bounded(["never executed"], SCRIPT.parent, "drain")
                self.assertIs(run.job, caught.exception.job)
                run.job.close.assert_called_once_with()
                self.assertEqual(2, run.process.wait.call_count)
                for reader in run.readers:
                    reader.join.assert_called_once()
                run.process.stdout.close.assert_called_once_with()
                run.process.stderr.close.assert_called_once_with()

    def test_job_drain_uses_existing_deadline_before_close(self) -> None:
        with self.inert_runner() as run:
            order = []
            run.job.terminate.side_effect = lambda: order.append("terminate")
            run.job.wait_empty.side_effect = lambda deadline: order.append(("drain", deadline)) or True
            run.job.close.side_effect = lambda: order.append("close")
            with mock.patch.object(VERIFIER.time, "monotonic", side_effect=[100, 104, 106, 108]):
                result = VERIFIER.run_bounded(["never executed"], SCRIPT.parent, "drain")
            self.assertEqual(0, result.returncode)
            self.assertEqual(["terminate", ("drain", 110), "close"], order)
            self.assertEqual(6, run.process.wait.call_args.kwargs["timeout"])
            self.assertEqual(4, run.readers[0].join.call_args.kwargs["timeout"])
            self.assertEqual(2, run.readers[1].join.call_args.kwargs["timeout"])

    def test_job_drain_preserves_first_error_and_failed_close(self) -> None:
        for stage in ("terminate", "query", "close"):
            with self.subTest(stage=stage), self.inert_runner() as run:
                run.process.wait.side_effect = [23, 23]
                getattr(run.job, {"terminate": "terminate", "query": "wait_empty", "close": "close"}[stage]).side_effect = OSError("inert secondary")
                with self.assertRaisesRegex(SystemExit, "exited with 23"):
                    VERIFIER.run_bounded(["never executed"], SCRIPT.parent, "first error")
                run.job.wait_empty.assert_called_once()
                run.job.close.assert_called_once_with()
                self.assertEqual(2, run.process.wait.call_count)

    @unittest.skipUnless(os.name == "nt", "Windows accounting ABI with mocked kernel")
    def test_job_accounting_checks_fixed_layout_bool_and_exact_length(self) -> None:
        information_type = VERIFIER._JobBasicAccountingInformation
        self.assertEqual(48, VERIFIER.ctypes.sizeof(information_type))
        self.assertEqual(40, information_type.active_processes.offset)
        for result, written, expected in ((1, 48, 7), (0, 48, None), (0, None, None),
                                           (1, None, None), (1, 0, None),
                                           (1, 47, None), (1, 49, None)):
            kernel = mock.MagicMock()
            def query(handle, information_class, address, size, returned):
                self.assertEqual((73, 1, 48), (handle, information_class, size))
                information = VERIFIER.ctypes.cast(address, VERIFIER.ctypes.POINTER(information_type)).contents
                if written is not None:
                    information.active_processes = 7
                    VERIFIER.ctypes.cast(returned, VERIFIER.ctypes.POINTER(VERIFIER.wintypes.DWORD)).contents.value = written
                return result
            kernel.QueryInformationJobObject.side_effect = query
            with self.subTest(result=result, written=written), \
                 mock.patch.object(VERIFIER.ctypes, "WinDLL", return_value=kernel), \
                 mock.patch.object(VERIFIER.ctypes, "get_last_error", return_value=5):
                job = VERIFIER.WindowsJob(73)
                if expected is None:
                    with self.assertRaises((OSError, RuntimeError)):
                        job.active_processes()
                else:
                    self.assertEqual(expected, job.active_processes())
                kernel.QueryInformationJobObject.assert_called_once()
                self.assertEqual(73, job.handle)

    def test_job_accounting_never_queries_closed_or_non_windows_owner(self) -> None:
        for host, handle in (("nt", 0), ("posix", 73)):
            with self.subTest(host=host), \
                 mock.patch.object(VERIFIER, "os", SimpleNamespace(name=host)), \
                 mock.patch.object(VERIFIER.ctypes, "WinDLL", create=True) as load:
                with self.assertRaisesRegex(RuntimeError, "Job"):
                    VERIFIER.WindowsJob(handle).active_processes()
                load.assert_not_called()

    def test_job_wait_empty_counts_active_processes_not_root_or_limits(self) -> None:
        job = VERIFIER.WindowsJob(73)
        for counts in ([2, 1, 0], [0]):
            with self.subTest(counts=counts), \
                 mock.patch.object(job, "active_processes", side_effect=counts) as query, \
                 mock.patch.object(VERIFIER.time, "monotonic", return_value=100), \
                 mock.patch.object(VERIFIER.time, "sleep") as sleep:
                self.assertIs(True, job.wait_empty(110))
                self.assertEqual(len(counts), query.call_count)
                self.assertEqual(len(counts) - 1, sleep.call_count)
                for call in sleep.call_args_list:
                    self.assertGreater(call.args[0], 0)
                    self.assertLessEqual(call.args[0], 0.01)

    def test_job_wait_empty_invalid_expired_and_late_zero_fail(self) -> None:
        job = VERIFIER.WindowsJob(73)
        for deadline in (float("nan"), float("inf"), -1, 100):
            with self.subTest(deadline=deadline), \
                 mock.patch.object(job, "active_processes", create=True) as query, \
                 mock.patch.object(VERIFIER.time, "monotonic", return_value=100), \
                 mock.patch.object(VERIFIER.time, "sleep") as sleep:
                with self.assertRaises((ValueError, RuntimeError)):
                    job.wait_empty(deadline)
                query.assert_not_called()
                sleep.assert_not_called()
        with mock.patch.object(job, "active_processes", return_value=0, create=True) as query, \
             mock.patch.object(VERIFIER.time, "monotonic", side_effect=[100, 111]), \
             mock.patch.object(VERIFIER.time, "sleep") as sleep:
            with self.assertRaisesRegex(RuntimeError, "deadline"):
                job.wait_empty(110)
            query.assert_called_once()
            sleep.assert_not_called()

    def test_job_wait_empty_never_reuses_zero_and_rejects_invalid_counts(self) -> None:
        job = VERIFIER.WindowsJob(73)
        with mock.patch.object(job, "active_processes", side_effect=[0, 1]) as query, \
             mock.patch.object(VERIFIER, "MAX_WINDOWS_JOB_DRAIN_POLLS", 1), \
             mock.patch.object(VERIFIER.time, "monotonic", return_value=100):
            self.assertIs(True, job.wait_empty(110))
            with self.assertRaisesRegex(RuntimeError, "poll"):
                job.wait_empty(110)
            self.assertEqual(2, query.call_count)
        for count in (None, False, -1, "0", mock.Mock()):
            with self.subTest(count=type(count).__name__), \
                 mock.patch.object(job, "active_processes", return_value=count) as query, \
                 mock.patch.object(VERIFIER.time, "monotonic", return_value=100), \
                 mock.patch.object(VERIFIER.time, "sleep") as sleep:
                with self.assertRaisesRegex(RuntimeError, "invalid active count"):
                    job.wait_empty(110)
                query.assert_called_once()
                sleep.assert_not_called()

    def test_job_wait_empty_checks_deadline_each_poll_and_releases_lock_for_sleep(self) -> None:
        job = VERIFIER.WindowsJob(73)
        def sleep_unlocked(seconds):
            self.assertTrue(job._lock.acquire(blocking=False))
            job._lock.release()
            self.assertLessEqual(seconds, 0.005001)
        with mock.patch.object(job, "active_processes", return_value=1) as query, \
             mock.patch.object(VERIFIER.time, "monotonic", side_effect=[100, 100.995, 101]), \
             mock.patch.object(VERIFIER.time, "sleep", side_effect=sleep_unlocked) as sleep:
            with self.assertRaisesRegex(RuntimeError, "deadline"):
                job.wait_empty(101)
            query.assert_called_once()
            sleep.assert_called_once()

    @unittest.skipUnless(os.name == "nt", "Windows accounting lock with mocked kernel")
    def test_job_accounting_query_and_close_use_same_owner_lock(self) -> None:
        job = VERIFIER.WindowsJob(73)
        entered, release, closing, closed = (threading.Event() for _ in range(4))
        kernel = mock.MagicMock()
        failures = []
        def query(handle, information_class, address, size, returned):
            entered.set()
            if not release.wait(timeout=2):
                raise RuntimeError("inert accounting synchronization deadline")
            VERIFIER.ctypes.cast(returned, VERIFIER.ctypes.POINTER(VERIFIER.wintypes.DWORD)).contents.value = size
            return 1
        def close(handle):
            closed.set()
            return 1
        kernel.QueryInformationJobObject.side_effect = query
        kernel.CloseHandle.side_effect = close
        def invoke(action, marker=None):
            if marker is not None:
                marker.set()
            try:
                action()
            except BaseException as error:
                failures.append(error)
        with mock.patch.object(VERIFIER.ctypes, "WinDLL", return_value=kernel):
            reader = threading.Thread(target=invoke, args=(job.active_processes,), daemon=True)
            closer = threading.Thread(target=invoke, args=(job.close, closing), daemon=True)
            reader.start()
            try:
                self.assertTrue(entered.wait(timeout=1))
                closer.start()
                self.assertTrue(closing.wait(timeout=1))
                self.assertFalse(closed.wait(timeout=0.05))
            finally:
                release.set()
                reader.join(timeout=2)
                if closer.ident is not None:
                    closer.join(timeout=2)
            self.assertFalse(reader.is_alive())
            self.assertFalse(closer.is_alive())
            self.assertFalse(failures)
            self.assertEqual(0, job.handle)
            with self.assertRaisesRegex(RuntimeError, "open owned Job"):
                job.active_processes()
            kernel.QueryInformationJobObject.assert_called_once()

    def test_job_wait_empty_poll_cap_and_query_failure_do_not_retry_forever(self) -> None:
        job = VERIFIER.WindowsJob(73)
        with mock.patch.object(VERIFIER, "MAX_WINDOWS_JOB_DRAIN_POLLS", 3, create=True), \
             mock.patch.object(job, "active_processes", return_value=1, create=True) as query, \
             mock.patch.object(VERIFIER.time, "monotonic", return_value=100), \
             mock.patch.object(VERIFIER.time, "sleep") as sleep:
            with self.assertRaisesRegex(RuntimeError, "poll"):
                job.wait_empty(110)
            self.assertEqual(3, query.call_count)
            self.assertEqual(2, sleep.call_count)
        for cap in (0, -1):
            with self.subTest(cap=cap), \
                 mock.patch.object(VERIFIER, "MAX_WINDOWS_JOB_DRAIN_POLLS", cap, create=True), \
                 mock.patch.object(job, "active_processes", create=True) as query:
                with self.assertRaises((ValueError, RuntimeError)):
                    job.wait_empty(110)
                query.assert_not_called()
        with mock.patch.object(job, "active_processes", side_effect=OSError("inert denial"), create=True) as query, \
             mock.patch.object(VERIFIER.time, "monotonic", return_value=100), \
             mock.patch.object(VERIFIER.time, "sleep") as sleep:
            with self.assertRaises(OSError):
                job.wait_empty(110)
            query.assert_called_once()
            sleep.assert_not_called()

    def test_partial_reader_creation_and_start_preserve_cleanup(self) -> None:
        for stage in ("constructor0", "constructor1", "start0", "start1"):
            with self.subTest(stage=stage), self.inert_runner() as run:
                failure = RuntimeError("injected " + stage)
                if stage.startswith("constructor"):
                    run.constructor.side_effect = [failure] if stage.endswith("0") else [run.readers[0], failure]
                else:
                    run.readers[int(stage[-1])].start.side_effect = failure
                with self.assertRaisesRegex((RuntimeError, SystemExit), "injected " + stage) as caught:
                    VERIFIER.run_bounded(["never executed"], SCRIPT.parent, "partial readers")
                run.resume.assert_not_called()
                run.process.kill.assert_called_once_with()
                run.process.wait.assert_called_once()
                run.job.close.assert_called_once_with()
                for index, stream in enumerate((run.process.stdout, run.process.stderr)):
                    if stage == f"start{index}":
                        stream.close.assert_not_called()
                        self.assertIn("reader start unconfirmed", str(caught.exception))
                        self.assertIs(run.process, caught.exception.process)
                    else:
                        stream.close.assert_called_once_with()
                for reader in run.readers:
                    if reader.ident is None:
                        reader.join.assert_not_called()
                    else:
                        reader.join.assert_called_once()

    def test_cleanup_preserves_nonzero_timeout_and_setup_primary(self) -> None:
        for stage, expected in (("exit", "exited with 23"), ("timeout", "exceeded"),
                                ("resume", "injected resume")):
            with self.subTest(stage=stage), self.inert_runner() as run:
                run.job.close.side_effect = OSError("secondary detail must not escape")
                if stage == "exit":
                    run.process.wait.side_effect = [23, 23]
                elif stage == "timeout":
                    run.process.wait.side_effect = [subprocess.TimeoutExpired("inert", 1), 0]
                else:
                    run.resume.side_effect = RuntimeError("injected resume")
                with self.assertRaises(SystemExit) as caught:
                    VERIFIER.run_bounded(["never executed"], SCRIPT.parent, "primary")
                self.assertIn(expected, str(caught.exception))
                self.assertIn("Job close", str(caught.exception))
                self.assertNotIn("secondary detail", str(caught.exception))
                self.assertIs(run.job, caught.exception.job)
                self.assertIs(run.process, caught.exception.process)
                self.assertIsInstance(caught.exception.__cause__, VERIFIER.CleanupFailure)
                run.process.stderr.close.assert_called_once_with()

    def test_interrupted_reader_ident_does_not_prove_start_registration(self) -> None:
        with self.inert_runner() as run:
            def interrupt_start():
                run.readers[0].ident = 1
                raise KeyboardInterrupt("inert registration gap")
            run.readers[0].start.side_effect = interrupt_start
            run.readers[0].join.side_effect = RuntimeError("not registered yet")
            with self.assertRaises(SystemExit) as caught:
                VERIFIER.run_bounded(["never executed"], SCRIPT.parent, "unknown start")
            self.assertIn("inert registration gap", str(caught.exception))
            run.process.stdout.close.assert_not_called()
            run.process.stderr.close.assert_called_once_with()

    def test_failed_join_never_authorizes_pipe_close_from_alive_flag(self) -> None:
        with self.inert_runner() as run:
            run.readers[0].join.side_effect = KeyboardInterrupt("inert join interruption")
            with self.assertRaises(SystemExit):
                VERIFIER.run_bounded(["never executed"], SCRIPT.parent, "failed join")
            run.process.stdout.close.assert_not_called()
            run.process.stderr.close.assert_called_once_with()

    def test_cleanup_wait_and_both_joins_share_one_deadline(self) -> None:
        with self.inert_runner() as run, \
             mock.patch.object(VERIFIER.time, "monotonic", side_effect=[100.0, 100.0, 110.0, 110.0]):
            VERIFIER.run_bounded(["never executed"], SCRIPT.parent, "cleanup budget")
        self.assertEqual(mock.call(timeout=10.0), run.process.wait.call_args_list[-1])
        for reader in run.readers:
            reader.join.assert_called_once_with(timeout=0.0)

    def test_reader_termination_failure_cannot_be_erased_by_later_cleanup(self) -> None:
        with self.inert_runner() as run:
            run.process.stdout.fileno.return_value = 1
            run.process.stderr.fileno.return_value = 2

            def construct(*, target, args, daemon):
                reader = run.readers[args[1]]
                def start():
                    reader.ident = 1
                    target(*args)
                reader.start.side_effect = start
                return reader

            run.constructor.side_effect = construct
            with mock.patch.object(VERIFIER, "os", SimpleNamespace(name="nt", read=lambda fd, count: b"x" * 32 if fd == 1 else b"")), \
                 mock.patch.object(VERIFIER, "MAX_CAPTURE_BYTES", 8), \
                 mock.patch.object(VERIFIER, "kill_process_tree", side_effect=[RuntimeError("inert reader kill"), None, None]) as kill, \
                 self.assertRaises(SystemExit) as caught:
                VERIFIER.run_bounded(["never executed"], SCRIPT.parent, "overflow")
            self.assertIn("more than 8 bytes", str(caught.exception))
            self.assertIn("output reader failed", str(caught.exception))
            self.assertEqual(3, kill.call_count)
            for reader in run.readers:
                reader.join.assert_called_once()

    @unittest.skipUnless(os.name == "nt", "Windows Job structure with pure mocked kernel")
    def test_job_setup_failure_keeps_actual_owner_even_if_close_fails(self) -> None:
        for stage in ("SetInformationJobObject", "AssignProcessToJobObject", "ctypes"):
            kernel = mock.MagicMock()
            kernel.CreateJobObjectW.return_value = 73
            kernel.SetInformationJobObject.return_value = 1
            kernel.AssignProcessToJobObject.return_value = 1
            kernel.CloseHandle.side_effect = [0, 1]
            if stage == "ctypes":
                kernel.SetInformationJobObject.side_effect = ValueError("inert ctypes fault")
            else:
                getattr(kernel, stage).return_value = 0
            with self.subTest(stage=stage), \
                 mock.patch.object(VERIFIER.ctypes, "WinDLL", return_value=kernel), \
                 mock.patch.object(VERIFIER.ctypes, "get_last_error", return_value=5):
                with self.assertRaises(VERIFIER.WindowsJobSetupError) as caught:
                    VERIFIER.WindowsJob.attach(SimpleNamespace(_handle=99))
                owner = caught.exception.job
                self.assertEqual(73, owner.handle)
                kernel.CloseHandle.assert_not_called()
                if stage != "ctypes":
                    self.assertEqual(5, caught.exception.__cause__.errno)
                with self.assertRaises(OSError):
                    owner.close()
                self.assertEqual(73, owner.handle)
                owner.close()
                self.assertEqual(0, owner.handle)

    def test_termination_and_close_serialize_handle_use(self) -> None:
        entered, release, closing, closed = (threading.Event() for _ in range(4))
        kernel = mock.MagicMock()
        failures = []
        def terminate(handle, status):
            entered.set()
            if not release.wait(timeout=2):
                raise RuntimeError("inert synchronization deadline")
            return 1
        def close(handle):
            closed.set()
            return 1
        kernel.TerminateJobObject.side_effect = terminate
        kernel.CloseHandle.side_effect = close
        job = VERIFIER.WindowsJob(73)
        def invoke(action, marker=None):
            if marker is not None:
                marker.set()
            try:
                action()
            except BaseException as error:
                failures.append(error)
        with mock.patch.object(VERIFIER, "os", SimpleNamespace(name="nt")), \
             mock.patch.object(VERIFIER.ctypes, "WinDLL", return_value=kernel, create=True):
            first = threading.Thread(target=invoke, args=(job.terminate,), daemon=True)
            second = threading.Thread(target=invoke, args=(job.close, closing), daemon=True)
            try:
                first.start()
                self.assertTrue(entered.wait(timeout=1))
                second.start()
                self.assertTrue(closing.wait(timeout=1))
                self.assertFalse(closed.wait(timeout=0.05), "handle closed during termination")
            finally:
                release.set()
                first.join(timeout=2)
                if second.ident is not None:
                    second.join(timeout=2)
            self.assertFalse(first.is_alive())
            self.assertFalse(second.is_alive())
            self.assertFalse(failures)
            self.assertTrue(closed.is_set())
            self.assertEqual(0, job.handle)

    def test_unix_kill_failures_report_uncertainty_but_disappearance_is_benign(self) -> None:
        for stage in ("group", "poll", "root", "gone"):
            with self.subTest(stage=stage):
                process = mock.Mock(spec=["pid", "poll", "kill"])
                process.pid = 42
                process.poll.return_value = None
                killpg = mock.Mock()
                if stage == "group":
                    killpg.side_effect = PermissionError("inert denial")
                elif stage == "poll":
                    process.poll.side_effect = OSError("inert poll failure")
                elif stage == "root":
                    process.kill.side_effect = PermissionError("inert denial")
                else:
                    killpg.side_effect = ProcessLookupError("already gone")
                    process.kill.side_effect = ProcessLookupError("already gone")
                with mock.patch.object(VERIFIER, "os", SimpleNamespace(name="posix", killpg=killpg)), \
                     mock.patch.object(VERIFIER, "signal", SimpleNamespace(SIGKILL=9)):
                    if stage == "gone":
                        VERIFIER.kill_process_tree(process, None)
                    else:
                        with self.assertRaises(RuntimeError):
                            VERIFIER.kill_process_tree(process, None)
                process.kill.assert_called_once_with()

    def test_job_terminate_false_reports_failure_and_retains_handle(self) -> None:
        kernel = mock.MagicMock()
        kernel.TerminateJobObject.return_value = 0
        job = VERIFIER.WindowsJob(73)
        with mock.patch.object(VERIFIER, "os", SimpleNamespace(name="nt")), \
             mock.patch.object(VERIFIER.ctypes, "WinDLL", return_value=kernel, create=True), \
             mock.patch.object(VERIFIER.ctypes, "get_last_error", return_value=5, create=True), \
             self.assertRaisesRegex(OSError, "TerminateJobObject"):
            job.terminate()
        self.assertEqual(73, job.handle)

    def test_job_close_false_retains_handle_then_success_is_idempotent(self) -> None:
        kernel = mock.MagicMock()
        kernel.CloseHandle.side_effect = [0, 1]
        job = VERIFIER.WindowsJob(73)
        with mock.patch.object(VERIFIER, "os", SimpleNamespace(name="nt")), \
             mock.patch.object(VERIFIER.ctypes, "WinDLL", return_value=kernel, create=True), \
             mock.patch.object(VERIFIER.ctypes, "get_last_error", return_value=5, create=True):
            with self.assertRaisesRegex(OSError, "CloseHandle"):
                job.close()
            self.assertEqual(73, job.handle)
            job.close()
            self.assertEqual(0, job.handle)
            job.close()
        self.assertEqual(2, kernel.CloseHandle.call_count)

    def test_job_terminate_failure_still_kills_retained_root_without_pid(self) -> None:
        process = mock.Mock(spec=["poll", "kill"])
        process.poll.return_value = None
        job = mock.Mock()
        job.terminate.side_effect = RuntimeError("injected termination failure")
        with mock.patch.object(VERIFIER, "os", SimpleNamespace(name="nt")), \
             self.assertRaises(RuntimeError):
            VERIFIER.kill_process_tree(process, job)
        process.kill.assert_called_once_with()

    def test_finished_stream_close_failure_does_not_skip_other_stream(self) -> None:
        streams = (mock.Mock(), mock.Mock())
        streams[0].close.side_effect = OSError("injected close failure")
        readers = [mock.Mock(), mock.Mock()]
        for reader in readers:
            reader.is_alive.return_value = False
        with self.assertRaises((OSError, RuntimeError)):
            VERIFIER.close_finished_output_streams(streams, readers)
        streams[1].close.assert_called_once_with()

    def test_cleanup_job_failure_attempts_all_independent_steps(self) -> None:
        # No process, descriptor read, or numeric PID is used in this matrix.
        for stage in ("terminate", "close"):
            with self.subTest(stage=stage):
                process = mock.Mock(spec=["stdout", "stderr", "wait", "poll", "kill"])
                process.poll.return_value = None
                process.wait.return_value = 0
                job = mock.Mock()
                getattr(job, stage).side_effect = RuntimeError("injected " + stage)
                readers = [mock.Mock(), mock.Mock()]
                for reader in readers:
                    reader.is_alive.return_value = False
                with mock.patch.object(VERIFIER, "os", SimpleNamespace(name="nt")), \
                     mock.patch.object(VERIFIER.subprocess, "Popen", return_value=process), \
                     mock.patch.object(VERIFIER.WindowsJob, "attach", return_value=job), \
                     mock.patch.object(VERIFIER, "resume_suspended_windows_process"), \
                     mock.patch.object(VERIFIER.threading, "Thread", side_effect=readers), \
                     self.assertRaises((RuntimeError, SystemExit)):
                    VERIFIER.run_bounded(["not executed"], SCRIPT.parent, "inert cleanup")
                process.kill.assert_called_once_with()
                self.assertGreaterEqual(process.wait.call_count, 2)
                job.close.assert_called_once_with()
                for reader in readers:
                    reader.join.assert_called_once()
                process.stdout.close.assert_called_once_with()
                process.stderr.close.assert_called_once_with()

    def test_windows_no_job_uses_retained_handle_without_numeric_pid_or_subprocess(self) -> None:
        for exited in (False, True):
            with self.subTest(exited=exited):
                # Accessing process.pid is not part of this held-root contract.
                process = mock.Mock(spec=["poll", "kill"])
                process.poll.return_value = 0 if exited else None
                with mock.patch.object(VERIFIER, "os", SimpleNamespace(name="nt")), \
                     mock.patch.object(VERIFIER.subprocess, "run") as numeric:
                    VERIFIER.kill_process_tree(process, None)
                numeric.assert_not_called()
                if exited:
                    process.kill.assert_not_called()
                else:
                    process.kill.assert_called_once_with()

    @unittest.skipUnless(os.name == "nt", "real harmless suspended Windows root")
    def test_windows_no_job_real_held_root_exits_without_ever_resuming(self) -> None:
        with tempfile.TemporaryDirectory(prefix="ku-native-held-root-") as raw:
            directory = Path(raw)
            marker = directory / "executed"
            child = "from pathlib import Path; Path(" + repr(str(marker)) + ").write_text('inert', encoding='ascii')"
            process = subprocess.Popen([sys.executable, "-B", "-c", child], cwd=directory,
                                       stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL,
                                       stderr=subprocess.DEVNULL, creationflags=0x08000204)
            try:
                self.assertIsNone(process.poll())
                with mock.patch.object(VERIFIER.subprocess, "run", side_effect=AssertionError("numeric tree forbidden")):
                    VERIFIER.kill_process_tree(process, None)
                process.wait(timeout=5)
                self.assertIsNotNone(process.poll())
                self.assertFalse(marker.exists())
            finally:
                # The exact process handle, no external PID/PPID discovery.
                if process.poll() is None:
                    process.kill()
                process.wait(timeout=5)

    @unittest.skipUnless(os.name == "nt", "real harmless closed Windows Job")
    def test_windows_closed_job_keeps_identity_for_retained_root_cleanup(self) -> None:
        with tempfile.TemporaryDirectory(prefix="ku-native-closed-job-") as raw:
            directory = Path(raw)
            marker = directory / "executed"
            child = "from pathlib import Path; Path(" + repr(str(marker)) + ").write_text('inert', encoding='ascii')"
            process = subprocess.Popen([sys.executable, "-B", "-c", child], cwd=directory,
                                       stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL,
                                       stderr=subprocess.DEVNULL, creationflags=0x08000204)
            job = None
            try:
                job = VERIFIER.WindowsJob.attach(process)
                self.assertIsNotNone(job, "host cannot establish required Job containment")
                job.close()
                self.assertEqual(job.handle, 0)
                # A closed Job is still the assigned Job object, not the
                # unassigned None case. Root exit alone cannot prove this branch.
                with mock.patch.object(job, "terminate", wraps=job.terminate) as terminate, \
                     mock.patch.object(VERIFIER.subprocess, "run", side_effect=AssertionError("numeric tree forbidden")):
                    VERIFIER.kill_process_tree(process, job)
                terminate.assert_called_once_with()
                process.wait(timeout=5)
                self.assertIsNotNone(process.poll())
                self.assertFalse(marker.exists())
            finally:
                if job is not None:
                    try:
                        job.terminate()
                    finally:
                        job.close()
                if process.poll() is None:
                    process.kill()
                process.wait(timeout=5)

    @unittest.skipUnless(os.name == "nt", "Windows ToolHelp deadline contract")
    def test_windows_thread_scan_deadline_includes_snapshot_creation(self) -> None:
        process = mock.Mock(pid=os.getpid())
        with (
            # The third observation is the new post-close check, at the same
            # expired instant; the original five-second setup budget is intact.
            mock.patch.object(VERIFIER.time, "monotonic", side_effect=[100.0, 106.0, 106.0]),
            self.assertRaisesRegex(RuntimeError, "thread lookup exceeded its bound"),
        ):
            VERIFIER.resume_suspended_windows_process(process)

    def test_main_emits_one_escaped_actions_annotation(self) -> None:
        stderr = io.StringIO()
        failure = SystemExit("bad % value\r\nnext line")
        with (
            mock.patch.object(VERIFIER, "run", side_effect=failure),
            mock.patch.dict(os.environ, {"GITHUB_ACTIONS": "true"}),
            redirect_stderr(stderr),
            self.assertRaisesRegex(SystemExit, "bad % value"),
        ):
            VERIFIER.main()
        self.assertEqual(
            stderr.getvalue(),
            "::error title=Native three-OS verification::"
            "bad %25 value%0D%0Anext line\n",
        )

    def test_live_pipe_reader_is_never_closed_synchronously(self) -> None:
        read_fd, write_fd = os.pipe()
        stream = os.fdopen(read_fd, "rb")
        started = threading.Event()

        def read_pipe() -> None:
            started.set()
            stream.read(8192)

        reader = threading.Thread(target=read_pipe, daemon=True)
        reader.start()
        self.assertTrue(started.wait(timeout=1))
        try:
            before = time.monotonic()
            VERIFIER.close_finished_output_streams((stream,), [reader])
            self.assertLess(time.monotonic() - before, 0.5)
            self.assertFalse(stream.closed)
        finally:
            os.close(write_fd)
            reader.join(timeout=2)
            self.assertFalse(reader.is_alive())
            VERIFIER.close_finished_output_streams((stream,), [reader])
        self.assertTrue(stream.closed)

    def test_normal_output_is_collected(self) -> None:
        result = VERIFIER.run_bounded(
            [sys.executable, "-B", "-c", "print('bounded-ok')"],
            SCRIPT.parent,
            "normal output self-test",
        )
        self.assertEqual(result.stdout.replace(b"\r\n", b"\n"), b"bounded-ok\n")

    def test_direct_exit_terminates_inherited_pipe_descendant(self) -> None:
        child = (
            "import subprocess,sys; "
            "subprocess.Popen([sys.executable,'-B','-c',"
            "\"import time; time.sleep(3); print('descendant-escaped',flush=True)\"]); "
            "print('parent-exited',flush=True)"
        )
        result = VERIFIER.run_bounded(
            [sys.executable, "-B", "-c", child],
            SCRIPT.parent,
            "inherited pipe self-test",
        )
        self.assertEqual(result.stdout.replace(b"\r\n", b"\n"), b"parent-exited\n")

    @unittest.skipUnless(os.name == "nt", "Windows real owned Job descendant accounting")
    def test_windows_job_observes_live_descendant_after_launcher_exit_then_drains(self) -> None:
        # Deliberately not TemporaryDirectory: uncertain cleanup keeps evidence.
        directory = Path(tempfile.mkdtemp(prefix="ku-job-drain-"))
        ready, expired = directory / "ready", directory / "expired"
        descendant = (
            "from pathlib import Path; import time; "
            f"Path({str(ready)!r}).write_bytes(b'ready'); "
            "time.sleep(15); "
            f"Path({str(expired)!r}).write_bytes(b'expired')"
        )
        launcher = (
            "import subprocess,sys; "
            f"subprocess.Popen([sys.executable,'-I','-B','-c',{descendant!r}],"
            "stdin=subprocess.DEVNULL,stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL,"
            "close_fds=True,creationflags=0x08000000)"
        )
        process = None
        job = None
        primary = None
        observed_before = None
        observed_after = None
        cleanup = VERIFIER.CleanupErrors()
        try:
            process = subprocess.Popen(
                # Markers are absolute; their disposable directory need not be
                # a child CWD whose filesystem lifetime this test does not own.
                [sys.executable, "-I", "-B", "-c", launcher], cwd=SCRIPT.parent,
                stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
                close_fds=True, creationflags=0x08000204,  # NO_WINDOW | NEW_GROUP | SUSPENDED
            )
            try:
                job = VERIFIER.WindowsJob.attach(process)
            except VERIFIER.WindowsJobSetupError as error:
                job = error.job
                raise
            self.assertIsNotNone(job, "real launcher must be contained before resume")
            VERIFIER.resume_suspended_windows_process(process)
            setup_deadline = time.monotonic() + 5
            self.assertEqual(0, process.wait(timeout=max(0, setup_deadline - time.monotonic())))
            for _ in range(1000):
                if time.monotonic() >= setup_deadline:
                    self.fail("descendant readiness exceeded its setup deadline")
                if ready.exists():
                    with ready.open("rb") as marker:
                        content = marker.read(6)
                    if content == b"ready":
                        break
                    self.assertIn(content, (b"",), "invalid bounded readiness marker")
                time.sleep(min(0.005, max(0, setup_deadline - time.monotonic())))
            else:
                self.fail("descendant readiness exceeded its poll bound")
            self.assertEqual(0, process.poll())
            self.assertFalse(expired.exists(), "descendant expired before observation")
            observed_before = job.active_processes()
            self.assertGreater(observed_before, 0, "exited launcher must still have a live Job descendant")
            self.assertLess(time.monotonic(), setup_deadline)
        except BaseException as error:
            primary = error
        finally:
            deadline = time.monotonic() + 5
            if process is not None:
                cleanup.attempt("fixture tree termination", lambda: VERIFIER.kill_process_tree(process, job))
            if job is not None:
                if cleanup.attempt("fixture Job drain", lambda: job.wait_empty(deadline)) is not True:
                    cleanup.add("fixture Job drain unconfirmed")
                else:
                    observed_after = cleanup.attempt("fixture final active count", job.active_processes)
                    if observed_after != 0 or time.monotonic() >= deadline:
                        cleanup.add("fixture zero-before-close unconfirmed")
                cleanup.attempt("fixture Job close", job.close)
            if process is not None:
                cleanup.attempt("fixture root wait", lambda: process.wait(timeout=max(0, deadline - time.monotonic())))
            if primary is None and not cleanup.failed:
                if cleanup.attempt("fixture expiry check", expired.exists) is not False:
                    cleanup.add("descendant expired rather than being promptly terminated")
                else:
                    for marker in (ready, expired):
                        cleanup.attempt("fixture marker removal", lambda marker=marker: marker.unlink(missing_ok=True))
                    if not cleanup.failed:
                        cleanup.attempt("fixture empty directory removal", directory.rmdir)
        try:
            cleanup.raise_if_any(primary)
        except BaseException as error:
            error.job = job
            error.process = process
            error.fixture_directory = directory
            error.add_note(
                f"owned Job fixture {directory.name}: active_before={observed_before}, "
                f"active_after={observed_after}; directory cleanup not confirmed"
            )
            raise
        self.assertGreater(observed_before, 0)
        self.assertEqual(0, observed_after)
        self.assertEqual(0, job.handle)
        print(f"owned Job drain: launcher exited, active_before={observed_before}, active_after=0 before close; fixed markers removed")

    @unittest.skipUnless(os.name == "nt", "Windows suspended-start contract")
    def test_windows_child_cannot_execute_before_job_assignment(self) -> None:
        with tempfile.TemporaryDirectory(prefix="ku-native-job-order-") as raw:
            directory = Path(raw)
            started = directory / "started"
            assigned = directory / "assigned"
            original_attach = VERIFIER.WindowsJob.attach
            observed_execution_before_assignment = False

            def attach_after_probe(process: subprocess.Popen[bytes]):
                nonlocal observed_execution_before_assignment
                deadline = time.monotonic() + 0.25
                while time.monotonic() < deadline and not started.exists():
                    time.sleep(0.005)
                observed_execution_before_assignment = started.exists()
                job = original_attach(process)
                assigned.write_text("assigned", encoding="utf-8")
                return job

            child = (
                "from pathlib import Path; import sys; "
                f"started=Path({str(started)!r}); assigned=Path({str(assigned)!r}); "
                "started.write_text('started',encoding='utf-8'); "
                "sys.exit(91) if not assigned.exists() else print('assigned-first')"
            )
            with mock.patch.object(
                VERIFIER.WindowsJob, "attach", side_effect=attach_after_probe
            ):
                result = VERIFIER.run_bounded(
                    [sys.executable, "-B", "-c", child],
                    directory,
                    "suspended Windows child ordering self-test",
                )
            self.assertFalse(observed_execution_before_assignment)
            self.assertEqual(
                result.stdout.replace(b"\r\n", b"\n"), b"assigned-first\n"
            )

    @unittest.skipUnless(os.name == "nt", "Windows suspended-start contract")
    def test_windows_job_assignment_failure_never_runs_child(self) -> None:
        with tempfile.TemporaryDirectory(prefix="ku-native-job-failure-") as raw:
            directory = Path(raw)
            marker = directory / "executed"
            child = (
                "from pathlib import Path; "
                f"Path({str(marker)!r}).write_text('executed',encoding='utf-8')"
            )
            with (
                mock.patch.object(VERIFIER.WindowsJob, "attach", return_value=None),
                self.assertRaisesRegex(SystemExit, "could not assign suspended Windows child"),
            ):
                VERIFIER.run_bounded(
                    [sys.executable, "-B", "-c", child],
                    directory,
                    "failed Windows Job assignment self-test",
                )
            self.assertFalse(marker.exists())

    @unittest.skipUnless(os.name == "nt", "Windows suspended-start contract")
    def test_windows_job_assignment_exception_never_runs_child(self) -> None:
        with tempfile.TemporaryDirectory(prefix="ku-native-job-exception-") as raw:
            directory = Path(raw)
            marker = directory / "executed"
            child = (
                "from pathlib import Path; "
                f"Path({str(marker)!r}).write_text('executed',encoding='utf-8')"
            )
            with (
                mock.patch.object(
                    VERIFIER.WindowsJob,
                    "attach",
                    side_effect=RuntimeError("synthetic Job setup failure"),
                ),
                self.assertRaisesRegex(RuntimeError, "synthetic Job setup failure"),
            ):
                VERIFIER.run_bounded(
                    [sys.executable, "-B", "-c", child],
                    directory,
                    "exceptional Windows Job assignment self-test",
                )
            self.assertFalse(marker.exists())

    def test_output_overflow_is_a_bounded_failure(self) -> None:
        previous = VERIFIER.MAX_CAPTURE_BYTES
        VERIFIER.MAX_CAPTURE_BYTES = 1024
        try:
            with self.assertRaisesRegex(SystemExit, "more than 1024 bytes"):
                VERIFIER.run_bounded(
                    [sys.executable, "-B", "-c", "print('x' * 4096)"],
                    SCRIPT.parent,
                    "output overflow self-test",
                )
        finally:
            VERIFIER.MAX_CAPTURE_BYTES = previous

    def test_process_timeout_is_a_bounded_failure(self) -> None:
        previous = VERIFIER.COMMAND_TIMEOUT_SECONDS
        VERIFIER.COMMAND_TIMEOUT_SECONDS = 0.05
        try:
            before = time.monotonic()
            with self.assertRaisesRegex(SystemExit, "exceeded 0.05 seconds"):
                VERIFIER.run_bounded(
                    [sys.executable, "-B", "-c", "import time; time.sleep(30)"],
                    SCRIPT.parent,
                    "timeout self-test",
                )
            self.assertLess(time.monotonic() - before, 5)
        finally:
            VERIFIER.COMMAND_TIMEOUT_SECONDS = previous


if __name__ == "__main__":
    unittest.main(verbosity=2)
