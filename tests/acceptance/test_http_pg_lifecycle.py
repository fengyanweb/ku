"""Private PostgreSQL setup, independent lanes and bounded recovery contracts."""
import hashlib
import json
from contextlib import redirect_stdout
import io
import os
from pathlib import Path
import signal
import socket
import sys
import tempfile
import threading
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


class FixtureAbsoluteDeadlineContracts(unittest.TestCase):
    def test_expired_tool_deadline_opens_no_child(self):
        with patch.object(fixture, "time", SimpleNamespace(monotonic=lambda: 105.0)), \
                patch.object(fixture.subprocess, "Popen") as launch, \
                self.assertRaises(TimeoutError):
            fixture.run(["fixture-tool"], deadline=105.0, progress=Mock())
        launch.assert_not_called()

    def test_starting_checkpoint_consumes_tool_budget_before_launch(self):
        clock = SimpleNamespace(now=100.0)

        def progress(phase, **details):
            clock.now += 5.0

        with patch.object(fixture, "time", SimpleNamespace(monotonic=lambda: clock.now)), \
                patch.object(fixture.subprocess, "Popen") as launch, \
                self.assertRaises(TimeoutError):
            fixture.run(["fixture-tool"], deadline=105.0, progress=progress)
        launch.assert_not_called()

    def test_short_tool_budget_includes_checkpoints_and_process_creation(self):
        clock = SimpleNamespace(now=100.0)
        child = Mock(pid=123, returncode=0)
        child.poll.return_value = 0

        def launch(*args, **kwargs):
            clock.now += 0.5
            return child

        def progress(phase, **details):
            if phase.endswith((":starting", ":running")):
                clock.now += 0.5

        def wait(*, timeout):
            self.assertEqual(timeout, 0.5)
            clock.now += 0.25

        child.wait.side_effect = wait
        with patch.object(fixture, "time", SimpleNamespace(monotonic=lambda: clock.now)), \
                patch.object(fixture.subprocess, "Popen", side_effect=launch):
            fixture.run(["fixture-tool"], timeout_seconds=2, progress=progress)
        self.assertEqual(clock.now, 101.75)
        child.kill.assert_not_called()
        child.communicate.assert_not_called()

    def test_late_process_creation_kills_and_reaps_with_bounded_cleanup(self):
        clock = SimpleNamespace(now=100.0)
        child = Mock(pid=123, returncode=0)
        child.poll.return_value = None

        def launch(*args, **kwargs):
            clock.now = 105.0
            return child

        with patch.object(fixture, "time", SimpleNamespace(monotonic=lambda: clock.now)), \
                patch.object(fixture.subprocess, "Popen", side_effect=launch), \
                self.assertRaises(TimeoutError):
            fixture.run(["fixture-tool"], deadline=105.0, progress=Mock())
        child.kill.assert_called_once()
        child.wait.assert_called_once_with(timeout=5)
        child.communicate.assert_not_called()

    def test_tool_timeout_reaps_direct_child_without_renewing_deadline(self):
        child = Mock(pid=123, returncode=1)
        child.poll.return_value = None
        child.wait.side_effect = [fixture.subprocess.TimeoutExpired("fixture-tool", 5), 1]
        with patch.object(fixture, "time", SimpleNamespace(monotonic=lambda: 100.0)), \
                patch.object(fixture.subprocess, "Popen", return_value=child), \
                self.assertRaises(TimeoutError):
            fixture.run(["fixture-tool"], deadline=105.0, progress=Mock())
        self.assertEqual(child.wait.call_args_list,
                         [unittest.mock.call(timeout=5.0), unittest.mock.call(timeout=5)])
        child.kill.assert_called_once()
        child.communicate.assert_not_called()

    def test_running_checkpoint_failure_still_kills_and_reaps_child(self):
        child = Mock(pid=123, returncode=None)
        child.poll.return_value = None

        def progress(phase, **details):
            if phase.endswith(":running"):
                raise RuntimeError("checkpoint failure")

        with patch.object(fixture.subprocess, "Popen", return_value=child), \
                self.assertRaisesRegex(RuntimeError, "checkpoint failure"):
            fixture.run(["fixture-tool"], progress=progress)
        child.kill.assert_called_once()
        child.wait.assert_called_once_with(timeout=5)

    def test_late_output_checkpoint_cannot_turn_exit_zero_into_success(self):
        clock = SimpleNamespace(now=100.0)
        child = Mock(pid=123, returncode=0)
        child.poll.return_value = 0

        def progress(phase, **details):
            if phase.endswith(":finished"):
                clock.now = 105.0

        with patch.object(fixture, "time", SimpleNamespace(monotonic=lambda: clock.now)), \
                patch.object(fixture.subprocess, "Popen", return_value=child), \
                self.assertRaises(TimeoutError):
            fixture.run(["fixture-tool"], deadline=105.0, progress=progress)
        child.wait.assert_called_once_with(timeout=5.0)
        child.kill.assert_not_called()

    def test_drain_after_other_work_uses_only_existing_remaining_time(self):
        service = Mock()
        with patch.object(fixture, "time", SimpleNamespace(monotonic=lambda: 104.8)):
            fixture.wait_service_exit(service, deadline=105.0)
        self.assertAlmostEqual(service.wait.call_args.kwargs["timeout"], 0.2)
        service.kill.assert_not_called()

    def test_drain_expiry_never_reopens_a_five_second_window(self):
        service = Mock()
        service.poll.return_value = None
        with patch.object(fixture, "time", SimpleNamespace(monotonic=lambda: 105.0)), \
                self.assertRaises(TimeoutError):
            fixture.wait_service_exit(service, deadline=105.0)
        service.kill.assert_called_once()
        service.wait.assert_called_once_with(timeout=3)

    def test_direct_exit_at_deadline_is_late_even_with_exit_zero(self):
        clock = SimpleNamespace(now=104.8)
        service = Mock()
        service.poll.return_value = 0
        service.wait.side_effect = lambda **kwargs: setattr(clock, "now", 105.0)
        with patch.object(fixture, "time", SimpleNamespace(monotonic=lambda: clock.now)), \
                self.assertRaises(TimeoutError):
            fixture.wait_service_exit(service, deadline=105.0)
        self.assertEqual(service.wait.call_count, 1)
        service.kill.assert_not_called()


