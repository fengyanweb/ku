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
from unittest.mock import MagicMock, patch

sys.dont_write_bytecode = True
SPEC = importlib.util.spec_from_file_location(
    "pg_fixture", Path(__file__).with_name("pg-loopback-fixture.py")
)
assert SPEC is not None and SPEC.loader is not None
FIXTURE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(FIXTURE)


class PgFixtureTests(unittest.TestCase):
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
        root.__truediv__.side_effect = {
            "verification.json": record,
            "fixture.json": manifest,
        }.__getitem__
        return SimpleNamespace(
            args=argparse.Namespace(fixture=root), target=target, record=record,
            manifest=manifest, state=state,
        )

    @classmethod
    def startup_inputs(cls) -> SimpleNamespace:
        # Reuse the existing invalidation contract without touching real fixtures.
        inputs = cls.mocked_verification_inputs()
        root = inputs.args.fixture
        inputs.pid = MagicMock(spec=Path)
        inputs.pid.exists.return_value = False
        data = MagicMock(spec=Path)
        data.__truediv__.side_effect = {"postmaster.pid": inputs.pid}.__getitem__
        password = MagicMock(spec=Path)
        password.read_text.return_value = "inert-contract-password"
        inputs.manifest.read_text.side_effect = None
        inputs.manifest.read_text.return_value = json.dumps({
            "format": 1, "version": FIXTURE.VERSION, "source_sha256": FIXTURE.SOURCE_SHA256,
            "port": 23456,
        })
        paths = {"verification.json": inputs.record, "fixture.json": inputs.manifest,
                 "data": data, "password.txt": password}
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
        with self.startup_scope(inputs), \
             patch.object(FIXTURE.subprocess, "Popen", side_effect=lambda *a, **k: events.append("popen") or process) as popen, \
             patch.object(FIXTURE.BOUNDS.WindowsJob, "attach", side_effect=lambda p: events.append("attach") or job), \
             patch.object(FIXTURE.BOUNDS, "resume_suspended_windows_process", side_effect=lambda p: events.append("resume")), \
             self.assertRaisesRegex(RuntimeError, "inert live boundary"):
            FIXTURE.verify(inputs.args)
        self.assertEqual(["popen", "attach", "resume", "wait"], events)
        self.assertTrue(popen.call_args.kwargs["creationflags"] & 4)
        process.wait.assert_called_once_with(timeout=25)
        job.terminate.assert_called_once_with()
        job.close.assert_called_once_with()
        inputs.tree.assert_not_called()
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
                process.wait.assert_called_once_with(timeout=5)
                inputs.record.open.assert_not_called()

    def test_pg_resume_startup_and_reap_failures_do_not_write_success(self) -> None:
        for stage in ("resume", "nonzero", "startup_timeout", "reap_timeout"):
            with self.subTest(stage=stage):
                inputs = self.startup_inputs()
                process, job = MagicMock(), MagicMock()
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
                self.assertEqual(2, job.terminate.call_count)  # second call is through retained closed Job
                job.close.assert_called_once_with()
                inputs.tree.assert_called_once_with(process, job)
                process.kill.assert_called_once_with()
                inputs.run.assert_not_called()
                inputs.numeric.assert_not_called()
                inputs.record.open.assert_not_called()

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
                        inputs.tree.assert_not_called()
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
        with patch.object(FIXTURE, "SECRET", "private-credential"):
            with patch.object(FIXTURE.BOUNDS, "run_bounded", side_effect=SystemExit("private-credential")):
                with self.assertRaisesRegex(RuntimeError, "^<redacted>$"):
                    FIXTURE.run([], FIXTURE.REPO, "fake", 2)
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
