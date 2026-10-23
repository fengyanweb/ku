"""Clock and recovery counterexamples for the permanent comparison runner."""
import unittest
import copy
import json
import signal
import csv
import gzip
import tempfile
import types
from pathlib import Path
from unittest import mock

from linux_harness import (arrival_cpu, business_send_end, measurement_clock,
                           qualification, recovery_probe_views, upper_point, aggregate_rounds,
                           histogram_bucket, paired_interval, interleaved_orders,
                           overload_boundary, full_lifecycle_cost, Case, write_json, read_json,
                           paired_capacity_goodput, declared_case_stacks, sustained_case,
                           benchmark_limits, cases, GIB, MIB, java_shutdown_receipt,
                           service_termination_succeeded, JAVA_SHUTDOWN_PREFIX, JAVA_SHUTDOWN_STAGES)


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


class MeasurementRulesTest(unittest.TestCase):
    def report(self, count=100):
        return {'schema': 'ku-cross-stack-client-v5', 'valid_driver': True, 'configuration': clock(),
                'ledger': {'planned_reconciled': True, 'failure_fraction_planned': 0,
                           'results': {'correct': count}, 'correct_total': count,
                           'goodput_rps': count, 'transport_counts': {},
                           'correct_scheduled_to_complete': {'count': count,
                               'p99_ms': 1 if count >= 100 else None, 'max_ms': 1}},
                'service_cost': {'before': {'cpu_stat': {'usage_usec': 100}, 'memory_events': {}},
                                 'after': {'cpu_stat': {'usage_usec': 200}, 'memory_events': {}},
                                 'cpu_seconds_per_correct': .001,'full_lifecycle':{'available':True,'natural_reclamation_passed':True}}}

    def test_seeded_window_pairs_not_requests(self):
        orders = interleaved_orders(('ku', 'go', 'rust', 'node'))
        self.assertEqual(len(orders), 7)
        self.assertEqual(orders, interleaved_orders(('ku', 'go', 'rust', 'node')))
        self.assertTrue(all(set(v) == {'ku', 'go', 'rust', 'node'} for v in orders))
        a, b = [2, 4, 6, 8, 10, 12, 14], [1, 2, 3, 4, 5, 6, 7]
        ci = paired_interval(a, b, resamples=1000)
        self.assertEqual(ci['status'], 'estimated')
        self.assertAlmostEqual(ci['lower95'], 2)
        self.assertAlmostEqual(ci['upper95'], 2)
        self.assertEqual(ci, paired_interval(a, b, resamples=1000))
        self.assertEqual(paired_interval(a[:6], b[:6])['status'], 'insufficient_paired_windows')
        self.assertEqual(paired_interval(a, [0]*7)['status'], 'unavailable_nonpositive_or_missing_window')

    def test_capacity_pairs_small_memory_scope_and_real_config_do_not_use_common_defaults(self):
        summaries={'common':{'ku':{'qualified':True,'round_goodput_rps':[50]*7},
                             'go':{'qualified':True,'round_goodput_rps':[50]*7}},
                   'own_capacity':{'ku':{'qualified':True,'round_goodput_rps':[200]*7},
                                   'go':{'qualified':True,'round_goodput_rps':[100]*7}}}
        state={'capacity':{s:{'candidate_rate':rate,'upper_bound_censored':False,
                             'upper_bound_kind':'service_or_slo_failure',
                             'upper_bound_point':{'rate':rate+10,'gate_kind':'service_or_slo_failure',
                                 'qualification':{'reasons':['ok_p99_over_slo']}}}
                          for s,rate in [('ku',200),('go',100)]}}
        result=paired_capacity_goodput(summaries,state)['go']
        self.assertAlmostEqual(result['goodput_rps_ratio']['estimate'],2)
        self.assertTrue(result['eligible_for_capacity_target_claim'])
        state['capacity']['go']['upper_bound_censored']=True
        self.assertFalse(paired_capacity_goodput(summaries,state)['go']['eligible_for_capacity_target_claim'])
        summaries['own_capacity']['ku']['qualified']=False
        self.assertFalse(paired_capacity_goodput(summaries,state)['go']['eligible_for_operating_point_comparison'])
        summaries['own_capacity']['ku']['round_goodput_rps']=[]
        self.assertEqual(paired_capacity_goodput(summaries,state)['go']['goodput_rps_ratio']['status'],'unavailable_unpaired_windows')
        args=types.SimpleNamespace(case='c4-m268435456-mixed')
        self.assertEqual(sustained_case(args),(4,256*MIB,'mixed'))
        self.assertIn((4,256*MIB,'mixed'),list(cases(args)))
        self.assertEqual(declared_case_stacks(('ku','java','dotnet','cpp'),4,256*MIB,'mixed'),('ku','java','dotnet','cpp'))
        self.assertEqual(declared_case_stacks(('java','dotnet','cpp'),4,GIB,'json_escape'),())
        args.case='c4-m1073741824-cpu'
        with self.assertRaises(ValueError): sustained_case(args)
        self.assertEqual(benchmark_limits({}),{'max_active_requests':1024,'max_connections':4096,'max_pending_requests':128})
        self.assertEqual(benchmark_limits({'BENCH_MAX_ACTIVE':'8256','BENCH_MAX_CONNECTIONS':'8320','BENCH_MAX_PENDING':'128'})['max_active_requests'],8256)
        with self.assertRaises(ValueError): benchmark_limits({'BENCH_MAX_ACTIVE':'0'})

    def test_bounded_histogram_covers_int64_duration(self):
        for n in (0, 9999, 10000, 2559999, 2560000, 2579999, 100000000, 3000000000, (1 << 63)-1):
            upper = histogram_bucket(n)
            self.assertGreaterEqual(upper, n)
            self.assertLessEqual(upper, (1 << 63)-1)
            if n > 2560000:
                self.assertLessEqual(upper-n, max(10000, n/128+1))

    def aggregate(self, folder, count, repeats=7, changing=False):
        reports = []
        for index in range(repeats):
            path = Path(folder)/f'{count}-{index}.gz'
            with gzip.open(path, 'wt', newline='') as target:
                writer = csv.DictWriter(target, fieldnames=('type', 'terminal', 'scheduled_ns', 'completed_ns'))
                writer.writeheader()
                for _ in range(count):
                    writer.writerow({'type': 'ok', 'terminal': 'correct', 'scheduled_ns': 0, 'completed_ns': 1000000})
            report = self.report(count)
            report['events'] = {'path': str(path)}
            if changing and index:
                report['configuration']['rate'] = 200
            reports.append(report)
        return reports

    def test_core_p99_requires1000_not_only_non_null_p99(self):
        with tempfile.TemporaryDirectory() as folder:
            reports = self.aggregate(folder, 20)
            result = aggregate_rounds(reports, 'ok')
            self.assertIsNotNone(result['pooled_correct_scheduled_to_complete']['ok']['p99_ms'])
            self.assertFalse(result['qualified'])
            self.assertEqual(result['status'], 'indeterminate_tail_sample_size')
            reports = self.aggregate(folder, 150)
            self.assertTrue(aggregate_rounds(reports, 'ok')['qualified'])
            reports[-1]['service_cost']['full_lifecycle']['natural_reclamation_passed']=False
            self.assertFalse(aggregate_rounds(reports,'ok')['qualified'])
            reports[-1]['service_cost']['full_lifecycle']['natural_reclamation_passed']=True
            self.assertFalse(aggregate_rounds(reports[:6], 'ok')['qualified'])
            reports[-1]['ledger']['correct_total'] += 1
            with self.assertRaisesRegex(ValueError, 'event count'):
                aggregate_rounds(reports, 'ok')

    def test_different_target_configuration_must_not_be_merged(self):
        with tempfile.TemporaryDirectory() as folder:
            reports = self.aggregate(folder, 150, changing=True)
            with self.assertRaisesRegex(ValueError, 'different driver'):
                aggregate_rounds(reports, 'ok')

    def test_overload_needs_valid_generation_and_service_boundary(self):
        report = self.report()
        self.assertFalse(overload_boundary(report, 'ok')['boundary_observed'])
        report['ledger']['results']['rejected_503'] = 1
        self.assertTrue(overload_boundary(report, 'ok')['boundary_observed'])
        report['valid_driver'] = False
        self.assertEqual(overload_boundary(report, 'ok')['status'], 'measurement_invalid_censored')
        report['valid_driver'] = True
        report['ledger']['transport_counts']['status:500'] = 1
        self.assertIn('http_500', qualification(report, 'ok')['reasons'])
        self.assertFalse(overload_boundary(report, 'ok')['boundary_observed'])

    def test_post_driver_cost_kept_even_when_forced(self):
        report = self.report()
        end = {'cpu_stat': {'usage_usec': 1200}, 'process_count': 0}
        result = full_lifecycle_cost(report, end, False, {'forced': True}, {'remaining_jobs': 3})
        self.assertAlmostEqual(result['cpu_seconds'], .0011)
        self.assertAlmostEqual(result['post_driver_cpu_seconds'], .001)
        self.assertFalse(result['natural_reclamation_passed'])
        self.assertTrue(result['forced_termination'])
        self.assertIsNone(result['useless_work_cpu_seconds'])

    def test_fixture5s_failure_still_observes_real_physical_completion(self):
        now = [0.0]
        quiet = {'active': 0, 'waiting_timer': 0, 'waiting_tcp': 0, 'actual_cpu_jobs': 0}
        domain = types.SimpleNamespace(
            snapshot=lambda: {'cpu_stat': {'usage_usec': int(now[0]*1e6)}, 'process_count': 1})
        service = types.SimpleNamespace(stopped=False, domain=domain, startup={},
            metrics=lambda: dict(quiet, active=int(now[0] < 6)),
            freeze=lambda value: None)
        service.stop = mock.Mock(side_effect=AssertionError('must await actual completion'))
        case = object.__new__(Case)
        case.instances, case.mode = {'go': service}, 'persistent_frozen_between_turns'
        case.args = types.SimpleNamespace(physical_settle_seconds=45)
        case.state, case.save, case.check_memory = {'history': []}, lambda: None, lambda: None
        with tempfile.TemporaryDirectory() as folder:
            path = Path(folder)/'report.json'
            write_json(path, self.report())
            with mock.patch('linux_harness.time.monotonic', side_effect=lambda: now[0]), \
                 mock.patch('linux_harness.time.sleep', side_effect=lambda delay: now.__setitem__(0, now[0]+delay)):
                case.idle('go', path)
            cost = read_json(path)['service_cost']['full_lifecycle']
        self.assertFalse(cost['settle_observation']['fixture5s_passed'])
        self.assertTrue(cost['natural_reclamation_passed'])
        self.assertGreater(cost['post_driver_cpu_seconds'], 5)
        self.assertLess(len(cost['settle_observation']['samples']), 128)
        service.stop.assert_not_called()