class RemoteSessionRetirementContracts(unittest.TestCase):
    def scenario(self, observations, *, initial_time=100.0, progress_seconds=0,
                 deadline=105.0, maximum=4, max_observations=8, sleep_extra_seconds=0):
        clock = SimpleNamespace(now=initial_time)
        pending = list(observations)
        calls, checkpoints, sleeps, history = [], [], [], {}

        def sql(statement, *, deadline):
            calls.append((statement, deadline, clock.now))
            self.assertTrue(pending, "unexpected additional database observation")
            elapsed, value = pending.pop(0)
            clock.now += elapsed
            if isinstance(value, Exception):
                raise value
            return json.dumps(value)

        def progress(sequence):
            checkpoints.append(sequence)
            clock.now += progress_seconds

        def sleep(seconds):
            self.assertGreater(seconds, 0)
            sleeps.append(seconds)
            clock.now += seconds + sleep_extra_seconds

        def execute():
            with patch.object(fixture, "time", SimpleNamespace(monotonic=lambda: clock.now,
                                                              sleep=sleep)):
                return fixture.observe_database_connections(
                    sql, history, maximum=maximum, began=100.0, deadline=deadline,
                    max_observations=max_observations, progress=progress)

        return SimpleNamespace(run=execute, calls=calls, checkpoints=checkpoints,
                               sleeps=sleeps, history=history, pending=pending, clock=clock)

    def test_remote_sessions_can_retire_within_original_window(self):
        states = [{"pid": index, "state": "active", "wait_event": "PgSleep"}
                  for index in range(4)] + [{"pid": 4, "state": "idle"}]
        scenario = self.scenario([(0.1, {"count": 5, "rows": states}),
                                  (0.1, {"count": 1, "rows": states[-1:]})])
        self.assertEqual(scenario.run(), 1)
        self.assertEqual([row[2] for row in scenario.calls], [100.0, 100.625])
        self.assertEqual([row[1] for row in scenario.calls], [105.0, 105.0])
        self.assertTrue(all(row[0] == fixture.SESSION_OBSERVATION_SQL for row in scenario.calls))
        self.assertIn("LIMIT 24", scenario.calls[0][0])
        self.assertIn("left(query, 128)", scenario.calls[0][0])
        self.assertEqual([row["count"] for row in scenario.history["observations"]], [5, 1])
        self.assertEqual(scenario.history["observations"][0]["rows"], states)
        self.assertEqual(scenario.history["absolute_deadline_monotonic"], 105.0)
        self.assertTrue(scenario.history["recovered"])
        self.assertFalse(scenario.history["business_operation_replay"])

    def test_permanent_excess_is_bounded_by_original_eight_observations(self):
        scenario = self.scenario([(0.1, {"count": 5, "rows": []})] * 8)
        with self.assertRaises(AssertionError):
            scenario.run()
        self.assertEqual(len(scenario.calls), 8)
        self.assertEqual([row[2] for row in scenario.calls],
                         [100.0, 100.625, 101.25, 101.875, 102.5, 103.125, 103.75, 104.375])
        self.assertEqual(scenario.history["stop_reason"], "observation_limit")
        self.assertFalse(scenario.history["recovered"])

    def test_nonmonotonic_session_counts_preserve_each_observation(self):
        scenario = self.scenario([(0.05, {"count": count, "rows": []}) for count in (5, 7, 6, 4)])
        self.assertEqual(scenario.run(), 4)
        self.assertEqual([row["count"] for row in scenario.history["observations"]], [5, 7, 6, 4])
        self.assertEqual(len(scenario.calls), 4)

    def test_new_reads_and_logs_consuming_window_do_not_renew_it(self):
        scenario = self.scenario([(0.1, {"count": 1, "rows": []})], initial_time=104.8)
        self.assertEqual(scenario.run(), 1)
        self.assertEqual([row[1:] for row in scenario.calls], [(105.0, 104.8)])
        self.assertAlmostEqual(scenario.history["elapsed_seconds"], 4.9)

    def test_expired_window_sends_no_session_query(self):
        scenario = self.scenario([], initial_time=105.0)
        with self.assertRaises(TimeoutError):
            scenario.run()
        self.assertEqual(scenario.calls, [])
        self.assertEqual(scenario.checkpoints, [])
        self.assertEqual(scenario.history["observations"], [])
        self.assertEqual(scenario.history["stop_reason"], "absolute_deadline")

    def test_progress_checkpoint_can_exhaust_window_without_launching_sql(self):
        scenario = self.scenario([], progress_seconds=5)
        with self.assertRaises(TimeoutError):
            scenario.run()
        self.assertEqual(scenario.calls, [])
        self.assertEqual(scenario.checkpoints, [1])
        self.assertEqual(scenario.history["elapsed_seconds"], 5)

    def test_slow_query_cannot_report_late_zero_as_success(self):
        scenario = self.scenario([(5, {"count": 0, "rows": []})], maximum=0)
        with self.assertRaises(TimeoutError):
            scenario.run()
        self.assertEqual(len(scenario.calls), 1)
        self.assertEqual(scenario.history["observations"][0]["count"], 0)
        self.assertEqual(scenario.history["observations"][0]["finished_monotonic"], 105.0)
        self.assertFalse(scenario.history["recovered"])

    def test_response_decoding_time_is_part_of_existing_window(self):
        scenario = self.scenario([(4.8, {"count": 0, "rows": []})], maximum=0)
        decoder = json.loads

        def decode(text):
            value = decoder(text)
            scenario.clock.now += 0.2
            return value

        with patch.object(fixture.json, "loads", side_effect=decode), self.assertRaises(TimeoutError):
            scenario.run()
        self.assertEqual(scenario.history["observations"][0]["finished_monotonic"], 105.0)
        self.assertFalse(scenario.history["recovered"])

    def test_oversleep_beyond_deadline_cannot_launch_next_session_query(self):
        scenario = self.scenario([(0.1, {"count": 5, "rows": []})], sleep_extra_seconds=4.5)
        with self.assertRaises(TimeoutError):
            scenario.run()
        self.assertEqual(len(scenario.calls), 1)
        self.assertEqual(scenario.checkpoints, [1])
        self.assertEqual(scenario.history["stop_reason"], "absolute_deadline")

    def test_sql_error_is_preserved_without_replaying_or_more_observations(self):
        scenario = self.scenario([(0.2, RuntimeError("observation tool failed")),
                                  (0.1, {"count": 0, "rows": []})])
        with self.assertRaisesRegex(RuntimeError, "observation tool failed"):
            scenario.run()
        self.assertEqual(len(scenario.calls), 1)
        self.assertEqual(len(scenario.pending), 1)
        self.assertIn("RuntimeError", scenario.history["observations"][0]["error"])
        self.assertFalse(scenario.history["recovered"])

    def test_immediate_mode_never_adds_waits_or_additional_queries(self):
        for count, accepted in ((4, True), (5, False)):
            with self.subTest(count=count):
                scenario = self.scenario([(0.1, {"count": count, "rows": []})],
                                         deadline=None, max_observations=1)
                if accepted:
                    self.assertEqual(scenario.run(), 4)
                else:
                    with self.assertRaises(AssertionError):
                        scenario.run()
                self.assertEqual(len(scenario.calls), 1)
                self.assertEqual(scenario.sleeps, [])
                self.assertIsNone(scenario.calls[0][1])

    def test_invalid_counts_or_unbounded_state_rows_fail_without_next_query(self):
        malformed = [{"count": count, "rows": []} for count in (-1, True, 1.0, "1", None)]
        malformed += [{"count": 1, "rows": rows} for rows in ({}, None, ["not a state"], [{}] * 25)]
        malformed += [[], None, {"count": 1}, {"rows": []}]
        for value in malformed:
            with self.subTest(value=value):
                scenario = self.scenario([(0.1, value), (0.1, {"count": 0, "rows": []})])
                with self.assertRaises(ValueError):
                    scenario.run()
                self.assertEqual(len(scenario.calls), 1)
                self.assertEqual(len(scenario.pending), 1)
                self.assertFalse(scenario.history["recovered"])


