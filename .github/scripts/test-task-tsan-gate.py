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

    def test_duplicate_label_preserves_evidence_without_running_twice(self) -> None:
        done = subprocess.CompletedProcess([], 0, PASS, b"")
        original = GATE.BOUNDS.COMMAND_TIMEOUT_SECONDS
        with tempfile.TemporaryDirectory() as temporary, patch.object(
            GATE.BOUNDS, "run_bounded", return_value=done,
        ) as run, patch("builtins.print"):
            logs = Path(temporary)
            GATE.run_command(["first"], logs, "same-target", 20)
            command_before = (logs / "same-target.command.json").read_bytes()
            output_before = (logs / "same-target.log").read_bytes()
            with self.assertRaises(FileExistsError):
                GATE.run_command(["second"], logs, "same-target", 20)
            run.assert_called_once_with(["first"], GATE.REPO, "same-target")
            self.assertEqual((logs / "same-target.command.json").read_bytes(), command_before)
            self.assertEqual((logs / "same-target.log").read_bytes(), output_before)
        self.assertEqual(GATE.BOUNDS.COMMAND_TIMEOUT_SECONDS, original)

    def test_invalid_evidence_labels_rejected_before_file_or_process_use(self) -> None:
        for label in ("", "../outside", "nested/name", r"nested\name", "a" * 201, "é", "a\nb", "Upper"):
            with self.subTest(label=label), patch.object(Path, "open") as opening, patch.object(
                GATE.BOUNDS, "run_bounded",
            ) as run:
                with self.assertRaises(ValueError):
                    GATE.run_command(["not-executed"], Path("unused"), label, 20)
                opening.assert_not_called()
                run.assert_not_called()

    def test_main_runs_seven_exact_tests_with_unique_logs_and_deduplicated_build(self) -> None:
        expected = (
            ("native_task_tsan_probe_test", "native_task_tsan_instrumentation_and_runtime_probe", True),
            ("native_task_driver_test", "native_task_driver_two_real_workers_overlap_without_same_task_overlap", False),
            ("native_task_driver_test", "native_task_driver_hot_yield_services_marker_before_cancellation", False),
            ("native_task_control_test", "native_task_control_frame_arbitration_and_references_execute_in_c", False),
            ("native_task_expression_pending_test", "native_task_expression_repeated_pending_preserves_lhs_and_starts_rhs_once", False),
            ("native_task_publishing_deadline_test", "native_task_publishing_cancel_tightens_unacked_child_after_timeout_result_race", False),
            ("native_task_cleanup_commit_test", "native_task_requested_cancel_tightens_unacked_child_before_cleanup_commit", False),
        )
        self.assertEqual(GATE.TARGETS, expected)
        calls = []

        def record(command, logs, label, timeout):
            calls.append((command, label, timeout))
            return PASS

        with tempfile.TemporaryDirectory() as temporary:
            logs = Path(temporary) / "fresh"
            with patch.object(
                GATE.sys, "argv", ["task-tsan-gate.py", "--logs", str(logs)],
            ), patch.object(GATE, "require_host", return_value="/test/clang"), patch.object(
                GATE, "run_command", side_effect=record,
            ), patch.dict(GATE.os.environ, {}, clear=True), patch("builtins.print"):
                GATE.main()
                self.assertEqual(GATE.os.environ["KU_NATIVE_REQUIRE_TSAN"], "1")
                self.assertEqual(GATE.os.environ["TSAN_OPTIONS"], "halt_on_error=1:exitcode=66")
                self.assertEqual(GATE.os.environ["KU_CC"], "/test/clang")

        self.assertEqual(len(calls), 11)  # Three metadata calls, one build, seven exact tests.
        self.assertEqual([label for _, label, _ in calls[:4]], [
            "commit", "rust-version", "clang-version", "build",
        ])
        unique_targets = list(dict.fromkeys(target for target, _, _ in expected))
        self.assertEqual(len(unique_targets), 6)
        build = ["cargo", "test", "--locked", "--no-run"]
        for target in unique_targets:
            build += ["--test", target]
        self.assertEqual(calls[3], (build, "build", 300))
        invocations = calls[4:]
        self.assertEqual(len(invocations), 7)
        labels = []
        for (command, label, timeout), (target, name, canary) in zip(invocations, expected):
            wanted = [
                "cargo", "test", "--locked", "--test", target, name,
                "--", "--exact", "--nocapture", "--test-threads=1",
            ]
            if canary:
                wanted.append("--ignored")
            self.assertEqual(command, wanted)
            self.assertEqual(label, f"{target}--{name}")
            self.assertRegex(label, r"^[a-z][a-z0-9_-]{0,199}$")
            self.assertEqual(timeout, 300)
            labels.append(label)
        self.assertEqual(len(set(labels)), 7)
        self.assertEqual(sum("--ignored" in command for command, _, _ in invocations), 1)

    def test_only_detector_probe_is_explicitly_ignored(self) -> None:
        self.assertEqual(len(GATE.TARGETS), 7)
        self.assertEqual(sum(canary for _, _, canary in GATE.TARGETS), 1)
        self.assertEqual(len({target for target, _, _ in GATE.TARGETS}), 6)
        self.assertEqual(len({(target, name) for target, name, _ in GATE.TARGETS}), 7)
        self.assertEqual(GATE.TSAN_OPTIONS, "halt_on_error=1:exitcode=66")
        self.assertNotIn("suppress", GATE.TSAN_OPTIONS)
        self.assertNotIn("ignore", GATE.TSAN_OPTIONS)


if __name__ == "__main__":
    unittest.main()
