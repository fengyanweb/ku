"""Private PostgreSQL fixture setup must not broaden its parent permissions."""
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
    def scenario(self, responses, *, initial_time=100.0, progress_seconds=0):
        clock = SimpleNamespace(now=initial_time)
        expected = {"rows": 1, "text": "中文🙂"}
        pending = list(responses)
        requests, progress, recovery = [], [], {}
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

        def execute():
            with patch.object(fixture, "time", SimpleNamespace(monotonic=lambda: clock.now)):
                fixture.recover_new_reads(
                    read, lambda: last, expected, recovery, began=100.0, deadline_seconds=5,
                    max_new_reads=8,
                    intermediate_errors=["query_error", "execution_unknown", "acquire_timeout"],
                    start_event="all pressure requests completed", progress=mark)

        return SimpleNamespace(run=execute, requests=requests, progress=progress,
                               recovery=recovery, pending=pending, expected=expected)

    def test_transient_result_then_distinct_read_success_keeps_complete_ledger(self):
        for code in ("acquire_timeout", "execution_unknown", "query_error"):
            with self.subTest(code=code):
                scenario = self.scenario([(0.109, "error:" + code),
                                          (0.016, '{"rows":1,"text":"中文🙂"}')])
                scenario.run()
                self.assertEqual(scenario.requests, [("/read", 105.0, 100.0),
                                                     ("/read", 105.0, 100.109)])
                self.assertTrue(scenario.recovery["recovered"])
                self.assertFalse(scenario.recovery["failed_request_replay"])
                observations = scenario.recovery["observations"]
                self.assertEqual([row["sequence"] for row in observations], [1, 2])
                self.assertEqual(observations[0]["body_prefix"], "error:" + code)
                self.assertAlmostEqual(observations[0]["recovery_elapsed_seconds"], 0.109)
                self.assertEqual(json.loads(observations[1]["body_prefix"]), scenario.expected)

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

    def test_response_at_deadline_is_not_recovery(self):
        scenario = self.scenario([(5, '{"rows":1,"text":"中文🙂"}')])
        with self.assertRaises(TimeoutError):
            scenario.run()
        self.assertEqual(len(scenario.requests), 1)
        self.assertEqual(len(scenario.recovery["observations"]), 1)
        self.assertFalse(scenario.recovery["recovered"])
        self.assertEqual(scenario.recovery["finished_monotonic"], 105.0)

    def test_all_eight_allowed_errors_exhaust_new_read_count_without_replay(self):
        scenario = self.scenario([(0.125, "error:acquire_timeout")] * 8)
        with self.assertRaises(AssertionError):
            scenario.run()
        self.assertEqual(len(scenario.requests), 8)
        self.assertEqual([path for path, _, _ in scenario.requests], ["/read"] * 8)
        self.assertEqual(len(scenario.recovery["observations"]), 8)
        self.assertEqual(scenario.recovery["elapsed_seconds"], 1.0)
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
