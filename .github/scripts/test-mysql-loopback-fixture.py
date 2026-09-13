#!/usr/bin/env python3
"""MySQL fixture contracts; no database processes or connections are started."""

from __future__ import annotations

import argparse
from contextlib import ExitStack
import importlib.util
import io
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
from types import SimpleNamespace
import unittest
from unittest.mock import MagicMock, patch

sys.dont_write_bytecode = True
SPEC = importlib.util.spec_from_file_location(
    "ku_mysql_fixture", Path(__file__).with_name("mysql-loopback-fixture.py")
)
assert SPEC is not None and SPEC.loader is not None
F = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(F)

PASS = b"test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured\n"
UUID = "00000000-0000-0000-0000-000000000001"


class MysqlFixtureTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temporary = tempfile.TemporaryDirectory(prefix="ku-mysql-contract-")
        self.base = Path(self.temporary.name).resolve()
        self.target = self.base / "target"
        self.target.mkdir()
        self.root = self.target / (F.PREFIX + "0" * 32)
        self.root.mkdir()
        for name in ("tmp", "data", "portable/bin", "portable/lib", "portable/include", "portable/empty-plugins"):
            (self.root / name).mkdir(parents=True, exist_ok=True)
        self.config = {"host": "127.0.0.1", "port": 23456, "user": "ku_test_" + "1" * 16,
                       "password": "a" * 64, "database": "ku_db_" + "2" * 16}
        self.admin = {"user": "ku_admin_" + "3" * 16, "password": "b" * 64}
        F.write_json(self.root / "db.json", self.config)
        F.write_json(self.root / "admin.json", self.admin)
        F.write_json(self.root / "fixture.json", {"format": 1, "version": F.VERSION, "server_uuid": UUID})
        self.test = self.base / "test.exe"
        self.ku = self.base / "ku.exe"
        self.test.write_bytes(b"test fixture, not executable")
        self.ku.write_bytes(b"test fixture, not executable")
        self.args = argparse.Namespace(fixture=self.root, test_binary=self.test, ku_binary=self.ku)
        self.stack = ExitStack()
        self.stack.enter_context(patch.object(F, "TARGET", self.target))
        self.stack.enter_context(patch.object(F, "windows_only"))
        self.stack.enter_context(patch.object(F, "SECRETS", []))
        self.stack.enter_context(patch("sys.stdout", new_callable=io.StringIO))
        # Accidentally reaching the real bounded subprocess runner is forbidden.
        self.real_run = self.stack.enter_context(patch.object(F.BOUNDS, "run_bounded", side_effect=AssertionError("unexpected process execution")))

    def tearDown(self) -> None:
        self.stack.close()
        self.temporary.cleanup()

    def fake_server(self) -> MagicMock:
        server = MagicMock()
        server.failed.is_set.return_value = False
        server.process.poll.return_value = None
        server.process.wait.return_value = 0
        return server

    def verification(self, *, server: MagicMock | None = None, output: bytes = PASS, identity: bytes | None = None) -> tuple[ExitStack, MagicMock, MagicMock]:
        stack = ExitStack()
        server = server or self.fake_server()
        stack.enter_context(patch.object(F, "MysqlProcess", return_value=server))
        fake_socket = MagicMock()
        fake_socket.SOL_SOCKET = 1
        fake_socket.SO_EXCLUSIVEADDRUSE = 2
        stack.enter_context(patch.object(F, "socket", fake_socket))
        identity = identity if identity is not None else f"{UUID}\t{self.root / 'data'}\t23456\n".encode()

        def command_result(command: list[str], cwd: Path, label: str, timeout: int = 30) -> bytes:
            if "identity" in label:
                return identity
            if "acceptance" in label:
                return output
            if "stop" in label:
                return b""
            raise AssertionError("unexpected subprocess command")

        run = stack.enter_context(patch.object(F, "run", side_effect=command_result))
        return stack, server, run

    def test_environment_strips_database_credentials_and_disables_login_path(self) -> None:
        original = {"PATH": "trusted compiler", "MYSQL_PWD": "not-used", "MYSQL_HOST": "business",
                    "MYSQL_TEST_LOGIN_FILE": "not-read", "PGPASSFILE": "not-read",
                    "KU_MYSQL_LIB": "not-used", "AWS_SECRET_ACCESS_KEY": "not-used"}
        with patch.dict(os.environ, original, clear=True):
            with F.isolated_environment(self.root):
                self.assertNotIn("MYSQL_PWD", os.environ)
                self.assertNotIn("PGPASSFILE", os.environ)
                self.assertNotIn("KU_MYSQL_LIB", os.environ)
                self.assertNotIn("AWS_SECRET_ACCESS_KEY", os.environ)
                self.assertEqual(str(self.root / "never-created.mylogin.cnf"), os.environ["MYSQL_TEST_LOGIN_FILE"])
                self.assertEqual(str(self.root / "tmp"), os.environ["TEMP"])
            self.assertEqual(original, dict(os.environ))

    def test_environment_restored_when_operation_raises(self) -> None:
        previous = dict(os.environ)
        with self.assertRaisesRegex(RuntimeError, "fixture fault"):
            with F.isolated_environment(self.root):
                raise RuntimeError("fixture fault")
        self.assertEqual(previous, dict(os.environ))

    def test_environment_preserves_only_nonsecret_msvc_discovery_roots(self) -> None:
        compiler = {"ProgramFiles(x86)": "C:/Program Files (x86)",
                    "ProgramFiles": "C:/Program Files", "ProgramW6432": "C:/Program Files"}
        with patch.dict(os.environ, dict(compiler, MYSQL_PWD="unused", DATABASE_URL="unused"), clear=True):
            with F.isolated_environment(self.root):
                for key, value in compiler.items():
                    self.assertEqual(value, os.environ[key])
                self.assertNotIn("MYSQL_PWD", os.environ)
                self.assertNotIn("DATABASE_URL", os.environ)

    def test_existing_private_login_path_is_rejected(self) -> None:
        (self.root / "never-created.mylogin.cnf").write_bytes(b"not a credential")
        with self.assertRaisesRegex(ValueError, "must not exist"):
            with F.isolated_environment(self.root):
                self.fail("existing login path accepted")

    def test_generated_config_rejects_remote_bool_port_and_injection(self) -> None:
        for field, value in (("host", "example.invalid"), ("host", "localhost"), ("port", True),
                             ("port", 0), ("port", 65536), ("user", "root"),
                             ("database", "mysql"), ("password", "bad' SQL")):
            config = dict(self.config, **{field: value})
            with self.subTest(field=field), self.assertRaises(ValueError):
                F.validate_config(config, self.admin)
        with self.assertRaises(ValueError):
            F.validate_config(dict(self.config, socket="business"), self.admin)

    def test_json_bound_duplicates_and_shape(self) -> None:
        path = self.root / "input.json"
        for value in (b'{"x":1,"x":2}', b'[]', b'\xff', b'"' + b'x' * F.MAX_JSON_BYTES + b'"'):
            path.write_bytes(value)
            with self.assertRaises(ValueError):
                F.read_json(path)

    def test_nested_json_is_rejected_without_a_raw_recursion_error(self) -> None:
        path = self.root / "input.json"
        path.write_bytes(b"[" * 2000 + b"0" + b"]" * 2000)
        with self.assertRaises(ValueError):
            F.read_json(path)

    def test_fixture_path_scope_rejected_before_process(self) -> None:
        with self.assertRaisesRegex(ValueError, "dedicated"):
            F.verify(argparse.Namespace(fixture=self.base))
        self.real_run.assert_not_called()

    def test_operation_lock_excludes_parallel_verification(self) -> None:
        with F.operation(self.root):
            with self.assertRaises(FileExistsError):
                F.verify(self.args)
        self.assertFalse((self.root / "operation.lock").exists())

    def test_unconfirmed_pid_preserves_operation_marker(self) -> None:
        with F.operation(self.root):
            (self.root / "server.pid").write_text("not a process", encoding="ascii")
        self.assertTrue((self.root / "operation.lock").exists())

    def test_startup_without_pid_still_blocks_reuse_and_cleanup(self) -> None:
        with F.operation(self.root):
            (self.root / "server.active").write_bytes(b"owned process not confirmed stopped")
        self.assertTrue((self.root / "operation.lock").exists())
        with self.assertRaises(ValueError):
            F.cleanup(self.args)

    def test_server_and_client_options_cannot_load_existing_defaults(self) -> None:
        command = F.server_command(self.root, self.config["port"])
        self.assertEqual("--no-defaults", command[1])
        for required in ("--no-monitor", "--mysqlx=OFF", "--bind-address=127.0.0.1", "--port-open-timeout=0",
                         "--persisted-globals-load=OFF", "--local-infile=OFF", "--shared-memory=OFF"):
            self.assertIn(required, command)
        self.assertFalse(any(arg in ("--install", "--skip-grant-tables", "--initialize-insecure") for arg in command))
        self.assertIn(str(self.root / "admin.cnf"), F.client_command(self.root, "mysql.exe")[1])
        self.assertFalse(any(self.config["password"] in arg for arg in command))

    def test_startup_sql_scopes_test_permissions_and_locks_root(self) -> None:
        F.write_startup_files(self.root, self.config, self.admin)
        sql = (self.root / "init.sql").read_text(encoding="utf-8")
        self.assertIn("ACCOUNT LOCK", sql)
        grant_database = self.config["database"].replace("_", r"\_")
        self.assertIn(f"ON `{grant_database}`.*", sql)
        self.assertNotIn(f"ON `{self.config['database']}`.*", sql)
        self.assertIn(f"CREATE DATABASE IF NOT EXISTS `{self.config['database']}`", sql)
        self.assertNotIn("GRANT ALL", sql)
        self.assertNotIn("FILE,", sql)
        self.assertNotIn("CREATE ROUTINE", sql)
        self.assertIn("GRANT SHUTDOWN ON *.*", sql)
        self.assertEqual(2, sql.count("CREATE USER IF NOT EXISTS"))

    def test_bounded_runner_suppresses_unknown_initializer_password(self) -> None:
        previous = F.BOUNDS.COMMAND_TIMEOUT_SECONDS
        with patch.object(F.BOUNDS, "run_bounded", side_effect=SystemExit("unrecognized-temporary-credential")):
            with self.assertRaises(RuntimeError) as caught:
                F.run([], self.root, "initialize fixture", 7)
        self.assertNotIn("unrecognized-temporary-credential", str(caught.exception))
        self.assertEqual(previous, F.BOUNDS.COMMAND_TIMEOUT_SECONDS)

    def test_failed_command_keeps_bounded_diagnostic_only_inside_private_root(self) -> None:
        with F.isolated_environment(self.root), patch.object(F, "MAX_SERVER_LOG_BYTES", 16), \
             patch.object(F.BOUNDS, "run_bounded", side_effect=SystemExit("private" * 100)):
            with self.assertRaises(RuntimeError) as caught:
                F.run([], self.root, "controlled failure")
        self.assertLessEqual((self.root / "command-error.log").stat().st_size, 16)
        self.assertNotIn("privateprivate", str(caught.exception))
        self.assertIsNone(F.PRIVATE_LOG_ROOT)

    def test_live_result_rejects_skips_and_wrong_counts(self) -> None:
        self.assertIn("1 passed", F.require_live_pass(PASS))
        for output in (b"", PASS + b"SKIP: unavailable", PASS.replace(b"1 passed", b"11 passed"),
                       PASS.replace(b"0 ignored", b"1 ignored"), PASS.replace(b"0 failed", b"1 failed")):
            with self.assertRaises(RuntimeError):
                F.require_live_pass(output)

    def test_live_output_redacts_generated_password(self) -> None:
        with patch.object(F, "SECRETS", [self.config["password"]]):
            text = F.require_live_pass(PASS + self.config["password"].encode())
        self.assertNotIn(self.config["password"], text)
        self.assertIn("<redacted>", text)

    def test_verify_success_uses_exact_test_and_records_stopped_server(self) -> None:
        stack, server, run = self.verification()
        with stack:
            F.verify(self.args)
        server.start.assert_called_once()
        server.wait_ready.assert_called_once()
        server.stop.assert_called_once()
        test_command = run.call_args_list[1].args[0]
        self.assertEqual([str(self.test), "--exact", F.LIVE_TEST, "--ignored", "--nocapture", "--test-threads=1"], test_command)
        record = F.read_json(self.root / "verification.json")
        self.assertIs(record["live_test_passed"], True)
        self.assertIs(record["server_stopped"], True)
        self.assertFalse((self.root / "init.sql").exists())
        self.assertFalse((self.root / "admin.cnf").exists())

    def test_port_conflict_never_starts_or_connects(self) -> None:
        stack, server, run = self.verification()
        with stack:
            F.socket.socket.return_value.__enter__.return_value.bind.side_effect = OSError("occupied")
            with self.assertRaises(OSError):
                F.verify(self.args)
        server.start.assert_not_called()
        run.assert_not_called()

    def test_start_or_readiness_failure_never_connects_and_always_stops(self) -> None:
        for step in ("start", "wait_ready"):
            with self.subTest(step=step):
                server = self.fake_server()
                getattr(server, step).side_effect = RuntimeError("controlled failure")
                stack, _, run = self.verification(server=server)
                with stack, self.assertRaises(RuntimeError):
                    F.verify(self.args)
                server.stop.assert_called_once()
                run.assert_not_called()
                self.assertFalse((self.root / "verification.json").exists())

    def test_identity_mismatch_stops_before_live_test(self) -> None:
        stack, server, run = self.verification(identity=b"wrong\twrong\t23456\n")
        with stack, self.assertRaisesRegex(RuntimeError, "identity"):
            F.verify(self.args)
        self.assertEqual(1, run.call_count)
        server.stop.assert_called_once()
        self.assertFalse((self.root / "verification.json").exists())

    def test_failed_or_skipped_test_invalidates_success_and_stops(self) -> None:
        F.write_json(self.root / "verification.json", {"old": "success"})
        stack, server, _ = self.verification(output=PASS + b"skip: compiler")
        with stack, self.assertRaises(RuntimeError):
            F.verify(self.args)
        server.stop.assert_called_once()
        self.assertFalse((self.root / "verification.json").exists())

    def test_failed_shutdown_never_records_success(self) -> None:
        stack, server, run = self.verification()
        with stack:
            run.side_effect = [f"{UUID}\t{self.root / 'data'}\t23456".encode(), PASS, RuntimeError("shutdown failure")]
            with self.assertRaises(RuntimeError):
                F.verify(self.args)
        server.stop.assert_called_once()
        self.assertFalse((self.root / "verification.json").exists())

    def test_suspended_process_requires_job_before_resume(self) -> None:
        process = MagicMock()
        process.poll.return_value = 0
        with patch.object(F.subprocess, "Popen", return_value=process) as popen, \
             patch.object(F.BOUNDS.WindowsJob, "attach", return_value=None), \
             patch.object(F.BOUNDS, "resume_suspended_windows_process") as resume, \
             patch.object(F.BOUNDS, "kill_process_tree") as kill:
            with self.assertRaisesRegex(RuntimeError, "contain"):
                F.MysqlProcess(self.root, ["not executed"]).start()
        self.assertTrue(popen.call_args.kwargs["creationflags"] & 0x4)
        self.assertTrue(popen.call_args.kwargs["creationflags"] & 0x08000000)
        resume.assert_not_called()
        kill.assert_called_once()
        process.wait.assert_called_once()
        self.assertGreaterEqual(process.wait.call_args.kwargs["timeout"], 0)
        self.assertLessEqual(process.wait.call_args.kwargs["timeout"], 10)

    def test_job_setup_exception_reaps_suspended_child(self) -> None:
        process = MagicMock()
        with patch.object(F.subprocess, "Popen", return_value=process), \
             patch.object(F.BOUNDS.WindowsJob, "attach", side_effect=OSError("job failure")), \
             patch.object(F.BOUNDS, "kill_process_tree") as kill:
            with self.assertRaises(OSError):
                F.MysqlProcess(self.root, ["not executed"]).start()
        kill.assert_called_once()
        process.wait.assert_called_once()
        self.assertGreaterEqual(process.wait.call_args.kwargs["timeout"], 0)
        self.assertLessEqual(process.wait.call_args.kwargs["timeout"], 10)

    def test_no_job_failure_paths_use_actual_shared_held_handle_cleanup(self) -> None:
        for stage in ("none", "exception", "wait_timeout"):
            with self.subTest(stage=stage):
                process = MagicMock()
                process.poll.return_value = None
                if stage == "wait_timeout":
                    process.wait.side_effect = subprocess.TimeoutExpired("held fixture root", 10)
                attach_error = OSError("assignment setup failed") if stage == "exception" else None
                expected = OSError if stage == "exception" else RuntimeError
                try:
                    with F.operation(self.root), \
                         patch.object(F.subprocess, "Popen", return_value=process), \
                         patch.object(F.BOUNDS.WindowsJob, "attach", return_value=None, side_effect=attach_error), \
                         patch.object(F.BOUNDS, "os", SimpleNamespace(name="nt")), \
                         patch.object(F.BOUNDS, "resume_suspended_windows_process") as resume, \
                         patch.object(F.BOUNDS, "kill_process_tree", wraps=F.BOUNDS.kill_process_tree) as held_cleanup, \
                         patch.object(F.subprocess, "run", side_effect=AssertionError("numeric tree forbidden")) as numeric, \
                         self.assertRaises(expected) as caught:
                        F.MysqlProcess(self.root, ["not executed"]).start()
                    resume.assert_not_called()
                    held_cleanup.assert_called_once_with(process, None)
                    numeric.assert_not_called()
                    process.kill.assert_called_once_with()
                    process.wait.assert_called_once()
                    self.assertGreaterEqual(process.wait.call_args.kwargs["timeout"], 0)
                    self.assertLessEqual(process.wait.call_args.kwargs["timeout"], 10)
                    retained = stage == "wait_timeout"
                    self.assertEqual(retained, (self.root / "server.active").exists())
                    self.assertEqual(retained, (self.root / "operation.lock").exists())
                    if retained:
                        self.assertIn("contain", str(caught.exception))
                        self.assertIn("TimeoutExpired", str(caught.exception))
                        # No reader ever started, so even a failed reap does
                        # not leave a BufferedReader lock preventing close.
                        process.stdout.close.assert_called_once()
                finally:
                    # Only inert test-owned leaves: isolate later subtests even
                    # when a red assertion prevents the production cleanup.
                    (self.root / "server.active").unlink(missing_ok=True)
                    (self.root / "operation.lock").unlink(missing_ok=True)

    @unittest.skipUnless(os.name == "nt", "real harmless held Windows root")
    def test_no_job_real_suspended_root_uses_actual_shared_helper(self) -> None:
        real_popen = subprocess.Popen
        held = []
        marker = self.root / "harmless-child-executed"
        child = "from pathlib import Path; Path(" + repr(str(marker)) + ").write_text('inert', encoding='ascii')"

        def launch(*args, **kwargs):
            process = real_popen(*args, **kwargs)
            held.append(process)
            return process

        server = F.MysqlProcess(self.root, [sys.executable, "-B", "-c", child])
        try:
            with patch.object(F.subprocess, "Popen", side_effect=launch) as popen, \
                 patch.object(F.BOUNDS.WindowsJob, "attach", return_value=None), \
                 patch.object(F.BOUNDS, "resume_suspended_windows_process") as resume, \
                 patch.object(F.BOUNDS, "kill_process_tree", wraps=F.BOUNDS.kill_process_tree) as held_cleanup, \
                 patch.object(F.subprocess, "run", side_effect=AssertionError("numeric tree forbidden")) as numeric, \
                 self.assertRaisesRegex(RuntimeError, "contain"):
                server.start()
            self.assertEqual(1, len(held))
            self.assertTrue(popen.call_args.kwargs["creationflags"] & 4)
            self.assertIsNotNone(held[0].poll())
            self.assertIsNone(server.reader)
            self.assertTrue(held[0].stdout.closed)
            self.assertFalse(marker.exists())
            self.assertFalse((self.root / "server.active").exists())
            resume.assert_not_called()
            held_cleanup.assert_called_once_with(held[0], None)
            numeric.assert_not_called()
        finally:
            # Assignment failure is injected; root creation/kill/wait are real.
            # This harmless child cannot spawn a descendant or touch a database.
            for process in held:
                if process.poll() is None:
                    process.kill()
                process.wait(timeout=5)
                process.stdout.close()

    def test_process_creation_failure_does_not_leave_false_active_marker(self) -> None:
        with patch.object(F.subprocess, "Popen", side_effect=FileNotFoundError("fixture executable")):
            with self.assertRaises(FileNotFoundError):
                F.MysqlProcess(self.root, ["not executed"]).start()
        self.assertFalse((self.root / "server.active").exists())

    def test_server_output_has_hard_byte_bound(self) -> None:
        server = F.MysqlProcess(self.root, [])
        server.process = SimpleNamespace(stdout=io.BytesIO(b"x" * 17))
        with patch.object(F, "MAX_SERVER_LOG_BYTES", 16), patch.object(F.BOUNDS, "kill_process_tree") as kill:
            server.drain()
        self.assertTrue(server.failed.is_set())
        self.assertEqual(16, len(server.output))
        self.assertEqual(16, (self.root / "server.log").stat().st_size)
        kill.assert_called_once()

    def test_readiness_needs_owned_pid_and_has_deadline(self) -> None:
        server = F.MysqlProcess(self.root, [])
        server.process = MagicMock(pid=123)
        server.process.poll.return_value = None
        server.output.extend(b"ready for connections")
        (self.root / "server.pid").write_text("456", encoding="ascii")
        with self.assertRaisesRegex(RuntimeError, "pid"):
            server.wait_ready()
        (self.root / "server.pid").unlink()
        with patch.object(F.time, "monotonic", side_effect=[0, F.START_TIMEOUT + 1]), \
             self.assertRaisesRegex(RuntimeError, "deadline"):
            server.wait_ready()

    def test_stop_timeout_keeps_pid_marker_and_closes_job(self) -> None:
        server = F.MysqlProcess(self.root, [])
        server.process = MagicMock()
        server.process.wait.side_effect = subprocess.TimeoutExpired("fixture", 10)
        server.job = MagicMock()
        server.job.wait_empty.return_value = True
        (self.root / "server.pid").write_text("123", encoding="ascii")
        (self.root / "server.active").write_bytes(b"owned server")
        with patch.object(F.BOUNDS, "kill_process_tree"), self.assertRaises(subprocess.TimeoutExpired):
            server.stop()
        self.assertTrue((self.root / "server.pid").exists())
        self.assertTrue((self.root / "server.active").exists())
        server.job.close.assert_called_once()

    def inert_process(self) -> tuple[object, object, object]:
        # No numeric pid: cleanup may operate only on this retained owner.
        stream = MagicMock(spec=["read", "close"])
        process = SimpleNamespace(stdout=stream, poll=MagicMock(return_value=None),
                                  kill=MagicMock(), wait=MagicMock(return_value=0))
        job = SimpleNamespace(terminate=MagicMock(), wait_empty=MagicMock(return_value=True), close=MagicMock())
        reader = SimpleNamespace(join=MagicMock(), is_alive=MagicMock(return_value=False), ident=1)
        return process, job, reader

    def marked_server(self) -> object:
        server = F.MysqlProcess(self.root, [])
        server.process, server.job, server.reader = self.inert_process()
        server.reader_start_state = "started"
        for name in ("server.pid", "server.active"):
            (self.root / name).write_bytes(b"inert retained marker")
        return server

    def test_stop_requires_exact_confirmed_job_drain_before_removing_markers(self) -> None:
        for result in (None, False, 1, "true", MagicMock()):
            with self.subTest(result_type=type(result).__name__):
                server = self.marked_server()
                server.job.wait_empty = MagicMock(return_value=result)
                with patch.object(F.BOUNDS, "kill_process_tree"), \
                     self.assertRaisesRegex(RuntimeError, "job_drain"):
                    server.stop()
                server.job.wait_empty.assert_called_once()
                server.process.wait.assert_called_once()
                server.job.close.assert_called_once()
                server.reader.join.assert_called_once()
                server.process.stdout.close.assert_called_once()
                self.assertFalse(server.stop_complete)
                self.assertTrue((self.root / "server.pid").exists())
                self.assertTrue((self.root / "server.active").exists())

    def test_stop_preserves_first_error_and_finishes_independent_steps_after_job_drain_error(self) -> None:
        for first in (None, RuntimeError("root wait primary")):
            with self.subTest(with_primary=first is not None):
                server = self.marked_server()
                server.process.wait.side_effect = first
                server.job.wait_empty = MagicMock(side_effect=OSError("drain private detail"))
                with patch.object(F.BOUNDS, "kill_process_tree"), self.assertRaises(RuntimeError) as caught:
                    server.stop()
                server.job.wait_empty.assert_called_once()
                server.job.close.assert_called_once()
                server.reader.join.assert_called_once()
                server.process.stdout.close.assert_called_once()
                self.assertIn("job_drain", str(caught.exception))
                if first is not None:
                    self.assertIn("root wait primary", str(caught.exception))
                    self.assertNotIn("drain private detail", str(caught.exception))
                self.assertFalse(server.stop_complete)
                self.assertTrue((self.root / "server.active").exists())

    def test_stop_drains_before_close_using_existing_absolute_cleanup_deadline(self) -> None:
        server = self.marked_server()
        order = []
        server.process.wait.side_effect = lambda **_: order.append("root_wait")
        server.job.wait_empty = MagicMock(side_effect=lambda deadline: order.append(("drain", deadline)) or True)
        server.job.close.side_effect = lambda: order.append("job_close")
        server.reader.join.side_effect = lambda **_: order.append("reader_join")
        with patch.object(F.BOUNDS, "kill_process_tree"), \
             patch.object(F.time, "monotonic", side_effect=[100.0, 101.0, 109.0]):
            server.stop()
        self.assertEqual(["root_wait", ("drain", 110.0), "job_close", "reader_join"], order)
        self.assertEqual(9.0, server.process.wait.call_args.kwargs["timeout"])
        self.assertEqual(1.0, server.reader.join.call_args.kwargs["timeout"])
        self.assertTrue(server.stop_complete)
        self.assertFalse((self.root / "server.active").exists())

    def test_stop_close_failure_after_confirmed_drain_cannot_complete(self) -> None:
        server = self.marked_server()
        job = server.job
        job.wait_empty = MagicMock(return_value=True)
        job.close.side_effect = OSError("close primary")
        with patch.object(F.BOUNDS, "kill_process_tree"), self.assertRaisesRegex(OSError, "close primary"):
            server.stop()
        job.wait_empty.assert_called_once()
        self.assertIs(server.job, job)
        self.assertFalse(server.stop_complete)
        self.assertTrue(server.cleanup_uncertain)
        self.assertTrue((self.root / "server.active").exists())
        server.reader.join.assert_called_once()

    def test_verify_start_failure_double_stop_does_not_requery_completed_owner(self) -> None:
        server = F.MysqlProcess(self.root, ["not executed"])
        process, job, reader = self.inert_process()
        reader.start = MagicMock()
        job.wait_empty = MagicMock(side_effect=[True, RuntimeError("closed owner cannot be queried")])
        stack, _, run = self.verification(server=server)
        with stack, patch.object(F.subprocess, "Popen", return_value=process), \
             patch.object(F.BOUNDS.WindowsJob, "attach", return_value=job), \
             patch.object(F.threading, "Thread", return_value=reader), \
             patch.object(F.BOUNDS, "os", SimpleNamespace(name="nt")), \
             patch.object(F.BOUNDS, "resume_suspended_windows_process", side_effect=OSError("resume primary")), \
             patch.object(server, "stop", wraps=server.stop) as stop, \
             self.assertRaisesRegex(OSError, "resume primary"):
            F.verify(self.args)
        self.assertEqual(2, stop.call_count)
        job.wait_empty.assert_called_once()
        job.close.assert_called_once()
        process.wait.assert_called_once()
        reader.join.assert_called_once()
        process.stdout.close.assert_called_once()
        run.assert_not_called()
        self.assertTrue(server.stop_complete)
        self.assertIs(server.job, job)
        for name in ("server.active", "server.pid", "init.sql", "admin.cnf", "verification.json"):
            self.assertFalse((self.root / name).exists(), name)

    def test_same_supervisor_cannot_restart_an_existing_or_completed_owner(self) -> None:
        for completed in (False, True):
            with self.subTest(completed=completed):
                server = self.marked_server()
                process = server.process
                if completed:
                    with patch.object(F.BOUNDS, "kill_process_tree"):
                        server.stop()
                with patch.object(F.subprocess, "Popen") as popen, \
                     self.assertRaisesRegex(RuntimeError, "cannot be restarted"):
                    server.start()
                popen.assert_not_called()
                self.assertIs(server.process, process)
                self.assertIs(server.stop_complete, completed)

    def test_marker_unlink_failure_does_not_latch_completed_cleanup(self) -> None:
        server = self.marked_server()
        unlink = Path.unlink

        def fail_active_marker(path, *args, **kwargs):
            if path == self.root / "server.active":
                raise OSError("active marker unlink primary")
            return unlink(path, *args, **kwargs)

        with patch.object(F.BOUNDS, "kill_process_tree"), patch.object(Path, "unlink", fail_active_marker), \
             self.assertRaisesRegex(OSError, "active marker unlink primary"):
            server.stop()
        self.assertFalse(server.stop_complete)
        self.assertTrue(server.cleanup_uncertain)
        self.assertTrue((self.root / "server.active").exists())
        with patch.object(F.BOUNDS, "kill_process_tree"), \
             self.assertRaisesRegex(RuntimeError, "previous_cleanup_failure"):
            server.stop()
        self.assertTrue((self.root / "server.active").exists())

    def test_verify_uncertain_job_drain_rejects_receipt_and_still_removes_secrets(self) -> None:
        server = F.MysqlProcess(self.root, [])
        server.process, server.job, server.reader = self.inert_process()
        server.reader_start_state = "started"
        server.job.wait_empty.side_effect = OSError("private query detail")
        server.start = MagicMock(side_effect=lambda: (self.root / "server.active").write_bytes(b"inert owned marker"))
        server.wait_ready = MagicMock()
        stack, _, _ = self.verification(server=server)
        with stack, patch.object(F.BOUNDS, "os", SimpleNamespace(name="nt")), \
             self.assertRaisesRegex(RuntimeError, "job_drain"):
            F.verify(self.args)
        server.job.wait_empty.assert_called_once()
        server.job.close.assert_called_once()
        server.reader.join.assert_called_once()
        self.assertFalse(server.stop_complete)
        for name in ("verification.json", "init.sql", "admin.cnf"):
            self.assertFalse((self.root / name).exists(), name)
        for name in ("server.active", "operation.lock", "db.json", "admin.json", "data"):
            self.assertTrue((self.root / name).exists(), name)

    def test_verify_second_stop_cannot_erase_first_job_drain_uncertainty(self) -> None:
        server = F.MysqlProcess(self.root, ["not executed"])
        process, job, reader = self.inert_process()
        reader.start = MagicMock()
        # Even an inert second observation claiming success cannot erase the
        # first failed query. Actual closed Job owners must reject a query.
        job.wait_empty.side_effect = [OSError("query private detail"), True]
        stack, _, run = self.verification(server=server)
        with stack, patch.object(F.subprocess, "Popen", return_value=process), \
             patch.object(F.BOUNDS.WindowsJob, "attach", return_value=job), \
             patch.object(F.threading, "Thread", return_value=reader), \
             patch.object(F.BOUNDS, "os", SimpleNamespace(name="nt")), \
             patch.object(F.BOUNDS, "resume_suspended_windows_process", side_effect=OSError("resume primary")), \
             self.assertRaises(RuntimeError) as caught:
            F.verify(self.args)
        self.assertEqual(2, job.wait_empty.call_count)
        self.assertIn("resume primary", str(caught.exception))
        self.assertIn("previous_cleanup_failure", str(caught.exception))
        self.assertNotIn("query private detail", str(caught.exception))
        self.assertFalse(server.stop_complete)
        self.assertTrue(server.cleanup_uncertain)
        self.assertTrue((self.root / "server.active").exists())
        run.assert_not_called()
        for name in ("init.sql", "admin.cnf", "verification.json"):
            self.assertFalse((self.root / name).exists(), name)

    def test_stop_faults_attempt_all_independent_cleanup_and_retain_markers(self) -> None:
        for fault in ("terminate", "wait", "job_close", "reader_join", "stream_close", "late_read"):
            with self.subTest(fault=fault):
                server = self.marked_server()
                methods = {"terminate": server.job.terminate, "wait": server.process.wait,
                           "job_close": server.job.close, "reader_join": server.reader.join,
                           "stream_close": server.process.stdout.close}
                if fault == "late_read":
                    server.reader.join.side_effect = lambda **_: server.failed.set()
                else:
                    methods[fault].side_effect = RuntimeError(fault + " primary")
                with patch.object(F.BOUNDS, "os", SimpleNamespace(name="nt")), \
                     self.assertRaises(RuntimeError) as caught:
                    server.stop()
                server.job.terminate.assert_called_once()
                server.process.kill.assert_called_once()
                server.process.wait.assert_called_once()
                server.job.close.assert_called_once()
                server.reader.join.assert_called_once()
                if fault == "reader_join":
                    server.process.stdout.close.assert_not_called()
                else:
                    server.process.stdout.close.assert_called_once()
                if fault != "late_read":
                    category = "Job termination" if fault == "terminate" else fault
                    self.assertIn(category, str(caught.exception))
                self.assertTrue((self.root / "server.pid").exists())
                self.assertTrue((self.root / "server.active").exists())

    def test_stop_keeps_first_failure_and_bounded_secondary_categories(self) -> None:
        server = self.marked_server()
        server.process.wait.side_effect = RuntimeError("wait primary")
        server.job.close.side_effect = RuntimeError("private-close-detail" * 10000)
        server.process.stdout.close.side_effect = RuntimeError("private-stream-detail" * 10000)
        with patch.object(F.BOUNDS, "kill_process_tree"), self.assertRaises(RuntimeError) as caught:
            server.stop()
        text = str(caught.exception)
        self.assertTrue("wait primary" in text, "first failure was replaced")
        self.assertIn("job_close", text)
        self.assertIn("stream_close", text)
        self.assertNotIn("private-close-detail", text)
        self.assertLess(len(text), 2048)
        server.reader.join.assert_called_once()
        server.process.stdout.close.assert_called_once()

    def test_stop_never_closes_a_live_reader_even_after_other_cleanup_fault(self) -> None:
        server = self.marked_server()
        server.reader.is_alive.return_value = True
        server.job.close.side_effect = RuntimeError("job_close primary")
        with patch.object(F.BOUNDS, "kill_process_tree"), self.assertRaises(RuntimeError):
            server.stop()
        server.reader.join.assert_called_once()
        server.process.stdout.close.assert_not_called()
        self.assertTrue((self.root / "server.active").exists())

    def test_started_reader_join_failure_does_not_authorize_close_from_alive_flag(self) -> None:
        server = self.marked_server()
        server.reader.join.side_effect = KeyboardInterrupt("inert join interruption")
        with patch.object(F.BOUNDS, "kill_process_tree"), self.assertRaises(BaseException):
            server.stop()
        server.process.stdout.close.assert_not_called()
        self.assertTrue((self.root / "server.active").exists())

    def test_stop_uses_one_absolute_budget_for_wait_and_reader_join(self) -> None:
        server = self.marked_server()
        with patch.object(F.BOUNDS, "kill_process_tree"), \
             patch.object(F.time, "monotonic", side_effect=[100.0, 101.0, 109.0]):
            server.stop()
        self.assertEqual(9.0, server.process.wait.call_args.kwargs["timeout"])
        self.assertEqual(1.0, server.reader.join.call_args.kwargs["timeout"])

    def test_drain_termination_error_reaches_supervisor_without_escaping(self) -> None:
        for mode in ("overflow", "read_error"):
            with self.subTest(mode=mode):
                (self.root / "server.log").unlink(missing_ok=True)
                server = self.marked_server()
                if mode == "overflow":
                    server.process.stdout.read.side_effect = [b"x" * 17, b""]
                else:
                    server.process.stdout.read.side_effect = OSError("private-read-detail")
                with patch.object(F, "MAX_SERVER_LOG_BYTES", 16), \
                     patch.object(F.BOUNDS, "kill_process_tree", side_effect=RuntimeError("private-kill-detail")) as kill:
                    server.drain()
                kill.assert_called_once()
                self.assertTrue(server.failed.is_set())
                self.assertLessEqual(len(server.output), 16)
                self.assertLessEqual((self.root / "server.log").stat().st_size, 16)
                with patch.object(F.BOUNDS, "kill_process_tree"), self.assertRaises(RuntimeError) as caught:
                    server.stop()
                self.assertIn("reader_terminate", str(caught.exception))
                self.assertNotIn("private-kill-detail", str(caught.exception))
                self.assertNotIn("private-read-detail", str(caught.exception))
                self.assertTrue((self.root / "server.active").exists())

    def test_verify_stop_failure_still_removes_both_generated_secret_files(self) -> None:
        stack, server, _ = self.verification()
        server.start.side_effect = lambda: (self.root / "server.active").write_bytes(b"uncertain")
        server.stop.side_effect = RuntimeError("stop primary")
        with stack, self.assertRaisesRegex(RuntimeError, "stop"):
            F.verify(self.args)
        for name in ("init.sql", "admin.cnf", "verification.json"):
            self.assertFalse((self.root / name).exists(), name)
        for name in ("db.json", "admin.json", "data", "server.active", "operation.lock"):
            self.assertTrue((self.root / name).exists(), name)

    def test_verify_first_unlink_failure_still_attempts_second_and_keeps_primary(self) -> None:
        stack, server, _ = self.verification(output=PASS + b"skip: fixture")
        server.stop.side_effect = RuntimeError("stop private detail")
        original_unlink = Path.unlink
        seen = []

        def unlink(path, *args, **kwargs):
            if path.name in ("init.sql", "admin.cnf"):
                seen.append(path.name)
            if path == self.root / "init.sql":
                raise OSError("unlink private detail")
            return original_unlink(path, *args, **kwargs)

        with stack, patch.object(Path, "unlink", unlink), self.assertRaises(RuntimeError) as caught:
            F.verify(self.args)
        self.assertEqual(["init.sql", "admin.cnf"], seen)
        self.assertIn("exactly one", str(caught.exception))
        self.assertIn("server_stop", str(caught.exception))
        self.assertIn("init_sql_unlink", str(caught.exception))
        self.assertNotIn("stop private detail", str(caught.exception))
        self.assertFalse((self.root / "admin.cnf").exists())
        self.assertFalse((self.root / "verification.json").exists())

    def test_start_keeps_setup_failure_when_stop_also_fails(self) -> None:
        server = F.MysqlProcess(self.root, ["not executed"])
        process, _, _ = self.inert_process()
        with patch.object(F.subprocess, "Popen", return_value=process), \
             patch.object(F.BOUNDS.WindowsJob, "attach", side_effect=OSError("setup primary")), \
             patch.object(server, "stop", side_effect=RuntimeError("cleanup private detail")), \
             self.assertRaises(RuntimeError) as caught:
            server.start()
        self.assertIn("setup primary", str(caught.exception))
        self.assertIn("server_stop", str(caught.exception))
        self.assertNotIn("cleanup private detail", str(caught.exception))
        self.assertTrue((self.root / "server.active").exists())

    def test_start_retains_failed_attach_owner_and_does_not_resume(self) -> None:
        server = F.MysqlProcess(self.root, ["not executed"])
        process, job, _ = self.inert_process()
        fault = F.BOUNDS.WindowsJobSetupError(job, OSError("setup private detail"))
        with patch.object(F.subprocess, "Popen", return_value=process), \
             patch.object(F.BOUNDS.WindowsJob, "attach", side_effect=fault), \
             patch.object(F.BOUNDS, "os", SimpleNamespace(name="nt")), \
             patch.object(F.BOUNDS, "resume_suspended_windows_process") as resume, \
             self.assertRaises(F.BOUNDS.WindowsJobSetupError) as caught:
            server.start()
        self.assertIs(fault, caught.exception)
        self.assertIs(server.job, job)
        job.terminate.assert_called_once()
        job.close.assert_called_once()
        process.kill.assert_called_once()
        process.wait.assert_called_once()
        process.stdout.close.assert_called_once()
        resume.assert_not_called()

    def test_start_resume_failure_preserves_first_error_and_reader_cleanup(self) -> None:
        server = F.MysqlProcess(self.root, ["not executed"])
        process, job, reader = self.inert_process()
        reader.start = MagicMock()
        job.close.side_effect = OSError("private close detail")
        with patch.object(F.subprocess, "Popen", return_value=process), \
             patch.object(F.BOUNDS.WindowsJob, "attach", return_value=job), \
             patch.object(F.threading, "Thread", return_value=reader), \
             patch.object(F.BOUNDS, "os", SimpleNamespace(name="nt")), \
             patch.object(F.BOUNDS, "resume_suspended_windows_process", side_effect=OSError("resume primary")), \
             self.assertRaises(RuntimeError) as caught:
            server.start()
        self.assertIn("resume primary", str(caught.exception))
        self.assertIn("server_stop", str(caught.exception))
        self.assertNotIn("private close detail", str(caught.exception))
        self.assertIs(server.job, job)
        reader.join.assert_called_once()
        process.stdout.close.assert_called_once()
        self.assertTrue((self.root / "server.active").exists())

    def test_start_keeps_reader_owner_when_thread_start_is_interrupted(self) -> None:
        server = F.MysqlProcess(self.root, ["not executed"])
        process, job, reader = self.inert_process()
        reader.start = MagicMock(side_effect=KeyboardInterrupt("reader start primary"))
        # start may be interrupted after a thread exists; its owner must already
        # be retained, even though start never returned normally.
        reader.is_alive.return_value = True
        with patch.object(F.subprocess, "Popen", return_value=process), \
             patch.object(F.BOUNDS.WindowsJob, "attach", return_value=job), \
             patch.object(F.threading, "Thread", return_value=reader), \
             patch.object(F.BOUNDS, "os", SimpleNamespace(name="nt")), \
             patch.object(F.BOUNDS, "resume_suspended_windows_process") as resume, \
             self.assertRaises(BaseException):
            server.start()
        self.assertIs(server.reader, reader)
        reader.join.assert_called_once()
        process.stdout.close.assert_not_called()
        resume.assert_not_called()
        self.assertTrue((self.root / "server.active").exists())

    def test_stop_uncertainty_is_not_erased_by_a_second_cleanup(self) -> None:
        server = self.marked_server()
        server.job.close.side_effect = OSError("first close failed")
        with patch.object(F.BOUNDS, "kill_process_tree"), self.assertRaises(OSError):
            server.stop()
        server.job.close.side_effect = None
        with patch.object(F.BOUNDS, "kill_process_tree"), self.assertRaisesRegex(RuntimeError, "previous_cleanup_failure"):
            server.stop()
        self.assertTrue((self.root / "server.active").exists())
        self.assertTrue((self.root / "server.pid").exists())

    def test_interrupted_unregistered_reader_cannot_be_closed_as_not_alive(self) -> None:
        server = F.MysqlProcess(self.root, ["not executed"])
        process, job, reader = self.inert_process()
        reader.ident = None
        reader.start = MagicMock(side_effect=KeyboardInterrupt("reader start primary"))
        reader.join.side_effect = RuntimeError("cannot join thread before it is started")
        reader.is_alive.return_value = False
        # OS creation can precede Python thread registration. Both observations
        # remain false/None here; they do not prove it can never read the pipe.
        with patch.object(F.subprocess, "Popen", return_value=process), \
             patch.object(F.BOUNDS.WindowsJob, "attach", return_value=job), \
             patch.object(F.threading, "Thread", return_value=reader), \
             patch.object(F.BOUNDS, "os", SimpleNamespace(name="nt")), \
             patch.object(F.BOUNDS, "resume_suspended_windows_process") as resume, \
             self.assertRaises(BaseException) as caught:
            server.start()
        process.stdout.close.assert_not_called()
        self.assertTrue((self.root / "server.active").exists())
        self.assertIs(server.reader, reader)
        self.assertIn("reader_start_unknown", str(caught.exception))
        resume.assert_not_called()

    def test_unknown_reader_needs_successful_join_not_just_an_identity(self) -> None:
        server = self.marked_server()
        server.reader_start_state = "unknown"
        server.reader.ident = 1
        server.reader.join.side_effect = RuntimeError("thread registration still incomplete")
        with patch.object(F.BOUNDS, "kill_process_tree"), \
             self.assertRaisesRegex(RuntimeError, "reader_start_unknown"):
            server.stop()
        self.assertEqual("unknown", server.reader_start_state)
        server.process.stdout.close.assert_not_called()
        self.assertTrue((self.root / "server.active").exists())

        # A later bounded cleanup can reap the now-registered, finished reader,
        # but cannot erase the earlier uncertainty or manufacture acceptance.
        server.reader.join.side_effect = None
        with patch.object(F.BOUNDS, "kill_process_tree"), \
             self.assertRaisesRegex(RuntimeError, "previous_cleanup_failure"):
            server.stop()
        self.assertEqual("started", server.reader_start_state)
        server.process.stdout.close.assert_called_once()
        self.assertTrue((self.root / "server.active").exists())
        self.assertTrue((self.root / "server.pid").exists())

    def test_reader_construction_failure_can_close_the_never_read_pipe(self) -> None:
        server = F.MysqlProcess(self.root, ["not executed"])
        process, job, _ = self.inert_process()
        with patch.object(F.subprocess, "Popen", return_value=process), \
             patch.object(F.BOUNDS.WindowsJob, "attach", return_value=job), \
             patch.object(F.threading, "Thread", side_effect=RuntimeError("reader construction primary")), \
             patch.object(F.BOUNDS, "os", SimpleNamespace(name="nt")), \
             patch.object(F.BOUNDS, "resume_suspended_windows_process") as resume, \
             self.assertRaisesRegex(RuntimeError, "reader construction primary"):
            server.start()
        self.assertIsNone(server.reader)
        self.assertEqual("not_attempted", server.reader_start_state)
        process.stdout.close.assert_called_once()
        self.assertFalse((self.root / "server.active").exists())
        resume.assert_not_called()

    def test_verify_late_reader_failure_after_graceful_wait_forbids_receipt(self) -> None:
        server = F.MysqlProcess(self.root, [])
        server.process, server.job, server.reader = self.inert_process()
        server.reader_start_state = "started"
        server.start = MagicMock(side_effect=lambda: (self.root / "server.active").write_bytes(b"inert owned marker"))
        server.wait_ready = MagicMock()
        server.reader.join.side_effect = lambda **_: server.failed.set()
        stack, _, _ = self.verification(server=server)
        with stack, patch.object(F.BOUNDS, "os", SimpleNamespace(name="nt")), \
             self.assertRaisesRegex(RuntimeError, "reader_failed"):
            F.verify(self.args)
        self.assertEqual(2, server.process.wait.call_count)
        for name in ("verification.json", "init.sql", "admin.cnf"):
            self.assertFalse((self.root / name).exists(), name)
        self.assertTrue((self.root / "server.active").exists())

    def test_verify_partial_startup_file_failure_still_removes_created_secret(self) -> None:
        stack, server, _ = self.verification()

        def partial_startup(*_):
            (self.root / "init.sql").write_bytes(b"generated inert secret")
            raise OSError("startup file primary")

        with stack, patch.object(F, "write_startup_files", side_effect=partial_startup), \
             self.assertRaisesRegex(OSError, "startup file primary"):
            F.verify(self.args)
        server.start.assert_not_called()
        self.assertFalse((self.root / "init.sql").exists())
        self.assertFalse((self.root / "verification.json").exists())

    def test_cleanup_removes_only_validated_private_tree(self) -> None:
        outside = self.target / "unrelated.txt"
        outside.write_bytes(b"preserved")
        F.cleanup(self.args)
        self.assertFalse(self.root.exists())
        self.assertEqual(b"preserved", outside.read_bytes())

    def test_cleanup_refuses_active_markers_and_bounds_before_deletion(self) -> None:
        for name in ("server.pid", "server.active", "operation.lock"):
            (self.root / name).write_bytes(b"marker")
            with self.assertRaises(ValueError):
                F.cleanup(self.args)
            (self.root / name).unlink()
        with patch.object(F, "MAX_CLEANUP_FILES", 1), self.assertRaisesRegex(ValueError, "bound"):
            F.cleanup(self.args)
        self.assertTrue((self.root / "db.json").exists())

    def fake_installation(self) -> Path:
        installed = self.base / "installed"
        for name in ("bin/mysqld.exe", "bin/mysql.exe", "bin/mysqladmin.exe", "bin/crypto.dll",
                     "lib/libmysql.dll", "lib/libmysql.lib", "include/mysql.h", "share/english/errmsg.sys"):
            path = installed / name
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_bytes(b"not executable; fixture content")
        (installed / "bin" / "mysqld.my").write_bytes(b"must not be copied")
        (installed / "my.ini").write_bytes(b"must not be read or copied")
        return installed

    def test_snapshot_excludes_configuration_and_component_manifests(self) -> None:
        installed = self.fake_installation()
        destination = self.base / "snapshot"
        destination.mkdir()
        F.copy_installation(installed, destination)
        self.assertTrue((destination / "portable" / "bin" / "mysqld.exe").exists())
        self.assertFalse((destination / "portable" / "bin" / "mysqld.my").exists())
        self.assertFalse((destination / "portable" / "my.ini").exists())
        self.assertFalse(any((destination / "portable" / "empty-plugins").iterdir()))

    def test_snapshot_rejects_size_and_entry_budget(self) -> None:
        installed = self.fake_installation()
        for name, setting in (("size", "MAX_FILE_BYTES"), ("entries", "MAX_FILES")):
            destination = self.base / name
            destination.mkdir()
            with patch.object(F, setting, 1), self.assertRaisesRegex(ValueError, "bound"):
                F.copy_installation(installed, destination)

    def test_prepare_initializes_only_new_datadir_with_secure_defaults(self) -> None:
        installed = self.fake_installation()
        fresh = self.target / (F.PREFIX + "1" * 32)
        fresh.mkdir()
        commands = []

        def run(command: list[str], cwd: Path, label: str, timeout: int = 30) -> bytes:
            commands.append(command)
            if "--version" in command:
                return b"mysqld Ver 8.0.29 for Win64 on x86_64"
            self.assertIn("--initialize", command)
            self.assertNotIn("--initialize-insecure", command)
            self.assertIn(f"--datadir={fresh / 'data'}", command)
            self.assertEqual("--no-defaults", command[1])
            (fresh / "data").mkdir()
            (fresh / "data" / "auto.cnf").write_text(f"[auto]\nserver-uuid={UUID}\n", encoding="ascii")
            return b"temporary password is deliberately not parsed or printed"

        fake_socket = MagicMock()
        fake_socket.socket.return_value.__enter__.return_value.getsockname.return_value = ("127.0.0.1", 23457)
        with patch.object(F, "private_fixture", return_value=fresh), patch.object(F, "run", side_effect=run), \
             patch.object(F, "socket", fake_socket):
            self.assertEqual(fresh, F.prepare(argparse.Namespace(installed_root=installed)))
        config, admin = F.read_json(fresh / "db.json"), F.read_json(fresh / "admin.json")
        F.validate_config(config, admin)
        self.assertNotEqual(config["password"], admin["password"])
        self.assertEqual(2, len(commands))
        self.assertFalse((fresh / "server.active").exists())

    def test_prepare_rejects_wrong_server_before_initialization(self) -> None:
        installed = self.fake_installation()
        fresh = self.target / (F.PREFIX + "1" * 32)
        fresh.mkdir()
        with patch.object(F, "private_fixture", return_value=fresh), \
             patch.object(F, "run", return_value=b"mysqld Ver 8.0.30 for Win64") as run:
            with self.assertRaisesRegex(ValueError, "exactly"):
                F.prepare(argparse.Namespace(installed_root=installed))
        self.assertEqual(1, run.call_count)
        self.assertFalse((fresh / "data").exists())

    def test_symlink_input_and_cleanup_are_rejected(self) -> None:
        link = self.root / "link"
        try:
            link.symlink_to(self.test)
        except OSError:
            self.skipTest("creating symlinks is not permitted on this host")
        with self.assertRaisesRegex(ValueError, "Symlinks"):
            F.bounded_bytes(link)
        with self.assertRaisesRegex(ValueError, "Symlinks"):
            F.cleanup(self.args)
        self.assertTrue(self.test.exists())
        self.assertTrue((self.root / "db.json").exists())


if __name__ == "__main__":
    unittest.main()
