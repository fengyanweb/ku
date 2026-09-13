#!/usr/bin/env python3
"""Build and execute the native ABI CI fixture with strict bounded checks."""

from __future__ import annotations

import argparse
from contextlib import contextmanager
import ctypes
from ctypes import wintypes
import hashlib
import math
import os
import platform
import re
import shutil
import signal
import stat
import subprocess
import sys
import tarfile
import threading
import time
from pathlib import Path
from typing import BinaryIO, Callable, Iterator, NoReturn, TypeVar


MAX_CAPTURE_BYTES = 1024 * 1024
MAX_C_SOURCE_BYTES = 32 * 1024 * 1024
MAX_BINARY_BYTES = 128 * 1024 * 1024
MAX_ARCHIVE_BYTES = MAX_BINARY_BYTES + 1024 * 1024
MAX_CLEANUP_ENTRIES = 4096
CLEANUP_TIMEOUT_SECONDS = 10
COMMAND_TIMEOUT_SECONDS = 300
MAX_WINDOWS_THREAD_SCAN = 65536
WINDOWS_PROCESS_SETUP_TIMEOUT_SECONDS = 5
PROCESS_CLEANUP_TIMEOUT_SECONDS = 10
MAX_WINDOWS_JOB_DRAIN_POLLS = 1024
WINDOWS_JOB_DRAIN_POLL_SECONDS = 0.01
_CleanupValue = TypeVar("_CleanupValue")


class CleanupFailure(RuntimeError):
    def __init__(self, summary: str, primary: BaseException | None = None) -> None:
        self.primary = primary
        self.cleanup_summary = summary
        # Keep the primary intact for caller-side credential redaction. Cutting
        # it through a password before redaction can expose a partial secret.
        # Command output was already capped by MAX_CAPTURE_BYTES at capture.
        prefix = f"{primary}; " if primary is not None else ""
        super().__init__(f"{prefix}cleanup failed: {summary}")


class CleanupErrors:
    """Finite categorical diagnostics; no raw secondary exception/secret text."""

    def __init__(self) -> None:
        self._errors: list[str] = []

    @property
    def failed(self) -> bool:
        return bool(self._errors)

    def add(self, label: str, error: BaseException | None = None) -> None:
        if len(self._errors) >= 12:
            return
        detail = type(error).__name__ if error is not None else "unconfirmed"
        if isinstance(error, CleanupFailure):
            detail += ": " + error.cleanup_summary[:160]
        self._errors.append(f"{label[:80]} ({detail[:200]})")

    def attempt(self, label: str, action: Callable[[], _CleanupValue]) -> _CleanupValue | None:
        try:
            return action()
        except BaseException as error:
            self.add(label, error)
            return None

    def summary(self) -> str:
        return "; ".join(self._errors)

    def raise_if_any(self, primary: BaseException | None = None) -> None:
        if self.failed:
            raise CleanupFailure(self.summary(), primary) from primary
        if primary is not None:
            raise primary


class WindowsJobSetupError(RuntimeError):
    def __init__(self, job: WindowsJob, cause: BaseException) -> None:
        self.job = job
        super().__init__(f"Windows Job setup failed ({type(cause).__name__}); owned Job retained")

TARGET_HOSTS = {
    "x86_64-windows": ("Windows", {"AMD64", "x86_64"}),
    "x86_64-linux": ("Linux", {"AMD64", "x86_64"}),
    "aarch64-darwin": ("Darwin", {"ARM64", "aarch64", "arm64"}),
}


if os.name == "nt":
    class _JobBasicAccountingInformation(ctypes.Structure):
        _fields_ = [
            ("total_user_time", ctypes.c_longlong),
            ("total_kernel_time", ctypes.c_longlong),
            ("this_period_total_user_time", ctypes.c_longlong),
            ("this_period_total_kernel_time", ctypes.c_longlong),
            ("total_page_fault_count", wintypes.DWORD),
            ("total_processes", wintypes.DWORD),
            ("active_processes", wintypes.DWORD),
            ("total_terminated_processes", wintypes.DWORD),
        ]


    class _JobBasicLimitInformation(ctypes.Structure):
        _fields_ = [
            ("per_process_user_time_limit", ctypes.c_longlong),
            ("per_job_user_time_limit", ctypes.c_longlong),
            ("limit_flags", wintypes.DWORD),
            ("minimum_working_set_size", ctypes.c_size_t),
            ("maximum_working_set_size", ctypes.c_size_t),
            ("active_process_limit", wintypes.DWORD),
            ("affinity", ctypes.c_size_t),
            ("priority_class", wintypes.DWORD),
            ("scheduling_class", wintypes.DWORD),
        ]


    class _JobIoCounters(ctypes.Structure):
        _fields_ = [
            ("read_operation_count", ctypes.c_ulonglong),
            ("write_operation_count", ctypes.c_ulonglong),
            ("other_operation_count", ctypes.c_ulonglong),
            ("read_transfer_count", ctypes.c_ulonglong),
            ("write_transfer_count", ctypes.c_ulonglong),
            ("other_transfer_count", ctypes.c_ulonglong),
        ]


    class _JobExtendedLimitInformation(ctypes.Structure):
        _fields_ = [
            ("basic_limit_information", _JobBasicLimitInformation),
            ("io_info", _JobIoCounters),
            ("process_memory_limit", ctypes.c_size_t),
            ("job_memory_limit", ctypes.c_size_t),
            ("peak_process_memory_used", ctypes.c_size_t),
            ("peak_job_memory_used", ctypes.c_size_t),
        ]


    class _ThreadEntry32(ctypes.Structure):
        _fields_ = [
            ("size", wintypes.DWORD),
            ("usage_count", wintypes.DWORD),
            ("thread_id", wintypes.DWORD),
            ("owner_process_id", wintypes.DWORD),
            ("base_priority", wintypes.LONG),
            ("priority_delta", wintypes.LONG),
            ("flags", wintypes.DWORD),
        ]


