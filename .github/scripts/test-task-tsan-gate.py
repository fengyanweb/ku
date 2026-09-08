#!/usr/bin/env python3
"""Read-only contracts for the required TSan runner, reusing bounded-runner mocks."""
from __future__ import annotations

import importlib.util
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch

sys.dont_write_bytecode = True
SPEC = importlib.util.spec_from_file_location(
    "ku_tsan_gate", Path(__file__).with_name("task-tsan-gate.py")
)
assert SPEC is not None and SPEC.loader is not None
GATE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(GATE)
PASS = b"test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out;\n"


class TSanGateContracts(unittest.TestCase):
    def test_exact_execution_required(self) -> None:
        GATE.require_pass(PASS)
        for bad in (
            b"", PASS.replace(b"1 passed", b"0 passed"), PASS.replace(b"1 passed", b"11 passed"),
            PASS.replace(b"0 ignored", b"1 ignored"), PASS.replace(b"ok.", b"FAILED."),
            PASS + PASS, PASS + b"skip: no compiler\n", PASS + b"SKIPPED: unavailable runtime\n",
        ):
            with self.subTest(output=bad), self.assertRaises(RuntimeError):
                GATE.require_pass(bad)

    def test_task_race_is_not_a_pass_even_with_success_summary(self) -> None:
        for diagnostic in (b"WARNING: ThreadSanitizer: data race\n", b"FATAL: ThreadSanitizer: mapping\n"):
            with self.subTest(diagnostic=diagnostic), self.assertRaises(RuntimeError):
                GATE.require_pass(PASS + diagnostic)
        GATE.require_pass(PASS + b"expected isolated TSan canary diagnostic:\n", canary=True)

    def test_missing_compiler_and_wrong_host_fail_without_skip(self) -> None:
        with patch.object(GATE.platform, "system", return_value="Linux"), patch.object(
            GATE.platform, "machine", return_value="x86_64"
        ), patch.object(GATE.shutil, "which", return_value=None):
            with self.assertRaisesRegex(RuntimeError, "no fallback"):
                GATE.require_host()
        with patch.object(GATE.platform, "system", return_value="Windows"), patch.object(
            GATE.shutil, "which"
        ) as discover:
            with self.assertRaisesRegex(RuntimeError, "actual Linux"):
                GATE.require_host()
            discover.assert_not_called()

    def test_runtime_failure_is_preserved_and_bound_restored(self) -> None:
        original = GATE.BOUNDS.COMMAND_TIMEOUT_SECONDS
        for failure in (SystemExit("TSan mmap failed"), OSError("compiler missing")):
            with tempfile.TemporaryDirectory() as temporary, patch.object(
                GATE.BOUNDS, "run_bounded", side_effect=failure
            ):
                with self.assertRaisesRegex(RuntimeError, "probe failed"):
                    GATE.run_command(["fake"], Path(temporary), "probe", 20)
                self.assertIn(str(failure), (Path(temporary) / "probe.failure.txt").read_text())
            self.assertEqual(original, GATE.BOUNDS.COMMAND_TIMEOUT_SECONDS)

    def test_stderr_is_checked_and_command_is_logged(self) -> None:
        done = subprocess.CompletedProcess([], 0, PASS, b"skip: no runtime\n")
        with tempfile.TemporaryDirectory() as temporary, patch.object(
            GATE.BOUNDS, "run_bounded", return_value=done
        ), patch("builtins.print"):
            output = GATE.run_command(["cargo", "test"], Path(temporary), "probe", 20)
            with self.assertRaises(RuntimeError):
                GATE.require_pass(output)
            self.assertEqual((Path(temporary) / "probe.log").read_bytes(), output)
            self.assertIn("cargo", (Path(temporary) / "probe.command.json").read_text())

    def test_only_detector_probe_is_explicitly_ignored(self) -> None:
        self.assertEqual(len(GATE.TARGETS), 6)
        self.assertEqual(sum(canary for _, _, canary in GATE.TARGETS), 1)
        self.assertEqual(len({target for target, _, _ in GATE.TARGETS}), 6)
        self.assertEqual(GATE.TSAN_OPTIONS, "halt_on_error=1:exitcode=66")
        self.assertNotIn("suppress", GATE.TSAN_OPTIONS)
        self.assertNotIn("ignore", GATE.TSAN_OPTIONS)


if __name__ == "__main__":
    unittest.main()
