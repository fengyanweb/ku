#!/usr/bin/env python3
"""Focused permanent contracts for deployment budgets and rolling failure."""

import importlib.util
import json
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

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
        with patch.object(M, "call", side_effect=TimeoutError("bounded status timeout")):
            self.assertEqual(M.status(config, config["slots"][0])["phase"], "unreachable")
        with patch.object(M, "call", side_effect=ConnectionRefusedError("no listener")):
            self.assertEqual(M.status(config, config["slots"][0])["phase"], "absent")

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