class WindowsJob:
    """Windows child-tree containment with kill-on-close semantics."""

    _KILL_ON_JOB_CLOSE = 0x00002000
    _EXTENDED_LIMIT_INFORMATION = 9

    def __init__(self, handle: int) -> None:
        self.handle = handle
        self._lock = threading.Lock()

    @classmethod
    def attach(cls, process: subprocess.Popen[bytes]) -> WindowsJob | None:
        if os.name != "nt":
            return None
        kernel32 = ctypes.WinDLL("kernel32", use_last_error=True)
        kernel32.CreateJobObjectW.argtypes = [ctypes.c_void_p, wintypes.LPCWSTR]
        kernel32.CreateJobObjectW.restype = wintypes.HANDLE
        kernel32.SetInformationJobObject.argtypes = [
            wintypes.HANDLE,
            ctypes.c_int,
            ctypes.c_void_p,
            wintypes.DWORD,
        ]
        kernel32.SetInformationJobObject.restype = wintypes.BOOL
        kernel32.AssignProcessToJobObject.argtypes = [wintypes.HANDLE, wintypes.HANDLE]
        kernel32.AssignProcessToJobObject.restype = wintypes.BOOL
        kernel32.CloseHandle.argtypes = [wintypes.HANDLE]
        kernel32.CloseHandle.restype = wintypes.BOOL

        job = cls(0)
        handle = kernel32.CreateJobObjectW(None, None)
        if not handle:
            return None
        job.handle = handle
        try:
            information = _JobExtendedLimitInformation()
            information.basic_limit_information.limit_flags = cls._KILL_ON_JOB_CLOSE
            if not kernel32.SetInformationJobObject(
                handle, cls._EXTENDED_LIMIT_INFORMATION,
                ctypes.byref(information), ctypes.sizeof(information),
            ):
                raise OSError(ctypes.get_last_error(), "SetInformationJobObject failed")
            if not kernel32.AssignProcessToJobObject(
                handle, wintypes.HANDLE(process._handle)  # type: ignore[attr-defined]
            ):
                raise OSError(ctypes.get_last_error(), "AssignProcessToJobObject failed")
        except BaseException as error:
            # The caller owns both the suspended child and this allocated Job.
            # Do not lose its handle through a failed close inside attach().
            raise WindowsJobSetupError(job, error) from error
        return job

    def terminate(self) -> None:
        with self._lock:
            if os.name != "nt" or not self.handle:
                return
            kernel32 = ctypes.WinDLL("kernel32", use_last_error=True)
            kernel32.TerminateJobObject.argtypes = [wintypes.HANDLE, wintypes.UINT]
            kernel32.TerminateJobObject.restype = wintypes.BOOL
            if not kernel32.TerminateJobObject(self.handle, 1):
                raise OSError(ctypes.get_last_error(), "TerminateJobObject failed")

    def close(self) -> None:
        with self._lock:
            if os.name != "nt" or not self.handle:
                return
            kernel32 = ctypes.WinDLL("kernel32", use_last_error=True)
            kernel32.CloseHandle.argtypes = [wintypes.HANDLE]
            kernel32.CloseHandle.restype = wintypes.BOOL
            if not kernel32.CloseHandle(self.handle):
                raise OSError(ctypes.get_last_error(), "CloseHandle failed")
            self.handle = 0

    def active_processes(self) -> int:
        """Query this still-owned Job, never the caller's implicit/null Job."""
        with self._lock:
            if os.name != "nt" or not self.handle:
                raise RuntimeError("Windows Job accounting requires an open owned Job")
            kernel32 = ctypes.WinDLL("kernel32", use_last_error=True)
            kernel32.QueryInformationJobObject.argtypes = [
                wintypes.HANDLE, ctypes.c_int, ctypes.c_void_p,
                wintypes.DWORD, ctypes.POINTER(wintypes.DWORD),
            ]
            kernel32.QueryInformationJobObject.restype = wintypes.BOOL
            information = _JobBasicAccountingInformation()
            returned = wintypes.DWORD(0xFFFFFFFF)
            size = ctypes.sizeof(information)
            if not kernel32.QueryInformationJobObject(
                self.handle, 1, ctypes.byref(information), size, ctypes.byref(returned),
            ):
                raise OSError(ctypes.get_last_error(), "QueryInformationJobObject failed")
            if returned.value != size:
                raise RuntimeError("Windows Job accounting returned an invalid length")
            return int(information.active_processes)

    def wait_empty(self, deadline: float) -> bool:
        """Observe zero before close under the caller's existing cleanup budget.

        This observes contained processes, not arbitrary host process creation.
        The deadline cannot preempt a synchronous kernel call that never returns.
        No owner lock is held while sleeping, and query errors are never retried.
        """
        if type(deadline) not in (int, float) or not math.isfinite(deadline):
            raise ValueError("Windows Job drain requires a finite deadline")
        if type(MAX_WINDOWS_JOB_DRAIN_POLLS) is not int or MAX_WINDOWS_JOB_DRAIN_POLLS <= 0:
            raise ValueError("Windows Job drain requires a positive poll bound")
        for attempt in range(MAX_WINDOWS_JOB_DRAIN_POLLS):
            now = time.monotonic()
            if not math.isfinite(now) or now >= deadline:
                raise RuntimeError("Windows Job drain exceeded its deadline")
            count = self.active_processes()
            now = time.monotonic()
            if not math.isfinite(now) or now >= deadline:
                raise RuntimeError("Windows Job drain exceeded its deadline")
            if type(count) is not int or count < 0:
                raise RuntimeError("Windows Job drain received an invalid active count")
            if count == 0:
                return True
            if attempt + 1 < MAX_WINDOWS_JOB_DRAIN_POLLS:
                time.sleep(min(WINDOWS_JOB_DRAIN_POLL_SECONDS, deadline - now))
        raise RuntimeError("Windows Job drain exceeded its poll bound")


