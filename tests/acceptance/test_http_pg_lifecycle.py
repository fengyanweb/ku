"""Private PostgreSQL fixture setup must not broaden its parent permissions."""
import json
import os
from pathlib import Path
import sys
import tempfile
from types import SimpleNamespace
import unittest
from unittest.mock import patch

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


if __name__ == "__main__":
    unittest.main()
