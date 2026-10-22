"""Private PostgreSQL fixture setup must not broaden its parent permissions."""
import json
from contextlib import redirect_stdout
import io
import os
from pathlib import Path
import signal
import sys
import tempfile
import time
from types import SimpleNamespace
import unittest
from unittest.mock import Mock, patch

sys.dont_write_bytecode = True
import http_pg_lifecycle as fixture


class WindowsPostgresDirectoryContracts(unittest.TestCase):
    def test_other_platforms_leave_permissions_and_report_alone(self):
        report = {}
        with patch.object(fixture.sys, "platform", "darwin"), patch.object(fixture.subprocess, "run") as runner:
            fixture.prepare_windows_pg_directory(Path.cwd(), report)
        runner.assert_not_called()
        self.assertEqual(report, {})

    def test_refuses_nonfixture_directory_before_running_commands(self):
        with patch.object(fixture.sys, "platform", "win32"), patch.object(fixture.subprocess, "run") as runner:
            with self.assertRaises(ValueError):
                fixture.prepare_windows_pg_directory(Path.cwd(), {})
        runner.assert_not_called()

    def test_setup_failure_retains_bounded_diagnostic_and_fails(self):
        report = {}
        failure = SimpleNamespace(returncode=1, stdout='{"sddl_before":"private"}', stderr="denied" * 1000)
        with tempfile.TemporaryDirectory(prefix="ku-http-pg-acceptance-") as temporary:
            with patch.object(fixture.sys, "platform", "win32"), \
                    patch.object(fixture.subprocess, "run", return_value=failure):
                with self.assertRaises(RuntimeError):
                    fixture.prepare_windows_pg_directory(Path(temporary), report)
        evidence = report["windows_pg_directory_acl"]
        self.assertEqual(evidence["exit_code"], 1)
        self.assertIn("sddl_before", evidence["stdout_tail"])
        self.assertEqual(len(evidence["stderr_tail"]), 2000)

    def test_unchanged_parent_is_required_and_root_is_passed_as_data(self):
        for changed in (False, True):
            with self.subTest(parent_changed=changed), \
                    tempfile.TemporaryDirectory(prefix="ku-http-pg-acceptance-") as temporary:
                report = {}
                acl = {"parent_sddl_before": "parent", "parent_sddl_after": "changed" if changed else "parent",
                       "current_user_modify_inheritable": True}
                success = SimpleNamespace(returncode=0, stdout=json.dumps(acl), stderr="")
                with patch.object(fixture.sys, "platform", "win32"), \
                        patch.object(fixture.subprocess, "run", return_value=success) as runner:
                    if changed:
                        with self.assertRaises(RuntimeError):
                            fixture.prepare_windows_pg_directory(Path(temporary), report)
                    else:
                        fixture.prepare_windows_pg_directory(Path(temporary), report)
                command = runner.call_args.args[0]
                self.assertNotIn(temporary, command[-1])
                self.assertEqual(runner.call_args.kwargs["env"]["KU_PG_FIXTURE_ROOT"], str(Path(temporary).resolve()))
                self.assertEqual(report["windows_pg_directory_acl"]["acl"], acl)

    def test_only_child_module_path_is_reset_and_other_environment_is_preserved(self):
        acl = {"parent_sddl_before": "parent", "parent_sddl_after": "parent",
               "current_user_modify_inheritable": True}
        success = SimpleNamespace(returncode=0, stdout=json.dumps(acl), stderr="")
        inherited = {"PSModulePath": "Core-only-module-path", "KU_PG_LIB": "private-libpq", "PATH": "unchanged-path"}
        with tempfile.TemporaryDirectory(prefix="ku-http-pg-acceptance-") as temporary, \
                patch.dict(fixture.os.environ, inherited), \
                patch.object(fixture.sys, "platform", "win32"), \
                patch.object(fixture.subprocess, "run", return_value=success) as runner:
            before = dict(os.environ)
            fixture.prepare_windows_pg_directory(Path(temporary), {})
            child = runner.call_args.kwargs["env"]
            self.assertFalse(any(name.upper() == "PSMODULEPATH" for name in child))
            self.assertEqual(child["KU_PG_LIB"], inherited["KU_PG_LIB"])
            self.assertEqual(child["PATH"], inherited["PATH"])
            self.assertEqual(dict(os.environ), before)

    def test_missing_current_user_modify_inheritance_fails(self):
        for accepted in (None, False):
            with self.subTest(accepted=accepted), \
                    tempfile.TemporaryDirectory(prefix="ku-http-pg-acceptance-") as temporary:
                acl = {"parent_sddl_before": "parent", "parent_sddl_after": "parent",
                       "current_user_modify_inheritable": accepted}
                success = SimpleNamespace(returncode=0, stdout=json.dumps(acl), stderr="")
                with patch.object(fixture.sys, "platform", "win32"), \
                        patch.object(fixture.subprocess, "run", return_value=success):
                    with self.assertRaises(RuntimeError):
                        fixture.prepare_windows_pg_directory(Path(temporary), {})

    @unittest.skipUnless(os.name == "nt", "actual directory ACL requires Windows")
    def test_real_windows_acl_grants_current_user_without_touching_parent(self):
        report = {}
        with tempfile.TemporaryDirectory(prefix="ku-http-pg-acceptance-") as temporary:
            fixture.prepare_windows_pg_directory(Path(temporary), report)
            acl = report["windows_pg_directory_acl"]["acl"]
            self.assertEqual(acl["parent_sddl_before"], acl["parent_sddl_after"])
            self.assertEqual(acl["owner_before"], acl["owner_after"])
            self.assertTrue(acl["current_user_modify_inheritable"])
            self.assertTrue(any(rule["sid"] == acl["user_sid"] and rule["allow"] and rule["modify"]
                                and rule["object_and_container_inherit"] and rule["applies_to_root"]
                                for rule in acl["current_user_access_rules"]))
            self.assertEqual(acl["rights"], "Modify")
            self.assertEqual(acl["inheritance"], "ContainerInherit,ObjectInherit")
            child = Path(temporary) / "test-data"
            child.mkdir()
            (child / "probe.txt").write_text("private", encoding="utf-8")


