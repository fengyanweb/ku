#!/usr/bin/env python3
"""Build and execute the native ABI CI fixture with strict bounded checks."""

from __future__ import annotations

import argparse
from contextlib import contextmanager
import ctypes
from ctypes import wintypes
import hashlib
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
from typing import BinaryIO, Iterator, NoReturn


MAX_CAPTURE_BYTES = 1024 * 1024
MAX_C_SOURCE_BYTES = 32 * 1024 * 1024
MAX_BINARY_BYTES = 128 * 1024 * 1024
MAX_ARCHIVE_BYTES = MAX_BINARY_BYTES + 1024 * 1024
MAX_CLEANUP_ENTRIES = 4096
CLEANUP_TIMEOUT_SECONDS = 10
COMMAND_TIMEOUT_SECONDS = 300

TARGET_HOSTS = {
    "x86_64-windows": ("Windows", {"AMD64", "x86_64"}),
    "x86_64-linux": ("Linux", {"AMD64", "x86_64"}),
    "aarch64-darwin": ("Darwin", {"ARM64", "aarch64", "arm64"}),
}


if os.name == "nt":
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


class WindowsJob:
    """Best-effort Windows child-tree containment with kill-on-close semantics."""

    _KILL_ON_JOB_CLOSE = 0x00002000
    _EXTENDED_LIMIT_INFORMATION = 9

    def __init__(self, handle: int) -> None:
        self.handle = handle

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

        handle = kernel32.CreateJobObjectW(None, None)
        if not handle:
            return None
        information = _JobExtendedLimitInformation()
        information.basic_limit_information.limit_flags = cls._KILL_ON_JOB_CLOSE
        if not kernel32.SetInformationJobObject(
            handle,
            cls._EXTENDED_LIMIT_INFORMATION,
            ctypes.byref(information),
            ctypes.sizeof(information),
        ) or not kernel32.AssignProcessToJobObject(
            handle, wintypes.HANDLE(process._handle)  # type: ignore[attr-defined]
        ):
            kernel32.CloseHandle(handle)
            return None
        return cls(handle)

    def terminate(self) -> None:
        if os.name != "nt" or not self.handle:
            return
        kernel32 = ctypes.WinDLL("kernel32", use_last_error=True)
        kernel32.TerminateJobObject.argtypes = [wintypes.HANDLE, wintypes.UINT]
        kernel32.TerminateJobObject.restype = wintypes.BOOL
        kernel32.TerminateJobObject(self.handle, 1)

    def close(self) -> None:
        if os.name != "nt" or not self.handle:
            return
        kernel32 = ctypes.WinDLL("kernel32", use_last_error=True)
        kernel32.CloseHandle.argtypes = [wintypes.HANDLE]
        kernel32.CloseHandle.restype = wintypes.BOOL
        kernel32.CloseHandle(self.handle)
        self.handle = 0


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
    if os.name == "nt":
        if windows_job is not None:
            windows_job.terminate()
        elif process.poll() is None:
            # Fallback for hosts whose outer Job Object rejects assignment.
            # This reliably contains a still-running direct child; after that
            # child has exited, Windows offers no process-group kill primitive.
            try:
                subprocess.run(
                    ["taskkill", "/PID", str(process.pid), "/T", "/F"],
                    stdin=subprocess.DEVNULL,
                    stdout=subprocess.DEVNULL,
                    stderr=subprocess.DEVNULL,
                    timeout=10,
                    check=False,
                )
            except (OSError, subprocess.TimeoutExpired):
                pass
    else:
        try:
            os.killpg(process.pid, signal.SIGKILL)
        except OSError:
            pass
    if process.poll() is None:
        try:
            process.kill()
        except OSError:
            pass


def close_finished_output_streams(
    streams: tuple[BinaryIO, ...], readers: list[threading.Thread]
) -> None:
    # BufferedReader.close() can wait forever for a different thread's blocked
    # read lock. A daemon reader that still owns an inherited pipe must be left
    # for process teardown after the bounded failure, never closed synchronously.
    for stream, reader in zip(streams, readers):
        if not reader.is_alive():
            stream.close()


def run_bounded(command: list[str], cwd: Path, label: str) -> subprocess.CompletedProcess[bytes]:
    process = subprocess.Popen(
        command,
        cwd=cwd,
        stdin=subprocess.DEVNULL,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        bufsize=0,
        creationflags=subprocess.CREATE_NEW_PROCESS_GROUP if os.name == "nt" else 0,
        start_new_session=os.name != "nt",
    )
    windows_job = WindowsJob.attach(process)
    output = [bytearray(), bytearray()]
    output_bytes = 0
    overflow = threading.Event()
    output_lock = threading.Lock()
    reader_errors: list[str] = []

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
        except (OSError, ValueError) as error:
            with output_lock:
                reader_errors.append(
                    f"{'stdout' if index == 0 else 'stderr'} reader failed: {error}"
                )

    assert process.stdout is not None and process.stderr is not None
    readers = [
        threading.Thread(target=drain, args=(process.stdout, 0), daemon=True),
        threading.Thread(target=drain, args=(process.stderr, 1), daemon=True),
    ]
    for reader in readers:
        reader.start()
    try:
        return_code = process.wait(timeout=COMMAND_TIMEOUT_SECONDS)
    except subprocess.TimeoutExpired:
        kill_process_tree(process, windows_job)
        if windows_job is not None:
            windows_job.close()
        try:
            process.wait(timeout=10)
        except subprocess.TimeoutExpired:
            for reader in readers:
                reader.join(timeout=2)
            close_finished_output_streams((process.stdout, process.stderr), readers)
            fail(f"{label} process tree did not terminate after timeout")
        for reader in readers:
            reader.join(timeout=2)
        close_finished_output_streams((process.stdout, process.stderr), readers)
        fail(f"{label} exceeded {COMMAND_TIMEOUT_SECONDS} seconds")
    except BaseException:
        kill_process_tree(process, windows_job)
        if windows_job is not None:
            windows_job.close()
        try:
            process.wait(timeout=10)
        except subprocess.TimeoutExpired:
            pass
        for reader in readers:
            reader.join(timeout=2)
        close_finished_output_streams((process.stdout, process.stderr), readers)
        raise
    # The direct child has exited, but a compiler or generated program may have
    # left descendants behind. Kill the independent Unix process group or the
    # Windows Job before waiting for inherited output handles to close.
    kill_process_tree(process, windows_job)
    if windows_job is not None:
        windows_job.close()
    for reader in readers:
        reader.join(timeout=10)
    if any(reader.is_alive() for reader in readers):
        kill_process_tree(process, windows_job)
        for reader in readers:
            reader.join(timeout=2)
        close_finished_output_streams((process.stdout, process.stderr), readers)
        fail(f"{label} left a child process holding an output pipe")
    close_finished_output_streams((process.stdout, process.stderr), readers)
    stdout = bytes(output[0])
    stderr = bytes(output[1])
    if overflow.is_set():
        fail(f"{label} produced more than {MAX_CAPTURE_BYTES} bytes of output")
    if reader_errors:
        fail(f"{label} output reader failed: {'; '.join(reader_errors)}")
    if return_code != 0:
        stdout_text = stdout.decode("utf-8", errors="replace")
        stderr_text = stderr.decode("utf-8", errors="replace")
        fail(f"{label} exited with {return_code}\nstdout:\n{stdout_text}\nstderr:\n{stderr_text}")
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