class LaneOrchestrationContracts(unittest.TestCase):
    def exercise(self, *, failure_lane=None, cleanup_errors=None, lane="both",
                 performance_seconds=0):
        with tempfile.TemporaryDirectory(prefix="ku-pg-lane-contract-") as temporary:
            root = Path(temporary)
            ku = root / "ku"
            ku.write_bytes(b"fixture CLI identity, never executed")
            output = root / "lifecycle.json"
            args = fixture.parse_args(["--ku", str(ku), "--output", str(output),
                                       "--lane", lane, "--performance-seconds", str(performance_seconds)])
            invoked = []
            snapshots = []

            def run_lane(arguments, selected, on_checkpoint):
                invoked.append(selected)
                self.assertEqual(arguments.ku, ku)
                self.assertEqual(arguments.performance_seconds, performance_seconds)
                filename = "http_pg_lifecycle.ku" if selected == "sync" else "http_pg_lifecycle_async.ku"
                source = Path(fixture.__file__).resolve().parents[1] / "fixtures" / filename
                raw = {"status": "running", "lane": selected, "fixture": filename,
                       "fixture_sha256": hashlib.sha256(source.read_bytes()).hexdigest(),
                       "cli_sha256": hashlib.sha256(ku.read_bytes()).hexdigest(),
                       "database_retry": False}
                fixture.checkpoint(raw, arguments.output, "fault:slow-query")
                on_checkpoint(raw)
                current = json.loads(output.read_text())
                self.assertEqual(current["phase_details"]["lane"], selected)
                self.assertEqual(current["lanes"][selected]["report"], raw)
                snapshots.append(current)
                raw["status"] = "failed" if selected == failure_lane else "passed"
                raw["cleanup_errors"] = cleanup_errors if selected == failure_lane and cleanup_errors else []
                if performance_seconds:
                    raw["throughput"] = {"requests_per_second": 100 if selected == "sync" else 200}
                fixture.checkpoint(raw, arguments.output, "fixture:finished")
                if selected == failure_lane:
                    raise RuntimeError("original lane assertion failed")
                return raw

            with patch.object(fixture, "run_lane", side_effect=run_lane), redirect_stdout(io.StringIO()):
                if failure_lane:
                    with self.assertRaisesRegex(RuntimeError, "lifecycle failed"):
                        fixture.run_lanes(args)
                else:
                    fixture.run_lanes(args)
            aggregate = json.loads(output.read_text())
            for entry in aggregate["lanes"].values():
                if entry["status"] == "blocked":
                    self.assertFalse(Path(entry["report_file"]).exists())
                else:
                    raw_bytes = Path(entry["report_file"]).read_bytes()
                    self.assertEqual(entry["report_sha256"], hashlib.sha256(raw_bytes).hexdigest())
                    self.assertEqual(entry["report"], json.loads(raw_bytes))
            self.assertFalse(output.with_name(output.name + ".tmp").exists())
            return aggregate, invoked, snapshots

    def test_default_runs_both_lanes_and_keeps_separate_current_evidence(self):
        args = fixture.parse_args(["--ku", "ku", "--output", "report.json"])
        self.assertEqual(args.lane, "both")
        report, invoked, snapshots = self.exercise()
        self.assertEqual(invoked, ["sync", "async"])
        self.assertEqual(report["status"], "passed")
        lanes = report["lanes"]
        self.assertEqual(lanes["sync"]["report"]["cli_sha256"], lanes["async"]["report"]["cli_sha256"])
        self.assertNotEqual(lanes["sync"]["report"]["fixture_sha256"], lanes["async"]["report"]["fixture_sha256"])
        self.assertEqual(snapshots[1]["lanes"]["sync"]["status"], "passed")
        self.assertFalse(report["database_retry"])

    def test_failed_lane_keeps_failure_and_runs_other_lane_only_after_cleanup(self):
        for failed in ("sync", "async"):
            with self.subTest(failed=failed):
                report, invoked, _ = self.exercise(failure_lane=failed)
                self.assertEqual(invoked, ["sync", "async"])
                self.assertEqual(report["status"], "failed")
                self.assertEqual(report["lanes"][failed]["report"]["status"], "failed")
                self.assertEqual(report["lanes"][failed]["failure"], "RuntimeError: original lane assertion failed")

    def test_failed_cleanup_blocks_next_cluster_instead_of_starting_it(self):
        report, invoked, _ = self.exercise(failure_lane="sync", cleanup_errors=[{"stage": "postgres"}])
        self.assertEqual(invoked, ["sync"])
        self.assertEqual(report["lanes"]["async"]["status"], "blocked")
        self.assertIn("cleanup is not confirmed", report["lanes"]["async"]["reason"])

    def test_missing_cleanup_evidence_blocks_next_cluster(self):
        with tempfile.TemporaryDirectory(prefix="ku-pg-lane-contract-") as temporary:
            args = fixture.parse_args(["--ku", "ku", "--output", str(Path(temporary) / "result.json")])
            stale_raw = args.output.with_name("result.sync.json")
            stale_raw.write_text(json.dumps({"status": "passed", "cleanup_errors": []}))
            with patch.object(fixture, "run_lane", side_effect=RuntimeError("preflight failed")) as runner, \
                    redirect_stdout(io.StringIO()), self.assertRaises(RuntimeError):
                fixture.run_lanes(args)
            self.assertEqual(runner.call_count, 1)
            report = json.loads(args.output.read_text())
            self.assertEqual(report["status"], "failed")
            self.assertEqual(report["lanes"]["sync"]["report"]["status"], "failed")
            self.assertNotIn("cleanup_errors", report["lanes"]["sync"]["report"])
            self.assertEqual(report["lanes"]["async"]["status"], "blocked")

    def test_explicit_lane_and_throughput_results_stay_independent(self):
        report, invoked, _ = self.exercise(lane="async", performance_seconds=3)
        self.assertEqual(invoked, ["async"])
        self.assertEqual(report["lanes"]["async"]["report"]["throughput"]["requests_per_second"], 200)
        report, invoked, _ = self.exercise(performance_seconds=3)
        self.assertEqual(invoked, ["sync", "async"])
        self.assertNotIn("throughput", report)
        self.assertEqual([entry["report"]["throughput"]["requests_per_second"]
                          for entry in report["lanes"].values()], [100, 200])

    def test_async_unknown_does_not_claim_synchronous_cancel_confirmation(self):
        metrics = {"reserved_connections": 4, "reserved_waiters": 8,
                   "pending_operations": 0, "pending_bytes": 0,
                   "confirmed_cancellations": 0, "unknown_results": 1}
        fixture.validate_database_metrics(metrics, "async", True)
        with self.assertRaises(AssertionError):
            fixture.validate_database_metrics(metrics, "sync", True)
        metrics["unknown_results"] = 0
        with self.assertRaises(AssertionError):
            fixture.validate_database_metrics(metrics, "async", True)
        fixture.validate_database_metrics(metrics, "async", False)
        for key in ("pending_operations", "pending_bytes"):
            with self.subTest(key=key), self.assertRaises(AssertionError):
                fixture.validate_database_metrics(metrics | {key: 1}, "async", False)