class ServiceTerminationTest(unittest.TestCase):
    def complete(self):
        return {'exit_code':143, 'requested_signal':signal.SIGTERM, 'process_id':123,
                'expected_listeners':4, 'forced':False, 'cgroup_removed':True,
                'before':{'memory_events':{'oom':0,'oom_kill':0,'oom_group_kill':0}},
                'after':{'process_count':0, 'threads':0, 'fd':0, 'memory_events':{'oom':0,'oom_kill':0,'oom_group_kill':0}},
                'java_shutdown':{'status':'recorded', 'receipt':{
                    'schema':'ku-java-shutdown-v1','pid':123,'expected_listeners':4,'listeners':4,
                    'complete':True,'completed_stages':list(JAVA_SHUTDOWN_STAGES),
                    'elapsed_ns':20_000_000,'deadline_expired':False,
                    'active':0,'waiting_timer':0,'actual_cpu_jobs':0,'database_pending':0}}}

    def test_only_expected_java_signal_and_complete_real_close_receipt_pass(self):
        value = self.complete()
        self.assertTrue(service_termination_succeeded('java', value))
        for stack in ('ku','go','rust','node','dotnet','cpp'):
            self.assertFalse(service_termination_succeeded(stack, value))
            self.assertTrue(service_termination_succeeded(stack, dict(value, exit_code=0)))
        for field, wrong in (('exit_code',0),('exit_code',-signal.SIGTERM),('exit_code',1),
                             ('exit_code',137),('requested_signal',None),('requested_signal',signal.SIGINT),
                             ('forced',True),('cgroup_removed',False),('process_id',124),('expected_listeners',1)):
            with self.subTest(field=field,wrong=wrong):
                changed=copy.deepcopy(value); changed[field]=wrong
                self.assertFalse(service_termination_succeeded('java',changed))
        for name in ('oom','oom_kill','oom_group_kill'):
            for where in ('before','after'):
                changed=copy.deepcopy(value); changed[where]['memory_events'][name]=1
                self.assertFalse(service_termination_succeeded('java',changed))
        for name in ('process_count','threads','fd'):
            changed=copy.deepcopy(value); changed['after'][name]=1
            self.assertFalse(service_termination_succeeded('java',changed))
            del changed['after'][name]
            self.assertFalse(service_termination_succeeded('java',changed))
        changed=copy.deepcopy(value); del changed['before']['memory_events']
        self.assertFalse(service_termination_succeeded('java',changed))

    def test_missing_failed_truncated_or_mismatched_receipt_cannot_pass(self):
        value=self.complete()
        for evidence in ({},{'status':'missing'},{'status':'invalid'},{'status':'truncated'},
                         {'status':'duplicate'},{'status':'oversized'},{'status':'recorded','receipt':[]}):
            self.assertFalse(service_termination_succeeded('java',dict(value,java_shutdown=evidence)))
        receipt=value['java_shutdown']['receipt']
        for field,wrong in (('complete',False),('complete',1),('deadline_expired',True),
                            ('elapsed_ns',4_500_000_000),('elapsed_ns',-1),('elapsed_ns',True),
                            ('schema','other'),('pid',999),('listeners',3),('expected_listeners',3),
                            ('active',1),('waiting_timer',1),('actual_cpu_jobs',1),('database_pending',1),
                            ('active',False),('completed_stages',JAVA_SHUTDOWN_STAGES[:-1]),
                            ('completed_stages',list(reversed(JAVA_SHUTDOWN_STAGES))),('error','TimeoutException')):
            with self.subTest(field=field,wrong=wrong):
                changed=dict(receipt); changed[field]=wrong
                evidence={'status':'recorded','receipt':changed}
                self.assertFalse(service_termination_succeeded('java',dict(value,java_shutdown=evidence)))

    def test_receipt_log_read_is_bounded_and_requires_one_complete_line(self):
        receipt=self.complete()['java_shutdown']['receipt']
        encoded=JAVA_SHUTDOWN_PREFIX+json.dumps(receipt).encode()+b'\n'
        with tempfile.TemporaryDirectory() as directory:
            path=Path(directory)/'service.log'
            self.assertEqual(java_shutdown_receipt(path)['status'],'missing')
            path.write_bytes(b'ready\n'+encoded)
            self.assertEqual(java_shutdown_receipt(path),{'status':'recorded','receipt':receipt})
            for body,status in ((encoded[:-1],'truncated'),(JAVA_SHUTDOWN_PREFIX+b'{bad}\n','invalid'),
                                (encoded+encoded,'duplicate'),(JAVA_SHUTDOWN_PREFIX+b'x'*2048+b'\n','oversized'),
                                (b'ready\n','missing'),(encoded+b'x'*9000+b'\n','missing')):
                path.write_bytes(body)
                self.assertEqual(java_shutdown_receipt(path)['status'],status)


if __name__ == '__main__':
    unittest.main()