class _ToolhelpHandles:
    """Two temporary owners; uncertainty is retained, never automatically retried."""

    __slots__ = ("_kernel32", "snapshot", "thread")

    def __init__(self, kernel32: ctypes.CDLL) -> None:
        self._kernel32 = kernel32
        self.snapshot: int | None = None
        self.thread: int | None = None

    def close_slot(self, slot: str) -> None:
        handle = getattr(self, slot)
        if handle is not None:
            if not self._kernel32.CloseHandle(handle):
                raise OSError(ctypes.get_last_error(), "ToolHelp CloseHandle failed")
            # Clear before any fallible post-close clock read. A raised close
            # may have an uncertain post-effect outcome: retain, do not retry.
            setattr(self, slot, None)


def resume_suspended_windows_process(process: subprocess.Popen[bytes]) -> None:
    """Resume the CREATE_SUSPENDED initial thread after Job assignment."""
    if os.name != "nt":
        return

    snapshot_flag = 0x00000004  # TH32CS_SNAPTHREAD
    suspend_resume_access = 0x0002  # THREAD_SUSPEND_RESUME
    query_limited_access = 0x0800  # THREAD_QUERY_LIMITED_INFORMATION
    resume_failed = 0xFFFFFFFF
    no_more_files = 18  # ERROR_NO_MORE_FILES
    invalid_handle = ctypes.c_void_p(-1).value
    kernel32 = ctypes.WinDLL("kernel32", use_last_error=True)
    kernel32.CreateToolhelp32Snapshot.argtypes = [wintypes.DWORD, wintypes.DWORD]
    kernel32.CreateToolhelp32Snapshot.restype = wintypes.HANDLE
    kernel32.Thread32First.argtypes = [wintypes.HANDLE, ctypes.POINTER(_ThreadEntry32)]
    kernel32.Thread32First.restype = wintypes.BOOL
    kernel32.Thread32Next.argtypes = [wintypes.HANDLE, ctypes.POINTER(_ThreadEntry32)]
    kernel32.Thread32Next.restype = wintypes.BOOL
    kernel32.OpenThread.argtypes = [wintypes.DWORD, wintypes.BOOL, wintypes.DWORD]
    kernel32.OpenThread.restype = wintypes.HANDLE
    kernel32.GetProcessIdOfThread.argtypes = [wintypes.HANDLE]
    kernel32.GetProcessIdOfThread.restype = wintypes.DWORD
    kernel32.ResumeThread.argtypes = [wintypes.HANDLE]
    kernel32.ResumeThread.restype = wintypes.DWORD
    kernel32.CloseHandle.argtypes = [wintypes.HANDLE]
    kernel32.CloseHandle.restype = wintypes.BOOL

    # ToolHelp calls are synchronous Win32 syscalls and cannot be preempted by
    # this Python deadline.  Start the clock before the first call and fail
    # closed on both the entry bound and observed elapsed time after every
    # syscall returns.
    handles = _ToolhelpHandles(kernel32)
    cleanup = CleanupErrors()
    deadline = time.monotonic() + WINDOWS_PROCESS_SETUP_TIMEOUT_SECONDS
    primary = None
    thread_id = None
    scanned_threads = 0
    try:
        snapshot = kernel32.CreateToolhelp32Snapshot(snapshot_flag, 0)
        if not snapshot or snapshot == invalid_handle:
            raise RuntimeError(
                f"could not enumerate suspended Windows child threads: {ctypes.get_last_error()}"
            )
        handles.snapshot = snapshot
        if time.monotonic() > deadline:
            raise RuntimeError("suspended Windows child thread lookup exceeded its bound")
        entry = _ThreadEntry32()
        entry.size = ctypes.sizeof(entry)
        has_entry = bool(kernel32.Thread32First(snapshot, ctypes.byref(entry)))
        if time.monotonic() > deadline:
            raise RuntimeError("suspended Windows child thread lookup exceeded its bound")
        while has_entry:
            scanned_threads += 1
            if (
                scanned_threads > MAX_WINDOWS_THREAD_SCAN
                or time.monotonic() > deadline
            ):
                raise RuntimeError("suspended Windows child thread lookup exceeded its bound")
            if entry.owner_process_id == process.pid:
                if thread_id is not None:
                    raise RuntimeError(
                        "suspended Windows child unexpectedly exposed multiple threads"
                    )
                thread_id = entry.thread_id
            entry.size = ctypes.sizeof(entry)
            ctypes.set_last_error(0)
            has_entry = bool(kernel32.Thread32Next(snapshot, ctypes.byref(entry)))
            if time.monotonic() > deadline:
                raise RuntimeError("suspended Windows child thread lookup exceeded its bound")
            if not has_entry and ctypes.get_last_error() not in (0, no_more_files):
                raise RuntimeError(
                    "could not continue suspended Windows child thread enumeration: "
                    f"{ctypes.get_last_error()}"
                )
        if thread_id is None:
            raise RuntimeError("suspended Windows child has no resumable initial thread")
        thread_handle = kernel32.OpenThread(
            suspend_resume_access | query_limited_access, False, thread_id
        )
        if not thread_handle:
            raise RuntimeError(
                "could not open the suspended Windows child thread: "
                f"{ctypes.get_last_error()}"
            )
        handles.thread = thread_handle
        if time.monotonic() > deadline:
            raise RuntimeError("suspended Windows child thread lookup exceeded its bound")
        owner_process_id = kernel32.GetProcessIdOfThread(thread_handle)
        if time.monotonic() > deadline:
            raise RuntimeError("suspended Windows child thread lookup exceeded its bound")
        if owner_process_id != process.pid:
            raise RuntimeError(
                "suspended Windows child thread owner changed before resume"
            )
        previous_count = kernel32.ResumeThread(thread_handle)
        if time.monotonic() > deadline:
            raise RuntimeError("suspended Windows child thread lookup exceeded its bound")
        if previous_count == resume_failed:
            raise RuntimeError(
                f"could not resume the suspended Windows child: {ctypes.get_last_error()}"
            )
        if previous_count != 1:
            raise RuntimeError(
                "Windows child did not have the expected single CREATE_SUSPENDED count"
            )
    except BaseException as error:
        primary = error
    for slot in ("thread", "snapshot"):
        if getattr(handles, slot) is None:
            continue
        try:
            handles.close_slot(slot)
        except BaseException as error:
            if primary is None:
                primary = error
            else:
                cleanup.add(f"ToolHelp {slot} close", error)
        # Keep the original setup deadline, including time spent closing. A
        # failed close/clock never skips the other independent acquired owner.
        try:
            if time.monotonic() > deadline:
                raise RuntimeError("suspended Windows child thread lookup exceeded its bound")
        except BaseException as error:
            if primary is None:
                primary = error
            else:
                cleanup.add(f"ToolHelp {slot} close deadline", error)
    try:
        cleanup.raise_if_any(primary)
    except BaseException as error:
        if handles.snapshot is not None or handles.thread is not None:
            error.toolhelp_handles = handles
        raise