class RecoveryHTTPDeadlineContracts(unittest.TestCase):
    def scripted_drip(self, chunks, timeout_stage):
        """Exercise the actual HTTP/deadline readers without peer scheduling."""
        clock = SimpleNamespace(now=100.0)
        timeouts = []
        delivered = []
        pending = list(chunks)

        def receive(connection, buffer, nbytes=0, flags=0):
            # Model only one transport wait and the socket's configured timeout.
            # The production DeadlineSocket must calculate the remaining budget.
            timeout = connection.gettimeout()
            self.assertIsNotNone(timeout)
            timeouts.append(timeout)
            if timeout < 3:
                clock.now += timeout
                raise TimeoutError("scripted transport wait exceeded socket timeout")
            clock.now += 3
            self.assertTrue(pending, "HTTP reader requested data beyond the complete response")
            chunk = pending.pop(0)
            size = min(len(chunk), nbytes or len(buffer), len(buffer))
            buffer[:size] = chunk[:size]
            delivered.append(chunk[:size])
            if size < len(chunk):
                pending.insert(0, chunk[size:])
            return size

        with patch.object(fixture, "time", SimpleNamespace(monotonic=lambda: clock.now)), \
                patch.object(socket.socket, "connect", lambda connection, address: None), \
                patch.object(socket.socket, "send", lambda connection, data, flags=0: len(data)), \
                patch.object(socket.socket, "recv_into", receive):
            connection = fixture.DeadlineHTTPConnection("127.0.0.1", 9, 108.0)
            try:
                connection.request("GET", "/read")
                if timeout_stage == "headers":
                    with self.assertRaises(TimeoutError):
                        connection.getresponse()
                else:
                    response = connection.getresponse()
                    self.assertEqual(response.status, 200)
                    with self.assertRaises(TimeoutError):
                        response.read()
            finally:
                connection.close()
        self.assertEqual(timeouts, [8.0, 5.0, 2.0])
        self.assertEqual(clock.now, 108.0)
        self.assertEqual(delivered, chunks[:2])
        self.assertEqual(pending, chunks[2:])

    def exchange(self, chunks, interval, budget):
        """A bounded real peer drips data faster than a per-read timeout."""
        stop = threading.Event()
        failures = []
        listener = socket.socket()
        listener.bind(("127.0.0.1", 0))
        listener.listen(1)
        listener.settimeout(0.5)

        def peer():
            try:
                with listener.accept()[0] as connection:
                    connection.settimeout(0.5)
                    request = b""
                    while b"\r\n\r\n" not in request:
                        part = connection.recv(4096)
                        if not part:
                            return
                        request += part
                        if len(request) > 8192:
                            raise AssertionError("fixture request exceeds bounded header")
                    for chunk in chunks:
                        connection.sendall(chunk)
                        if stop.wait(interval):
                            break
            except OSError as error:
                if not stop.is_set():
                    failures.append(error)

        thread = threading.Thread(target=peer)
        thread.start()
        began = time.monotonic()
        connection = fixture.DeadlineHTTPConnection("127.0.0.1", listener.getsockname()[1], began + budget)
        try:
            connection.request("GET", "/read")
            response = connection.getresponse()
            body = response.read()
            fixture.deadline_remaining(connection.deadline)
            return response.status, body
        finally:
            connection.close()
            stop.set()
            listener.close()
            thread.join(timeout=0.6)
            self.assertFalse(thread.is_alive(), "bounded peer must exit")
            self.assertFalse(failures, failures)
            self.assertLess(time.monotonic() - began, 0.8)

    def test_complete_response_within_absolute_budget(self):
        self.assertEqual(self.exchange([b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok"], 0, 0.5),
                         (200, b"ok"))

    def test_dripping_headers_cannot_renew_the_absolute_budget(self):
        self.scripted_drip([b"HTTP/1.1 200 OK\r\n", b"Content-Length: 2\r\n",
                            b"X-Drip: x\r\n\r\nok"], "headers")
        with self.assertRaises((TimeoutError, socket.timeout)):
            self.exchange([b"HTTP/1.1 200 OK\r\n", b"Content-Length: 2\r\n", b"X-Drip: "]
                          + [b"x"] * 32 + [b"\r\n\r\nok"], 0.035, 0.13)

    def test_dripping_body_cannot_renew_the_absolute_budget(self):
        self.scripted_drip([b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n", b"o", b"k"], "body")
        with self.assertRaises((TimeoutError, socket.timeout)):
            self.exchange([b"HTTP/1.1 200 OK\r\nContent-Length: 32\r\n\r\n"]
                          + [b"x"] * 32, 0.035, 0.13)

    def test_expired_deadline_opens_no_socket(self):
        connection = fixture.DeadlineHTTPConnection("127.0.0.1", 9, time.monotonic() - 1)
        try:
            with patch.object(fixture, "DeadlineSocket") as factory, self.assertRaises(TimeoutError):
                connection.request("GET", "/read")
            factory.assert_not_called()
        finally:
            connection.close()

    def test_deadline_connection_does_not_add_dns_to_the_budget(self):
        with self.assertRaises(ValueError):
            fixture.DeadlineHTTPConnection("localhost", 9, time.monotonic() + 1)


class FreshReadRecoveryContracts(unittest.TestCase):
    def test_fault_response_origin_includes_later_side_effect_and_checkpoint_time(self):
        expected = {"rows": 1, "text": "中文🙂"}
        report = {"parameters": {"fault_recovery_deadline_seconds": 5,
                                 "disconnect_recovery_max_new_reads": 8,
                                 "disconnect_recovery_intermediate_errors": ["acquire_timeout"]},
                  "last_request": {"finished_monotonic": 104.7}}
        clock = SimpleNamespace(now=104.8)
        calls = []

        def read(path, *, deadline):
            calls.append((path, deadline, clock.now))
            clock.now += 0.1
            body = json.dumps(expected)
            report["last_request"] = {"body_prefix": body, "finished_monotonic": clock.now}
            return body

        with patch.object(fixture, "time", SimpleNamespace(monotonic=lambda: clock.now)):
            fixture.recover_fault_reads(read, report, expected, "lost_write_recovery",
                                        {"finished_monotonic": 100.0},
                                        start_event="original INSERT response finished",
                                        progress=lambda index: None)
        self.assertEqual(calls, [("/read", 105.0, 104.8)])
        recovery = report["lost_write_recovery"]
        self.assertEqual(recovery["started_monotonic"], 100.0)
        self.assertAlmostEqual(recovery["elapsed_seconds"], 4.9)
        self.assertFalse(recovery["failed_request_replay"])

    def test_side_effect_check_consuming_fault_window_cannot_start_new_read(self):
        report = {"parameters": {"fault_recovery_deadline_seconds": 5,
                                 "disconnect_recovery_max_new_reads": 8,
                                 "disconnect_recovery_intermediate_errors": ["acquire_timeout"]}}
        read = Mock()
        with patch.object(fixture, "time", SimpleNamespace(monotonic=lambda: 105.0)), \
                self.assertRaises(TimeoutError):
            fixture.recover_fault_reads(read, report, {}, "timeout_recovery",
                                        {"finished_monotonic": 100.0},
                                        start_event="original /slow response finished",
                                        progress=lambda index: None)
        read.assert_not_called()
        self.assertEqual(report["timeout_recovery"]["observations"], [])
        self.assertEqual(report["timeout_recovery"]["absolute_deadline_monotonic"], 105.0)
        self.assertEqual(report["timeout_recovery"]["stop_reason"], "absolute_deadline")

    def scenario(self, responses, *, initial_time=100.0, progress_seconds=0,
                 sleep_extra_seconds=0):
        clock = SimpleNamespace(now=initial_time)
        expected = {"rows": 1, "text": "中文🙂"}
        pending = list(responses)
        requests, progress, recovery, sleeps = [], [], {}, []
        last = {}

        def read(path, *, deadline):
            requests.append((path, deadline, clock.now))
            self.assertTrue(pending, "recovery sent an unexpected additional request")
            elapsed, outcome = pending.pop(0)
            last.clear()
            last.update(sequence=len(requests), route=path, started_monotonic=clock.now)
            clock.now += elapsed
            last["finished_monotonic"] = clock.now
            if isinstance(outcome, Exception):
                last["exception"] = type(outcome).__name__
                raise outcome
            last.update(status=200, body_prefix=outcome)
            return outcome

        def mark(index):
            progress.append(index)
            clock.now += progress_seconds

        def sleep(seconds):
            self.assertGreater(seconds, 0)
            sleeps.append(seconds)
            clock.now += seconds + sleep_extra_seconds

        def execute():
            with patch.object(fixture, "time", SimpleNamespace(monotonic=lambda: clock.now,
                                                              sleep=sleep)):
                fixture.recover_new_reads(
                    read, lambda: last, expected, recovery, began=100.0, deadline_seconds=5,
                    max_new_reads=8,
                    intermediate_errors=["query_error", "execution_unknown", "acquire_timeout"],
                    start_event="all pressure requests completed", progress=mark)

        return SimpleNamespace(run=execute, requests=requests, progress=progress,
                               recovery=recovery, pending=pending, expected=expected,
                               sleeps=sleeps)

    def test_transient_result_then_distinct_read_success_keeps_complete_ledger(self):
        for code in ("acquire_timeout", "execution_unknown", "query_error"):
            with self.subTest(code=code):
                scenario = self.scenario([(0.109, "error:" + code),
                                          (0.016, '{"rows":1,"text":"中文🙂"}')])
                scenario.run()
                self.assertEqual(scenario.requests, [("/read", 105.0, 100.0),
                                                     ("/read", 105.0, 100.625)])
                self.assertTrue(scenario.recovery["recovered"])
                self.assertFalse(scenario.recovery["failed_request_replay"])
                observations = scenario.recovery["observations"]
                self.assertEqual([row["sequence"] for row in observations], [1, 2])
                self.assertEqual(observations[0]["body_prefix"], "error:" + code)
                self.assertAlmostEqual(observations[0]["recovery_elapsed_seconds"], 0.109)
                self.assertEqual(json.loads(observations[1]["body_prefix"]), scenario.expected)
                self.assertEqual(observations[1]["recovery_scheduled_monotonic"], 100.625)
                self.assertEqual(scenario.recovery["stop_reason"], "recovered")

    def test_two_second_recovery_is_observed_without_more_reads_or_time(self):
        scenario = self.scenario([(0.125, "error:acquire_timeout")] * 4
                                  + [(0.125, '{"rows":1,"text":"中文🙂"}')])
        scenario.run()
        self.assertEqual([row[2] - 100 for row in scenario.requests],
                         [0, 0.625, 1.25, 1.875, 2.5])
        self.assertTrue(all(row[1] == 105 for row in scenario.requests))
        self.assertEqual(scenario.recovery["elapsed_seconds"], 2.625)
        self.assertTrue(scenario.recovery["recovered"])
        self.assertFalse(scenario.recovery["failed_request_replay"])
        self.assertFalse(scenario.recovery["full_deadline_window_elapsed"])

    def test_later_new_request_keeps_original_deadline_instead_of_renewing(self):
        scenario = self.scenario([(4, "error:acquire_timeout"),
                                  (0.75, '{"rows":1,"text":"中文🙂"}')])
        scenario.run()
        self.assertEqual(scenario.requests, [("/read", 105.0, 100.0), ("/read", 105.0, 104.0)])
        self.assertEqual(scenario.recovery["absolute_deadline_monotonic"], 105.0)
        self.assertEqual(scenario.recovery["elapsed_seconds"], 4.75)

    def test_expired_window_sends_no_new_read(self):
        scenario = self.scenario([], initial_time=105.0)
        with self.assertRaisesRegex(TimeoutError, "absolute deadline exceeded after 0"):
            scenario.run()
        self.assertEqual(scenario.requests, [])
        self.assertEqual(scenario.progress, [])
        self.assertEqual(scenario.recovery["observations"], [])
        self.assertFalse(scenario.recovery["recovered"])

    def test_checkpoint_time_consumes_same_window_and_cannot_send_after_expiry(self):
        scenario = self.scenario([], progress_seconds=5)
        with self.assertRaisesRegex(TimeoutError, "absolute deadline exceeded after 0"):
            scenario.run()
        self.assertEqual(scenario.progress, [1])
        self.assertEqual(scenario.requests, [])
        self.assertEqual(scenario.recovery["elapsed_seconds"], 5)

    def test_probe_wait_that_crosses_deadline_sends_no_new_read(self):
        scenario = self.scenario([(0.125, "error:acquire_timeout")], sleep_extra_seconds=4.5)
        with self.assertRaisesRegex(TimeoutError, "absolute deadline exceeded after 1"):
            scenario.run()
        self.assertEqual(scenario.requests, [("/read", 105.0, 100.0)])
        self.assertEqual(scenario.progress, [1])
        self.assertEqual(scenario.sleeps, [0.5])
        self.assertEqual(scenario.recovery["elapsed_seconds"], 5.125)
        self.assertEqual(scenario.recovery["stop_reason"], "absolute_deadline")
        self.assertTrue(scenario.recovery["full_deadline_window_elapsed"])

    def test_response_at_deadline_is_not_recovery(self):
        scenario = self.scenario([(5, '{"rows":1,"text":"中文🙂"}')])
        with self.assertRaises(TimeoutError):
            scenario.run()
        self.assertEqual(len(scenario.requests), 1)
        self.assertEqual(len(scenario.recovery["observations"]), 1)
        self.assertFalse(scenario.recovery["recovered"])
        self.assertEqual(scenario.recovery["finished_monotonic"], 105.0)

    def test_all_eight_allowed_errors_exhaust_distributed_count_without_replay(self):
        scenario = self.scenario([(0.125, "error:acquire_timeout")] * 8)
        with self.assertRaises(AssertionError):
            scenario.run()
        self.assertEqual(len(scenario.requests), 8)
        self.assertEqual([path for path, _, _ in scenario.requests], ["/read"] * 8)
        self.assertEqual(len(scenario.recovery["observations"]), 8)
        self.assertEqual([row[2] - 100 for row in scenario.requests],
                         [0, 0.625, 1.25, 1.875, 2.5, 3.125, 3.75, 4.375])
        self.assertEqual(scenario.recovery["elapsed_seconds"], 4.5)
        self.assertEqual(scenario.recovery["stop_reason"], "new_read_limit")
        self.assertFalse(scenario.recovery["full_deadline_window_elapsed"])
        self.assertFalse(scenario.recovery["recovered"])

    def test_transient_errors_exhaust_window_without_sending_another_read(self):
        scenario = self.scenario([(3, "error:execution_unknown"), (2, "error:acquire_timeout"),
                                  (0.1, '{"rows":1,"text":"中文🙂"}')])
        with self.assertRaisesRegex(TimeoutError, "absolute deadline exceeded after 2"):
            scenario.run()
        self.assertEqual(scenario.requests, [("/read", 105.0, 100.0), ("/read", 105.0, 103.0)])
        self.assertEqual(len(scenario.pending), 1)
        self.assertEqual(len(scenario.recovery["observations"]), 2)
        self.assertFalse(scenario.recovery["recovered"])

    def test_unknown_error_or_unexpected_body_fails_without_next_read(self):
        for body, error in (("error:permission_denied", AssertionError),
                            ('{"rows":2,"text":"中文🙂"}', AssertionError),
                            ("not JSON", json.JSONDecodeError)):
            with self.subTest(body=body):
                scenario = self.scenario([(0.1, body), (0.1, '{"rows":1,"text":"中文🙂"}')])
                with self.assertRaises(error):
                    scenario.run()
                self.assertEqual(len(scenario.requests), 1)
                self.assertEqual(len(scenario.pending), 1)
                self.assertEqual(scenario.recovery["observations"][0]["body_prefix"], body)
                self.assertFalse(scenario.recovery["recovered"])

    def test_failed_new_request_is_recorded_and_never_replayed(self):
        scenario = self.scenario([(0.2, RuntimeError("HTTP 503")),
                                  (0.1, '{"rows":1,"text":"中文🙂"}')])
        with self.assertRaisesRegex(RuntimeError, "HTTP 503"):
            scenario.run()
        self.assertEqual(len(scenario.requests), 1)
        self.assertEqual(len(scenario.pending), 1)
        self.assertEqual(scenario.recovery["observations"][0]["exception"], "RuntimeError")
        self.assertFalse(scenario.recovery["recovered"])
        self.assertAlmostEqual(scenario.recovery["elapsed_seconds"], 0.2)


if __name__ == "__main__":
    unittest.main()
