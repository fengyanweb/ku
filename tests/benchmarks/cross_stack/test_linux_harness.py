"""Clock and recovery counterexamples for the permanent comparison runner."""
import unittest

from linux_harness import (arrival_cpu, business_send_end, measurement_clock,
                           qualification, recovery_probe_views, upper_point)


def clock(origin=100_000_000_000, uncertainty=1000):
    return {'origin_monotonic_status': 'available', 'origin_monotonic_ns': origin,
            'origin_monotonic_uncertainty_ns': uncertainty}


def event(rid, planned, completed, terminal='correct'):
    return {'rid': rid, 'scheduled_ns': str(planned), 'completed_ns': str(completed),
            'terminal': terminal}


class ClockAndRecoveryTest(unittest.TestCase):
    def test_missing_or_uncertain_anchor_is_not_zero_or_server_failure(self):
        self.assertFalse(measurement_clock({})['available'])
        self.assertTrue(measurement_clock(clock(uncertainty=1_000_000))['available'])
        self.assertFalse(measurement_clock(clock(uncertainty=1_000_001))['available'])
        report = {'valid_driver': True, 'configuration': {}, 'ledger': {
            'planned_reconciled': True, 'failure_fraction_planned': 0,
            'correct_scheduled_to_complete': {'count': 100, 'p99_ms': 1}},
            'service_cost': {'before': {'memory_events': {}}, 'after': {'memory_events': {}}}}
        gate = qualification(report, 'ok')
        self.assertEqual(gate['reasons'], ['measurement_monotonic_clock_unavailable'])
        upper = upper_point(16, report, gate, 'retained/client.json')
        self.assertEqual(upper['gate_kind'], 'measurement_invalid_censored')
        self.assertTrue(upper['censored'])

    def test_wall_clock_rollback_does_not_change_actual_send_end(self):
        value = dict(clock(), origin_unix_ns=1_000_000_000_000,
                     last_actual_sent_offset_ns=15_000_000_000,
                     last_actual_sent_monotonic_ns=115_000_000_000,
                     last_actual_sent_unix_ns=1_014_730_694_027)
        observed = business_send_end(value)
        self.assertEqual(observed['status'], 'observed')
        self.assertEqual(observed['monotonic_ns'], 115_000_000_000)
        self.assertEqual(observed['unix_ns_display_only'], value['last_actual_sent_unix_ns'])
        value['last_actual_sent_monotonic_ns'] += 1
        self.assertEqual(business_send_end(value)['status'],
                         'unknown_invalid_or_inconsistent_client_field')

    def test_missing_or_no_send_is_explicit(self):
        self.assertIsNone(business_send_end(clock())['monotonic_ns'])
        result = business_send_end(dict(clock(), last_actual_sent_monotonic_ns=None))
        self.assertEqual(result['status'], 'not_applicable_no_actual_business_send')
        self.assertIsNone(result['monotonic_ns'])

    def test_saturated_driver_censors_capacity_instead_of_blame_server(self):
        report = {'valid_driver': True, 'configuration': clock(), 'ledger': {
            'planned_reconciled': True, 'failure_fraction_planned': 0,
            'correct_scheduled_to_complete': {'count': 100, 'p99_ms': 1}},
            'service_cost': {'before': {'memory_events': {}}, 'after': {'memory_events': {}},
                             'driver_headroom': {'saturated': True}}}
        gate = qualification(report, 'ok')
        self.assertEqual(gate['reasons'], ['driver_cpu_saturated'])
        self.assertEqual(upper_point(10000, report, gate, 'retained/client.json')['gate_kind'],
                         'driver_invalid_censored')

    def test_actual_send_tail_never_rescues_primary_five_second_gate(self):
        rows = [event('late', 6_100_000_000, 6_200_000_000),
                event('coverage', 7_000_000_000, 7_100_000_000, 'timeout')]
        views = recovery_probe_views(rows, 99_000_000_000,
                                     {'planned_cutoff': 100_000_000_000,
                                      'actual_send_end': 102_000_000_000})
        self.assertFalse(views['planned_cutoff']['recovered'])
        self.assertIsNone(views['planned_cutoff']['first_complete_new_probe'])
        self.assertTrue(views['actual_send_end']['recovered'])
        self.assertEqual(views['planned_cutoff']['probe_phases']['later'],
                         {'correct': 1, 'timeout': 1})

    def test_anchor_uncertainty_cannot_turn_boundary_result_into_pass(self):
        rows = [event('boundary', 4_999_900_000, 4_999_950_000),
                event('coverage', 6_000_000_000, 6_100_000_000, 'timeout')]
        value = recovery_probe_views(rows, 100_000_000_000,
                                     {'cutoff': 100_000_000_000}, uncertainty_ns=100_000)['cutoff']
        self.assertIsNone(value['recovered'])
        self.assertEqual(value['status'], 'unknown_boundary_clock_uncertainty')
        self.assertEqual(value['boundary_ambiguous_correct_probes'], 1)
        clear = recovery_probe_views([event('clear', 100_000_000, 200_000_000)],
                                     100_000_000_000, {'cutoff': 100_000_000_000},
                                     uncertainty_ns=100_000)['cutoff']
        self.assertTrue(clear['recovered'])
        self.assertLess(clear['first_complete_new_probe']['complete_after_seconds_upper_bound'], 5)

    def test_incomplete_probe_coverage_is_unknown(self):
        value = recovery_probe_views([event('failed', 1_000_000_000, 2_000_000_000, 'timeout')],
                                     100_000_000_000, {'cutoff': 100_000_000_000})['cutoff']
        self.assertIsNone(value['recovered'])
        self.assertFalse(value['observation_covers_full5s'])

    def test_arrival_cpu_uses_monotonic_despite_realtime_step(self):
        samples = [{'domains': [{'label': 'service', 'monotonic_ns': mono,
                                'wall_time_ns': wall, 'cpu_stat': {'usage_usec': cpu}}]}
                   for mono, wall, cpu in ((0, 1_000_000_000_000, 100),
                                          (1_000_000_000, 999_000_000_000, 1100),
                                          (2_000_000_000, 1_000_000_000_000, 2100))]
        value = arrival_cpu(samples, 'service', 500_000_000, 1_500_000_000)
        self.assertTrue(value['available'])
        self.assertAlmostEqual(value['cpu_seconds'], .001)


if __name__ == '__main__':
    unittest.main()
