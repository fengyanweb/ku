#!/usr/bin/env python3
"""Focused permanent contracts for deployment budgets and rolling failure."""

import importlib.util
import json
from pathlib import Path
import socket
import sys
import tempfile
import time
import unittest
from unittest.mock import Mock, patch

SPEC = importlib.util.spec_from_file_location("manage", Path(__file__).with_name("manage.py"))
M = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(M)


class DeploymentContracts(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(prefix="ku-deploy-regression-")
        self.root = Path(self.temporary.name)
        self.config = json.loads(Path(__file__).with_name("cluster.example.json").read_text())
        token = self.root / "token"
        token.write_text("a" * 32)
        token.chmod(0o600)
        self.config["control_token_file"] = str(token)
        self.path = self.root / "cluster.json"
        self.path.write_text(json.dumps(self.config))

    def tearDown(self):
        self.temporary.cleanup()

    def test_rolling_spare_is_in_the_same_total_database_budget(self):
        self.config["deployment_budget"]["database_connections"] = 23
        self.path.write_text(json.dumps(self.config))
        with self.assertRaisesRegex(ValueError, "database_connections"):
            M.load(self.path)

    def test_background_and_supervision_threads_cannot_disappear_from_budget(self):
        self.config["deployment_budget"]["per_instance"]["total_threads"] = 8
        self.path.write_text(json.dumps(self.config))
        with self.assertRaisesRegex(ValueError, "supervisor/launcher"):
            M.load(self.path)

    def test_existing_environment_cannot_silently_override_declared_capacity(self):
        config = M.load(self.path)
        slot = config["slots"][0]
        slot["env"]["KU_DB_MAX_CONNECTIONS"] = "100000"
        with self.assertRaisesRegex(ValueError, "conflicts"):
            M.environment(config, slot)

    def test_reference_proxy_disables_replay_and_hides_control_routes(self):
        rendered = M.render(M.load(self.path))
        self.assertIn("retries 0", rendered)
        self.assertIn("no option redispatch", rendered)
        self.assertIn("http-request del-header X-Ku-Control-Token", rendered)
        self.assertIn("deny_status 404 if { path_beg /__ku/ }", rendered)

    def test_control_timeout_does_not_claim_the_supervisor_has_retired(self):
        config = M.load(self.path)
        with patch.object(M, "call", side_effect=TimeoutError("bounded status timeout")), \
                patch.object(M, "windows_control_port_absent", return_value=(False, {"result": "inconclusive"})):
            self.assertEqual(M.status(config, config["slots"][0])["phase"], "unreachable")
        with patch.object(M, "call", side_effect=ConnectionRefusedError("no listener")):
            self.assertEqual(M.status(config, config["slots"][0])["phase"], "absent")

    def test_windows_timeout_releases_reservation_only_after_exclusive_vacancy_proof(self):
        config = M.load(self.path)
        slot = config["slots"][0]
        with patch.object(M.sys, "platform", "win32"), \
                patch.object(M, "call", side_effect=TimeoutError("connect expired")), \
                patch.object(M, "windows_control_port_absent", return_value=(True, {"result": "vacant"})) as proof:
            state = M.status(config, slot)
        self.assertEqual(state["phase"], "absent")
        self.assertEqual(state["control_failure"]["error_type"], "TimeoutError")
        self.assertEqual(state["control_presence"]["result"], "vacant")
        proof.assert_called_once_with(slot["control_addr"])

    def test_exclusive_probe_never_reuses_or_listens_and_retains_inconclusive_port(self):
        probe = Mock()
        probe.bind.side_effect = OSError(98, "address occupied or lingering connection")
        with patch.object(M.socket, "SO_EXCLUSIVEADDRUSE", -5, create=True), \
                patch.object(M.socket, "socket") as factory:
            factory.return_value.__enter__.return_value = probe
            vacant, evidence = M.windows_control_port_absent("127.0.0.1:12345")
        self.assertFalse(vacant)
        self.assertEqual(evidence["result"], "inconclusive")
        probe.setsockopt.assert_called_once_with(socket.SOL_SOCKET, -5, 1)
        probe.bind.assert_called_once_with(("127.0.0.1", 12345))
        probe.listen.assert_not_called()
        factory.return_value.__exit__.assert_called_once()

    def test_live_owned_supervisor_without_listener_still_prevents_duplicate_start(self):
        config = M.load(self.path)
        slot = config["slots"][0]
        process = Mock(pid=12345)
        process.poll.return_value = None
        with patch.dict(M.OWNED_SUPERVISORS, {slot["name"]: process}, clear=True), \
                patch.object(M, "call", side_effect=ConnectionRefusedError("not bound yet")), \
                patch.object(M, "windows_control_port_absent") as proof, \
                patch.object(M.subprocess, "Popen") as spawn:
            state = M.status(config, slot)
            self.assertEqual(state["phase"], "unreachable")
            self.assertEqual(state["pid"], 12345)
            self.assertTrue(state["owned_supervisor_alive"])
            with self.assertRaisesRegex(RuntimeError, "cannot prove absence"):
                M.start(config, slot)
            proof.assert_not_called()
            spawn.assert_not_called()

    def test_listening_unresponsive_control_stays_unreachable_with_actual_failure_stage(self):
        config = M.load(self.path)
        slot = config["slots"][0]
        with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as listener:
            listener.bind(("127.0.0.1", 0))
            listener.listen(1)
            slot["control_addr"] = "127.0.0.1:" + str(listener.getsockname()[1])
            started = time.monotonic()
            state = M.status(config, slot)
            elapsed = time.monotonic()-started
        self.assertEqual(state["phase"], "unreachable", state)
        self.assertEqual(state["control_failure"]["stage"], "response_headers", state)
        self.assertLess(elapsed, 3)
        if sys.platform == "win32":
            self.assertEqual(state["control_presence"]["result"], "inconclusive", state)

    @unittest.skipUnless(sys.platform == "win32", "actual Winsock exclusive-bind contract")
    def test_actual_windows_vacant_endpoint_does_not_reserve_a_phantom_supervisor(self):
        config = M.load(self.path)
        slot = config["slots"][0]
        with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as allocation:
            allocation.bind(("127.0.0.1", 0))
            slot["control_addr"] = "127.0.0.1:" + str(allocation.getsockname()[1])
        state = M.status(config, slot)
        self.assertEqual(state["phase"], "absent", state)
        self.assertEqual(state["control_failure"]["stage"], "connect", state)
        if state["control_failure"]["error_type"] != "ConnectionRefusedError":
            self.assertEqual(state["control_presence"]["result"], "vacant", state)

    def test_failed_new_instance_never_stops_old_or_loses_previous_program(self):
        config = M.load(self.path)
        old, _, new = config["slots"]
        binary = self.root / "new-program"
        binary.write_bytes(b"fixture")
        previous = new["program"]

        def current(_config, slot):
            return {"phase": "absent" if slot["name"] == "spare" else "ready"}

        stopped = []
        with patch.object(M, "status", side_effect=current), \
                patch.object(M, "start", side_effect=TimeoutError("new never ready")), \
                patch.object(M, "stop", side_effect=lambda _config, slot: stopped.append(slot["name"])), \
                patch.object(M, "proxy_state"):
            with self.assertRaisesRegex(TimeoutError, "new never ready"):
                M.roll(config, old["name"], new["name"], binary)
        self.assertEqual(stopped, ["spare"])
        self.assertEqual(new["program"], previous)
        saved = json.loads(self.path.read_text())
        self.assertEqual(saved["slots"][2]["program"], previous)
        self.assertNotIn("_token", saved)


if __name__ == "__main__":
    unittest.main()
