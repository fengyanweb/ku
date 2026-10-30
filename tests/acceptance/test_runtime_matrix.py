#!/usr/bin/env python3
"""Regression contracts for real-counter failures and bounded Windows first use."""
import math
import os
import sys
import unittest
from unittest.mock import patch

sys.dont_write_bytecode = True
import runtime_matrix as matrix
import multiloop_load as load


def counters(handles=80, rss=6000000):
    return {"handles": handles, "rss_bytes": rss, "threads": 13, "cpu_seconds": .25}


class ResourceMeasurementContracts(unittest.TestCase):
    def test_missing_baseline_is_failure_without_synthetic_delta(self):
        result = matrix.resource_recovery_check({"passed": True},
            {"sample_error": "TimeoutExpired"}, counters(83), "handles")
        self.assertFalse(result["passed"])
        self.assertIsNone(result["descriptor_delta"])
        self.assertIsNone(result["rss_delta_bytes"])
        self.assertIn("baseline", result["sample_errors"])

    def test_counter_error_cannot_pass_even_if_fields_are_present(self):
        invalid = dict(counters(), sample_error="OSError")
        result = matrix.resource_recovery_check({"passed": True}, counters(), invalid, "handles")
        self.assertFalse(result["passed"])
        self.assertIn("final", result["sample_errors"])

    def test_original_two_handle_limit_is_unchanged(self):
        for delta in (0, 2, 3, 10):
            with self.subTest(delta=delta):
                result = matrix.resource_recovery_check({"passed": True}, counters(), counters(80 + delta), "handles")
                self.assertEqual(result["passed"], delta <= 2)
                self.assertEqual(result["descriptor_delta"], delta)

    def test_cpu_errors_are_not_zero_samples(self):
        for value in (None, math.inf, math.nan, -1, True):
            with self.subTest(value=value):
                self.assertTrue(matrix.resource_sample_errors(dict(counters(), cpu_seconds=value), "handles", True))

    def initialization(self, recovered_handles):
        samples = [counters(value) for value in recovered_handles for _ in range(3)]
        with patch.object(matrix, "cpu_sample", side_effect=samples), \
             patch.object(matrix, "recover", return_value={"passed": True}), \
             patch.object(matrix, "call", return_value=("success", .01, None)):
            service = type("Service", (), {"child": type("Child", (), {"pid": 123})()})()
            return matrix.windows_resource_initialization(service, counters(70))

    def test_cold_first_use_is_retained_with_three_stable_repeats(self):
        result = self.initialization([80, 80, 80, 80])
        self.assertTrue(result["passed"])
        self.assertEqual(result["cold_to_first_recovered_cycle"]["descriptor_delta"], 10)
        self.assertEqual(len(result["cycles"]), 4)

    def test_repeated_growth_is_not_rebased_each_cycle(self):
        result = self.initialization([80, 81, 82, 83])
        self.assertFalse(result["passed"])
        self.assertEqual(result["cycles"][-1]["compared_to_first_recovered_cycle"]["descriptor_delta"], 3)

    @unittest.skipUnless(os.name == "nt", "actual Win32 counters require Windows")
    def test_real_windows_sampler_closes_its_diagnostic_handles(self):
        import ctypes
        from ctypes import wintypes
        kernel = ctypes.WinDLL("kernel32", use_last_error=True)
        kernel.GetProcessHandleCount.argtypes = [wintypes.HANDLE, ctypes.POINTER(wintypes.DWORD)]
        kernel.GetProcessHandleCount.restype = wintypes.BOOL

        def own_handles():
            count = wintypes.DWORD()
            self.assertTrue(kernel.GetProcessHandleCount(wintypes.HANDLE(-1), ctypes.byref(count)))
            return count.value

        load.windows_process_sample(os.getpid())
        before = own_handles()
        for _ in range(32):
            sample = load.process_sample(os.getpid())
            self.assertFalse(matrix.resource_sample_errors(sample, "handles", True), sample)
            self.assertGreater(sample["threads"], 0)
        self.assertLessEqual(own_handles(), before)

    @unittest.skipUnless(os.name == "nt", "actual Win32 counter error requires Windows")
    def test_real_windows_invalid_pid_reports_error(self):
        sample = load.process_sample(0x7FFFFFFF)
        self.assertIn("sample_error", sample)
        self.assertNotIn("handles", sample)


if __name__ == "__main__":
    unittest.main()
