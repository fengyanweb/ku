#!/usr/bin/env python3
"""Focused permanent contracts for deployment budgets and rolling failure."""

import importlib.util
import json
from pathlib import Path
import socket
import sys
import tempfile
import threading
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

    def caddy_config(self):
        self.config.update(proxy_kind="caddy", caddy_admin_addr="127.0.0.1:19991", caddy_health_addr="127.0.0.1:19992")
        budget = self.config["deployment_budget"]
        budget.update(management_threads=34, management_memory_bytes=128*1024*1024)
        budget["total_threads"] = 3*budget["per_instance"]["total_threads"]+34
        budget["reserved_memory_bytes"] = 3*budget["per_instance"]["reserved_memory_bytes"]+budget["management_memory_bytes"]
        self.path.write_text(json.dumps(self.config))
        return M.load(self.path)

    def test_caddy_admin_is_loopback_unique_and_separate_from_business(self):
        for address in ("0.0.0.0:19991", "127.0.0.1:18081"):
            self.caddy_config()
            self.config["caddy_admin_addr"] = address
            self.path.write_text(json.dumps(self.config))
            with self.assertRaisesRegex(ValueError, "127.0.0.1|distinct"):
                M.load(self.path)

    def test_caddy_proxy_cost_cannot_disappear_from_deployment_reservations(self):
        for field in ("management_threads", "management_memory_bytes", "reserved_memory_bytes"):
            self.caddy_config()
            self.config["deployment_budget"][field] = 0
            self.path.write_text(json.dumps(self.config))
            with self.assertRaisesRegex(ValueError, "Caddy|reserved_memory_bytes"):
                M.load(self.path)

    def test_caddy_render_preserves_control_health_and_disables_both_retry_layers(self):
        config = self.caddy_config()
        rendered = json.loads(M.render(config))
        servers = rendered["apps"]["http"]["servers"]
        public = servers["ku_public"]
        self.assertEqual(public["routes"][0]["handle"][0]["status_code"], 404)
        self.assertIn("/__ku/*", public["routes"][0]["match"][0]["path"])
        proxy = public["routes"][1]["handle"][1]
        self.assertEqual(proxy["load_balancing"]["retries"], 0)
        self.assertEqual(proxy["load_balancing"]["try_duration"], 0)
        self.assertFalse(proxy["transport"]["keep_alive"]["enabled"])
        self.assertEqual(proxy["health_checks"]["active"]["upstream"], config["caddy_health_addr"])
        for slot, route in zip(config["slots"], servers["ku_health"]["routes"]):
            self.assertEqual(route["match"][0]["header"]["X-Ku-Proxy-Upstream"], [slot["listen_addr"]])
            self.assertEqual(route["handle"][0]["upstreams"][0]["dial"], slot["control_addr"])
            self.assertEqual(route["handle"][0]["headers"]["request"]["set"]["X-Ku-Control-Token"], [config["_token"]])

    def test_caddy_maintenance_keeps_fixed_state_and_removes_only_selected_backend(self):
        config = self.caddy_config()
        config["proxy_slot_states"] = {"a": "maint"}
        proxy = json.loads(M.render(config))["apps"]["http"]["servers"]["ku_public"]["routes"][1]["handle"][1]
        self.assertEqual([u["dial"] for u in proxy["upstreams"]], [s["listen_addr"] for s in config["slots"][1:]])
        self.config["proxy_slot_states"] = {"unknown": "ready"}
        self.path.write_text(json.dumps(self.config))
        with self.assertRaisesRegex(ValueError, "fixed slot"):
            M.load(self.path)

    def test_caddy_failed_reload_does_not_claim_state_changed_or_overwrite_foreign_config(self):
        config = self.caddy_config()
        with patch.object(M, "caddy_admin", return_value="{}") as admin:
            with self.assertRaisesRegex(RuntimeError, "owned deployment"):
                M.proxy_state(config, config["slots"][0], "maint")
            admin.assert_called_once_with(config, "GET", "/config/")
        with patch.object(M, "caddy_admin", side_effect=[M.render(config), RuntimeError("rejected load")]):
            with self.assertRaisesRegex(RuntimeError, "rejected load"):
                M.proxy_state(config, config["slots"][0], "maint")
        self.assertNotIn("proxy_slot_states", config)
        self.assertNotIn("proxy_slot_states", json.loads(self.path.read_text()))

    def test_caddy_registration_and_initial_optimistic_metric_do_not_count_as_ready(self):
        config = self.caddy_config()
        config["operation_timeout_seconds"] = .01
        config["_caddy_loaded_at"] = time.monotonic()
        with patch.object(M, "caddy_admin") as admin:
            with self.assertRaisesRegex(TimeoutError, "active health"):
                M.wait_proxy_ready(config, config["slots"][0])
            admin.assert_not_called()
        config["_caddy_loaded_at"] = time.monotonic()-11
        address = config["slots"][0]["listen_addr"]
        responses = [json.dumps([{"address": address}]), f'caddy_reverse_proxy_upstreams_healthy{{upstream="{address}"}} 0']
        with patch.object(M, "caddy_admin", side_effect=lambda _config, _method, path, **_: responses[0 if path.endswith("upstreams") else 1]):
            with self.assertRaisesRegex(TimeoutError, "active health"):
                M.wait_proxy_ready(config, config["slots"][0])

    def test_caddy_admin_absolute_deadline_includes_slow_headers(self):
        config = self.caddy_config()
        finished = threading.Event()
        with socket.socket() as listener:
            listener.bind(("127.0.0.1", 0))
            listener.listen(1)
            config["caddy_admin_addr"] = "127.0.0.1:"+str(listener.getsockname()[1])

            def drip():
                with listener.accept()[0] as stream:
                    stream.recv(4096)
                    for byte in b"HTTP/1.0 200 OK\r\n":
                        if finished.wait(.03):
                            break
                        try:
                            stream.sendall(bytes([byte]))
                        except OSError:
                            break

            thread = threading.Thread(target=drip, daemon=True)
            thread.start()
            started = time.monotonic()
            try:
                with self.assertRaises((TimeoutError, socket.timeout)):
                    M.caddy_admin(config, "GET", "/config/", deadline=started+.08)
                self.assertLess(time.monotonic()-started, .5)
            finally:
                finished.set()
                thread.join(timeout=1)
            self.assertFalse(thread.is_alive())

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