class FixtureProcessContracts(unittest.TestCase):
    def test_timeout_keeps_sixty_second_command_and_bounded_reap(self):
        child = Mock(pid=123, returncode=1)
        child.wait.side_effect = [fixture.subprocess.TimeoutExpired("fixture-tool", 60), 1]
        phases = []
        with patch.object(fixture.subprocess, "Popen", return_value=child), self.assertRaises(RuntimeError):
            fixture.run(["fixture-tool"], progress=lambda phase, **details: phases.append((phase, details)))
        self.assertEqual(child.wait.call_args_list, [unittest.mock.call(timeout=60), unittest.mock.call(timeout=5)])
        child.kill.assert_called_once()
        child.communicate.assert_not_called()
        self.assertEqual(phases[-1][0], "fixture-tool:timeout")

    def test_background_descendant_holding_stdout_does_not_delay_tool_output(self):
        with tempfile.TemporaryDirectory(prefix="ku-pg-tool-contract-") as temporary:
            pid_file = Path(temporary) / "descendant.pid"
            program = (
                "import pathlib,subprocess,sys; "
                "child=subprocess.Popen([sys.executable,'-c','import time; time.sleep(5)'],"
                "stdout=sys.stdout,stderr=sys.stderr); "
                f"pathlib.Path({str(pid_file)!r}).write_text(str(child.pid)); "
                "print('tool-output-ready',flush=True)"
            )
            try:
                entered = time.monotonic()
                with redirect_stdout(io.StringIO()):
                    output = fixture.run([sys.executable, "-c", program])
                self.assertEqual(output, "tool-output-ready")
                self.assertLess(time.monotonic() - entered, 3)
            finally:
                if pid_file.is_file():
                    try:
                        os.kill(int(pid_file.read_text()), signal.SIGTERM)
                    except ProcessLookupError:
                        pass

    def test_cleanup_errors_are_saved_and_do_not_skip_remaining_cleanup(self):
        stopped = []

        class Service:
            def poll(self):
                return None

            def kill(self):
                stopped.append("http")
                raise OSError("fixture HTTP stop failure")

        class Proxy:
            def close(self):
                stopped.append("proxy")
                raise OSError("fixture proxy stop failure")

        def stop_database():
            stopped.append("postgres")
            raise OSError("fixture database stop failure")

        with tempfile.TemporaryDirectory(prefix="ku-pg-cleanup-contract-") as temporary:
            output = Path(temporary) / "result.json"
            data = Path(temporary) / "data"
            data.mkdir()
            (data / "postmaster.pid").write_text("fixture-private-pid")
            report = {"status": "failed", "failure": "original fixture failure", "checks": {"read": "passed"}}
            with redirect_stdout(io.StringIO()), self.assertRaises(RuntimeError):
                fixture.cleanup_fixture(report, output, Service(), Proxy(), data, stop_database, False)
            recorded = json.loads(output.read_text())
            self.assertEqual(stopped, ["http", "proxy", "postgres"])
            self.assertEqual(len(recorded["cleanup_errors"]), 3)
            self.assertEqual(recorded["failure"], "original fixture failure")
            self.assertEqual(recorded["checks"], {"read": "passed"})
            self.assertEqual(recorded["current_phase"], "cleanup:finished")
            self.assertFalse(output.with_name(output.name + ".tmp").exists())

    def test_missing_private_pid_and_unstarted_server_are_not_stopped(self):
        with tempfile.TemporaryDirectory(prefix="ku-pg-cleanup-contract-") as temporary:
            output = Path(temporary) / "result.json"
            with patch.object(fixture, "checkpoint"), patch("builtins.print"):
                stop = Mock()
                fixture.cleanup_fixture({"status": "passed"}, output, None, None, Path(temporary) / "data", stop, False)
            stop.assert_not_called()


if __name__ == "__main__":
    unittest.main()