def fail(message: str) -> NoReturn:
    raise SystemExit(f"native CI verification failed: {message}")


def workflow_command_escape(message: str) -> str:
    return message.replace("%", "%25").replace("\r", "%0D").replace("\n", "%0A")


def resolve_existing_file(raw: str, label: str) -> Path:
    path = Path(raw).resolve()
    if not path.is_file():
        fail(f"{label} is not a file: {path}")
    return path


def kill_process_tree(
    process: subprocess.Popen[bytes], windows_job: WindowsJob | None
) -> None:
    errors = CleanupErrors()
    if os.name == "nt":
        if windows_job is not None:
            errors.attempt("Job termination", windows_job.terminate)
        # Internal Windows callers create SUSPENDED and never resume without
        # successful Job assignment. None therefore means only an unresumed
        # retained root; use the held Popen handle below, never PID-tree lookup.
        # A previously assigned Job stays non-None even after its handle closes.
    else:
        try:
            os.killpg(process.pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
        except BaseException as error:
            errors.add("process group termination", error)
    # Unknown poll state still permits the independent exact-handle kill.
    if errors.attempt("root poll", process.poll) is None:
        try:
            process.kill()
        except ProcessLookupError:
            pass
        except BaseException as error:
            errors.add("root termination", error)
    errors.raise_if_any()


def close_finished_output_streams(
    streams: tuple[BinaryIO, ...], readers: list[threading.Thread]
) -> None:
    # BufferedReader.close() can wait forever for a different thread's blocked
    # read lock. A daemon reader that still owns an inherited pipe must be left
    # for process teardown after the bounded failure, never closed synchronously.
    errors = CleanupErrors()
    for index, stream in enumerate(streams):
        try:
            if index >= len(readers) or not readers[index].is_alive():
                errors.attempt(f"output {index} close", stream.close)
        except BaseException as error:
            errors.add(f"output {index} reader state", error)
    errors.raise_if_any()


def run_bounded(command: list[str], cwd: Path, label: str) -> subprocess.CompletedProcess[bytes]:
    creation_flags = 0
    if os.name == "nt":
        # No child/compiler code may run before the kill-on-close Job owns it.
        # Popen closes the initial thread handle, so resume it below through a
        # bounded ToolHelp lookup only after successful Job assignment.
        creation_flags = 0x00000200 | 0x00000004  # NEW_PROCESS_GROUP | SUSPENDED
    output = [bytearray(), bytearray()]
    output_bytes = 0
    overflow = threading.Event()
    output_lock = threading.Lock()
    reader_errors = CleanupErrors()
    errors = CleanupErrors()
    process = subprocess.Popen(
        command,
        cwd=cwd,
        stdin=subprocess.DEVNULL,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        bufsize=0,
        creationflags=creation_flags,
        start_new_session=os.name != "nt",
    )
    windows_job = None
    primary = None
    return_code = None
    # Entries exist before construction/start; a partial failure cannot lose
    # the other pipe or cause join() on a thread that was never started.
    readers: list[tuple[BinaryIO, threading.Thread | None, bool | None]] = []

    def drain(stream: BinaryIO, index: int) -> None:
        nonlocal output_bytes
        try:
            descriptor = stream.fileno()
            while chunk := os.read(descriptor, 8192):
                should_kill = False
                with output_lock:
                    remaining = MAX_CAPTURE_BYTES - output_bytes
                    if remaining > 0:
                        kept = chunk[:remaining]
                        output[index].extend(kept)
                        output_bytes += len(kept)
                    if len(chunk) > remaining:
                        if not overflow.is_set():
                            overflow.set()
                            should_kill = True
                if should_kill:
                    kill_process_tree(process, windows_job)
                if overflow.is_set():
                    return
        except BaseException as error:
            with output_lock:
                reader_errors.add(f"output {index} reader", error)
            try:
                kill_process_tree(process, windows_job)
            except BaseException as cleanup_error:
                with output_lock:
                    reader_errors.add(f"output {index} reader termination", cleanup_error)

    assert process.stdout is not None and process.stderr is not None
    streams = (process.stdout, process.stderr)
    try:
        try:
            windows_job = WindowsJob.attach(process)
        except WindowsJobSetupError as error:
            windows_job = error.job
            raise
        if os.name == "nt":
            if windows_job is None:
                fail("could not assign suspended Windows child to a Job Object")
        for index, stream in enumerate(streams):
            reader = threading.Thread(target=drain, args=(stream, index), daemon=True)
            readers.append((stream, reader, False))
            try:
                reader.start()
            except BaseException:
                # OS creation can precede ident/_started registration. Until
                # observed started, an interrupted start is unknown, not proof
                # that no reader can ever own this pipe's buffered-read lock.
                readers[index] = (stream, reader, None)
                raise
            readers[index] = (stream, reader, True)
        if os.name == "nt":
            resume_suspended_windows_process(process)
        return_code = process.wait(timeout=COMMAND_TIMEOUT_SECONDS)
    except subprocess.TimeoutExpired:
        primary = SystemExit(f"native CI verification failed: {label} exceeded {COMMAND_TIMEOUT_SECONDS} seconds")
    except BaseException as error:
        primary = error
    # Even a naturally exited root can leave descendants holding output pipes.
    # Each operation is independent and shares one existing-size cleanup bound;
    # a failed syscall never skips exact-root wait, the Job close, or readers.
    deadline = time.monotonic() + PROCESS_CLEANUP_TIMEOUT_SECONDS
    errors.attempt("process tree termination", lambda: kill_process_tree(process, windows_job))
    if windows_job is not None:
        if errors.attempt("Job drain", lambda: windows_job.wait_empty(deadline)) is not True:
            errors.add("Job drain not confirmed")
        errors.attempt("Job close", windows_job.close)
    errors.attempt("root wait", lambda: process.wait(timeout=max(0.0, deadline - time.monotonic())))
    for index, stream in enumerate(streams):
        reader = readers[index][1] if index < len(readers) else None
        started = readers[index][2] if index < len(readers) else False
        if reader is not None and started is None:
            if reader.ident is not None:
                started = True
            else:
                errors.add(f"output {index} reader start unconfirmed")
                continue
        if reader is not None and started:
            def join_reader() -> bool:
                reader.join(timeout=max(0.0, deadline - time.monotonic()))
                return True
            joined = errors.attempt(f"output {index} reader join", join_reader) is True
            if not joined:
                # A join interruption can itself invalidate thread-state
                # bookkeeping. Do not authorize synchronous close from a later
                # is_alive()==False after any failed join, even after start.
                errors.add(f"output {index} reader join unconfirmed")
                continue
        try:
            alive = reader is not None and reader.is_alive()
        except BaseException as error:
            errors.add(f"output {index} reader state", error)
            alive = True
        if alive:
            errors.add(f"output {index} child process holding an output pipe")
        else:
            errors.attempt(f"output {index} close", stream.close)
    with output_lock:
        stdout, stderr = bytes(output[0]), bytes(output[1])
        if reader_errors.failed:
            errors.add("output reader failed", CleanupFailure(reader_errors.summary()))
    if primary is None and overflow.is_set():
        primary = SystemExit(f"native CI verification failed: {label} produced more than {MAX_CAPTURE_BYTES} bytes of output")
    if primary is None and return_code != 0:
        stdout_text = stdout.decode("utf-8", errors="replace")
        stderr_text = stderr.decode("utf-8", errors="replace")
        primary = SystemExit(f"native CI verification failed: {label} exited with {return_code}\nstdout:\n{stdout_text}\nstderr:\n{stderr_text}")
    if errors.failed:
        # Normalize cleanup-only failures to the existing external fail contract.
        failure = CleanupFailure(errors.summary(), primary)
        exit_error = SystemExit(f"native CI verification failed: {failure}")
        # Preserve actual owners for a catching caller; failed CloseHandle must
        # not turn a still-owned Job into only a string diagnostic.
        exit_error.job = windows_job
        exit_error.process = process
        exit_error.readers = readers
        raise exit_error from failure
    if primary is not None:
        raise primary
    assert return_code is not None
    return subprocess.CompletedProcess(command, return_code, stdout, stderr)


@contextmanager
def source_tree_temporarily_absent(fixture: Path) -> Iterator[None]:
    source = fixture / "src"
    holding = fixture.parent / f".{fixture.name}-source-hold"

    # A forcibly stopped earlier local run must be recoverable without guessing.
    if holding.is_symlink() or (holding.exists() and not holding.is_dir()):
        fail(f"source holding path is not a real directory: {holding}")
    if holding.exists():
        if source.exists() or source.is_symlink():
            fail(f"both source tree and stale backup exist: {source}")
        holding.replace(source)

    if source.is_symlink() or not source.is_dir():
        fail(f"fixture source tree is not a real directory: {source}")

    moved = False
    try:
        source.replace(holding)
        moved = True
        if source.exists() or source.is_symlink() or not holding.is_dir():
            fail("complete fixture source tree was not isolated before native execution")
        yield
    finally:
        restore_errors = []
        if moved:
            if source.exists():
                restore_errors.append(f"{source}: source path was unexpectedly recreated")
            elif not holding.is_dir() or holding.is_symlink():
                restore_errors.append(f"{holding}: source backup is missing or unsafe")
            else:
                try:
                    holding.replace(source)
                except OSError as error:
                    restore_errors.append(f"{source}: {error}")
        if restore_errors:
            fail("could not restore isolated fixture sources: " + "; ".join(restore_errors))


def assert_stdout(completed: subprocess.CompletedProcess[bytes], expected: str, label: str) -> None:
    try:
        stdout = completed.stdout.decode("utf-8", errors="strict").replace("\r\n", "\n")
    except UnicodeDecodeError as error:
        fail(f"{label} stdout was not UTF-8: {error}")
    if stdout != expected:
        fail(f"{label} stdout was {stdout!r}")


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        while chunk := stream.read(64 * 1024):
            digest.update(chunk)
    return digest.hexdigest()


def remove_tree_bounded(path: Path, expected_parent: Path, label: str) -> str | None:
    if not path.exists() and not path.is_symlink():
        return None
    try:
        if path.parent.resolve() != expected_parent.resolve():
            return f"{label} escaped its expected parent: {path}"
        if path.is_symlink() or not path.is_dir():
            return f"{label} is not a real directory: {path}"
        deadline = time.monotonic() + CLEANUP_TIMEOUT_SECONDS
        pending = [path]
        entries = 0
        while pending:
            if time.monotonic() > deadline:
                return f"{label} cleanup scan exceeded {CLEANUP_TIMEOUT_SECONDS} seconds"
            directory = pending.pop()
            with os.scandir(directory) as children:
                for child in children:
                    entries += 1
                    if entries > MAX_CLEANUP_ENTRIES:
                        return f"{label} contains more than {MAX_CLEANUP_ENTRIES} entries"
                    if child.is_dir(follow_symlinks=False):
                        pending.append(Path(child.path))
        shutil.rmtree(path)
        return None
    except OSError as error:
        return f"failed to remove {label} {path}: {error}"


def unlink_known_file(path: Path, label: str) -> str | None:
    try:
        if path.is_dir() and not path.is_symlink():
            return f"{label} unexpectedly became a directory: {path}"
        path.unlink(missing_ok=True)
        return None
    except OSError as error:
        return f"failed to remove {label} {path}: {error}"


def prepare_artifact_directory(path: Path, known_outputs: list[Path]) -> None:
    if path.is_symlink() or (path.exists() and not path.is_dir()):
        fail(f"artifact directory is not a real directory: {path}")
    path.mkdir(parents=True, exist_ok=True)
    for output in known_outputs:
        error = unlink_known_file(output, "stale artifact output")
        if error:
            fail(error)


def create_unix_archive(staged: Path, archive: Path) -> None:
    with staged.open("rb") as stream, tarfile.open(
        archive, "w", format=tarfile.PAX_FORMAT
    ) as output:
        info = output.gettarinfo(str(staged), arcname=staged.name)
        info.mode = 0o755
        info.uid = 0
        info.gid = 0
        info.uname = ""
        info.gname = ""
        info.mtime = 0
        output.addfile(info, stream)

    archive_size = archive.stat().st_size
    if archive_size == 0 or archive_size > MAX_ARCHIVE_BYTES:
        fail(f"portable native archive has invalid size {archive_size}")
    with tarfile.open(archive, "r") as packaged:
        members = packaged.getmembers()
        if len(members) != 1:
            fail("portable native archive must contain exactly one file")
        member = members[0]
        if not member.isfile() or member.name != staged.name:
            fail("portable native archive contains an unexpected entry")
        if member.size != staged.stat().st_size or member.mode & 0o111 != 0o111:
            fail("portable native archive did not preserve executable metadata")
        packaged_file = packaged.extractfile(member)
        if packaged_file is None:
            fail("portable native archive executable could not be read back")
        packaged_digest = hashlib.sha256()
        while chunk := packaged_file.read(64 * 1024):
            packaged_digest.update(chunk)
        if packaged_digest.hexdigest() != sha256_file(staged):
            fail("portable native archive content does not match the staged executable")


def raise_verification_or_cleanup_failure(
    pending_error: BaseException | None, cleanup_errors: list[str]
) -> None:
    if cleanup_errors:
        detail = "; ".join(cleanup_errors)
        if pending_error is not None:
            raise SystemExit(
                f"native CI verification failed: {pending_error}; "
                f"cleanup also failed: {detail}"
            ) from pending_error
        fail(f"verification cleanup failed: {detail}")
    if pending_error is not None:
        raise pending_error


def verify_host(target: str) -> None:
    expected_system, expected_machines = TARGET_HOSTS[target]
    actual_system = platform.system()
    actual_machine = platform.machine()
    if actual_system != expected_system or actual_machine not in expected_machines:
        fail(
            f"target {target} requires {expected_system}/{sorted(expected_machines)}, "
            f"runner is {actual_system}/{actual_machine}"
        )


def build_package(ku: Path, fixture: Path, target: str, label: str) -> None:
    run_bounded(
        [
            str(ku),
            "build",
            "--backend",
            "c",
            "--release",
            "--target",
            target,
            "--locked",
            "--clean",
            ".",
        ],
        fixture,
        label,
    )


def verify_portable_c_artifact(c_source: Path) -> None:
    c_source = resolve_existing_file(str(c_source), "generated portable C artifact")
    c_size = c_source.stat().st_size
    if c_size == 0 or c_size > MAX_C_SOURCE_BYTES:
        fail(f"generated portable C artifact has invalid size {c_size}")
    generated = c_source.read_text(encoding="utf-8")
    for forbidden in ("run_source", "const SOURCE"):
        if forbidden in generated:
            fail(f"generated portable C artifact contains runner marker {forbidden!r}")
    if not re.search(r"__ku_import\d+_Add", generated):
        fail("generated portable C artifact does not contain imported Add implementation")
    if "ku_fs_base_locator" in generated:
        fail("portable fixture unexpectedly contains source-relative std.fs runtime state")


def verify_source_relative_fs(
    ku: Path, fixture: Path, target: str, output_name: str
) -> None:
    build_package(ku, fixture, target, "source-relative std.fs native build")
    build_dir = fixture / ".ku" / "build" / target / "release"
    c_source = resolve_existing_file(
        str(build_dir / "c" / "native_fs_relative.c"),
        "generated source-relative std.fs C artifact",
    )
    c_size = c_source.stat().st_size
    if c_size == 0 or c_size > MAX_C_SOURCE_BYTES:
        fail(f"generated source-relative std.fs C artifact has invalid size {c_size}")
    generated = c_source.read_text(encoding="utf-8")
    if "static KuString ku_fs_base_locator(void)" not in generated:
        fail("source-relative std.fs fixture did not emit its executable-relative locator")
    for forbidden in ("run_source", "const SOURCE"):
        if forbidden in generated:
            fail(f"source-relative std.fs C artifact contains runner marker {forbidden!r}")

    executable = resolve_existing_file(
        str(build_dir / output_name), "source-relative std.fs native executable"
    )
    executable_size = executable.stat().st_size
    if executable_size == 0 or executable_size > MAX_BINARY_BYTES:
        fail(f"source-relative std.fs native executable has invalid size {executable_size}")
    completed = run_bounded(
        [str(executable)], fixture, "source-relative std.fs native executable"
    )
    assert_stdout(completed, "native-fs-relative-ok\n", "source-relative std.fs executable")
    runtime_output = fixture / "src" / "native-fs-output.txt"
    try:
        content = runtime_output.read_text(encoding="utf-8")
    except OSError as error:
        fail(f"source-relative std.fs output was not created under the source data tree: {error}")
    if content != "fs-ok":
        fail("source-relative std.fs round trip did not produce the expected content")
    print(
        "source-relative std.fs verification ok: source data stayed beside its "
        "relocatable locator; this executable is not the uploaded portable artifact"
    )


def run() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--ku", required=True, help="path to the freshly built ku CLI")
    parser.add_argument("--target", required=True, choices=sorted(TARGET_HOSTS))
    parser.add_argument("--artifact-dir", required=True)
    args = parser.parse_args()

    verify_host(args.target)
    ku = resolve_existing_file(args.ku, "ku executable")
    repository = Path(__file__).resolve().parents[2]
    fixture = repository / ".github" / "fixtures" / "native-three-os"
    fs_fixture = fixture / "fs-relative"
    portable_build_root = fixture / ".ku"
    fs_build_root = fs_fixture / ".ku"
    for build_root in (portable_build_root, fs_build_root):
        if build_root.is_symlink():
            fail(f"fixture build root must not be a symbolic link: {build_root}")

    portable_output_name = "native_ci.exe" if args.target == "x86_64-windows" else "native_ci"
    fs_output_name = (
        "native_fs_relative.exe"
        if args.target == "x86_64-windows"
        else "native_fs_relative"
    )
    staged_name = f"native_ci-{args.target}"
    if args.target == "x86_64-windows":
        staged_name += ".exe"
    build_dir = fixture / ".ku" / "build" / args.target / "release"
    c_source = build_dir / "c" / "native_ci.c"
    executable = build_dir / portable_output_name

    artifact_dir = Path(args.artifact_dir).resolve()
    staged = artifact_dir / staged_name
    archive = artifact_dir / f"{staged_name}.tar"
    expected_artifact = staged if args.target == "x86_64-windows" else archive
    prepare_artifact_directory(artifact_dir, [staged, archive])

    fs_runtime_output = fs_fixture / "src" / "native-fs-output.txt"
    lock_guard_outputs = (
        fixture / "ku.lock.io.lock",
        fs_fixture / "ku.lock.io.lock",
    )
    cleanup_errors: list[str] = []
    pending_error: BaseException | None = None
    succeeded = False
    try:
        stale_output_error = unlink_known_file(
            fs_runtime_output, "stale source-relative std.fs output"
        )
        if stale_output_error:
            fail(stale_output_error)
        for lock_guard in lock_guard_outputs:
            stale_lock_error = unlink_known_file(lock_guard, "stale package lock guard")
            if stale_lock_error:
                fail(stale_lock_error)

        build_package(ku, fixture, args.target, "portable native build")
        verify_portable_c_artifact(c_source)
        executable = resolve_existing_file(str(executable), "portable native executable")
        executable_size = executable.stat().st_size
        if executable_size == 0 or executable_size > MAX_BINARY_BYTES:
            fail(f"portable native executable has invalid size {executable_size}")

        # The complete source directory is absent for both runs. The second run
        # is from the exact staging location later used to create the artifact.
        with source_tree_temporarily_absent(fixture):
            original = run_bounded(
                [str(executable)], fixture, "portable native executable before staging"
            )
            assert_stdout(
                original,
                "native-ci-ok\n",
                "portable native executable before staging",
            )
            shutil.copy2(executable, staged)
            if os.name != "nt":
                staged.chmod(staged.stat().st_mode | stat.S_IXUSR | stat.S_IXGRP | stat.S_IXOTH)
            if not staged.is_file() or staged.stat().st_size != executable_size:
                fail("staged portable executable does not match the built executable size")
            if sha256_file(staged) != sha256_file(executable):
                fail("staged portable executable checksum does not match the built executable")
            staged_run = run_bounded(
                [str(staged)], artifact_dir, "staged portable native executable"
            )
            assert_stdout(
                staged_run, "native-ci-ok\n", "staged portable native executable"
            )

        if args.target != "x86_64-windows":
            create_unix_archive(staged, archive)
            staged_error = unlink_known_file(staged, "temporary staged portable executable")
            if staged_error:
                fail(staged_error)

        verify_source_relative_fs(ku, fs_fixture, args.target, fs_output_name)
        if not expected_artifact.is_file():
            fail(f"portable upload artifact was not produced: {expected_artifact}")
        succeeded = True
    except BaseException as error:
        pending_error = error
    finally:
        output_error = unlink_known_file(
            fs_runtime_output, "source-relative std.fs runtime output"
        )
        if output_error:
            cleanup_errors.append(output_error)
        for lock_guard in lock_guard_outputs:
            error = unlink_known_file(lock_guard, "package lock guard")
            if error:
                cleanup_errors.append(error)
        for root, owner, label in (
            (portable_build_root, fixture, "portable fixture build root"),
            (fs_build_root, fs_fixture, "source-relative std.fs fixture build root"),
        ):
            error = remove_tree_bounded(root, owner, label)
            if error:
                cleanup_errors.append(error)
        if not succeeded or cleanup_errors:
            for output in (staged, archive):
                error = unlink_known_file(output, "failed portable artifact output")
                if error:
                    cleanup_errors.append(error)
            try:
                artifact_dir.rmdir()
            except OSError:
                pass

    raise_verification_or_cleanup_failure(pending_error, cleanup_errors)

    print(f"portable native CI artifact ok: {args.target} -> {expected_artifact}")


def main() -> None:
    try:
        run()
    except (Exception, SystemExit) as error:
        if os.environ.get("GITHUB_ACTIONS") == "true":
            detail = workflow_command_escape(str(error))
            print(
                f"::error title=Native three-OS verification::{detail}",
                file=sys.stderr,
            )
        raise


if __name__ == "__main__":
    main()
