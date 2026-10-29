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
from task_cpu_window import generation_gate, driver_lifecycle_cpu

from linux_harness import (arrival_cpu, business_send_end, measurement_clock, quiet_metrics,
                           qualification, recovery_probe_views, upper_point, aggregate_rounds,
                           histogram_bucket, paired_interval, interleaved_orders,
                           overload_boundary, full_lifecycle_cost, Case, Harness, write_json, read_json,
                           paired_capacity_goodput, declared_case_stacks, sustained_case,
                           benchmark_limits, cases, GIB, MIB, java_shutdown_receipt,
                           service_termination_succeeded, JAVA_SHUTDOWN_PREFIX, JAVA_SHUTDOWN_STAGES)
from linux_harness import result_assessment


def clock(origin=100_000_000_000, uncertainty=1000):
    return {'origin_monotonic_status': 'available', 'origin_monotonic_ns': origin,
            'origin_monotonic_uncertainty_ns': uncertainty}


def event(rid, planned, completed, terminal='correct'):
    return {'rid': rid, 'scheduled_ns': str(planned), 'completed_ns': str(completed),
            'terminal': terminal}


class DispatchGateTest(unittest.TestCase):
    def test_all_plan_dispatch_is_distinct_from_backpressure_and_missing_records(self):
        driver = {"ledger": {"planned": 100,
                            "scheduled_to_generated": {"count": 100, "p99_ms": 1},
                            "scheduled_to_sent": {"count": 90, "p99_ms": 40}}}
        cost = {"available": True, "usage_usec": {"seconds": .2}}
        self.assertTrue(generation_gate(driver, cost, 0, 1_000_000_000)["passed"])
        driver["ledger"]["scheduled_to_generated"]["count"] = 90
        self.assertFalse(generation_gate(driver, cost, 0, 1_000_000_000)["passed"])
        del driver["ledger"]["scheduled_to_generated"]
        legacy = generation_gate(driver, cost, 0, 1_000_000_000)
        self.assertFalse(legacy["passed"])
        self.assertIn("legacy", legacy["basis"])


class AssessmentQualityTest(unittest.TestCase):
    policy = {'arrival_limits_ms': {'p99_ms': 2, 'p999_ms': 10, 'max_ms': 50}}
    environment = {'quality': 'pass', 'evidence': 'control-three-windows.json'}

    def report(self):
        return {'valid_driver': True, 'configuration': clock(), 'ledger': {
            'planned': 1000, 'attempted': 1000, 'planned_reconciled': True,
            'all_scheduled_to_terminal': True, 'failure_fraction_planned': 0,
            'results': {'correct': 1000}, 'http_500_total': 0,
            'scheduled_to_attempt': {'count': 1000, 'p99_ms': 1, 'p999_ms': 4, 'max_ms': 8},
            'correct_scheduled_to_complete': {'count': 1000, 'p99_ms': 1}},
            'service_cost': {'before': {'memory_events': {}}, 'after': {'memory_events': {}}}}

    def test_accounting_is_not_arrival_quality_and_lateness_is_not_its_own_cause(self):
        r = self.report()
        r['ledger']['scheduled_to_attempt']['p99_ms'] = 338
        r['assessment'] = a = result_assessment(r, self.policy, self.environment)
        self.assertTrue(r['valid_driver'])
        self.assertTrue(a['functional_valid'])
        self.assertTrue(a['accounting_valid'])
        self.assertEqual(a['arrival_schedule_quality'], 'fail')
        self.assertFalse(a['measurement_invalid'])
        self.assertFalse(a['candidate_accepted'])
        self.assertFalse(qualification(r, 'json_object')['eligible_for_formal_candidate'])

    def test_http500_survives_simultaneous_driver_and_observer_failure(self):
        r = self.report()
        r['valid_driver'] = False
        r['ledger']['results'] = {'correct': 999, 'server_error': 1}
        r['ledger']['http_500_total'] = 1
        r['service_cost']['sampler_error'] = 'independently recorded observer exception'
        a = result_assessment(r, self.policy, self.environment)
        self.assertFalse(a['functional_valid'])
        self.assertFalse(a['accounting_valid'])
        self.assertTrue(a['measurement_invalid'])
        self.assertEqual(a['failed_request_outcomes'], {'server_error': 1})
        gate = qualification(r, 'json_object')
        self.assertEqual(upper_point(500, r, gate, 'kept/client.json')['gate_kind'],
                         'service_or_slo_failure')
        self.assertEqual(overload_boundary(r, 'json_object')['status'],
                         'service_fault_without_capacity_attribution')

    def test_missing_histogram_or_qualification_evidence_is_unknown(self):
        r = self.report()
        del r['ledger']['scheduled_to_attempt']
        a = result_assessment(r, self.policy, {'quality': 'pass'})
        self.assertEqual(a['arrival_schedule_quality'], 'unknown')
        self.assertEqual(a['environment_quality'], 'unknown')
        self.assertFalse(a['measurement_invalid'])
        self.assertEqual(result_assessment(self.report())['arrival_schedule_quality'], 'unknown')

    def test_failed_request_is_not_removed_when_its_delay_source_is_unknown(self):
        r = self.report()
        r['ledger']['results'] = {'correct': 999, 'timeout': 1}
        r['ledger']['scheduled_to_attempt']['max_ms'] = 500
        a = result_assessment(r, self.policy, self.environment)
        self.assertFalse(a['functional_valid'])
        self.assertEqual(a['failed_request_outcomes'], {'timeout': 1})
        self.assertFalse(a['measurement_invalid'])

    def test_expected_rejection_is_separate_from_correct_goodput(self):
        r = self.report()
        r['ledger']['results'] = {'correct': 990, 'rejected_503': 10}
        a = result_assessment(r, self.policy, self.environment)
        self.assertTrue(a['functional_valid'])
        self.assertFalse(a['candidate_accepted'])

    def test_oom_and_unfrozen_policy_are_never_accepted(self):
        r = self.report()
        r['service_cost']['after']['memory_events']['oom_kill'] = 1
        a = result_assessment(r, self.policy, self.environment)
        self.assertFalse(a['functional_valid'])
        self.assertEqual(a['environment_quality'], 'fail')
        self.assertFalse(result_assessment(self.report())['policy_registered'])

    def test_thresholds_are_validated_not_silently_widened(self):
        for value in (-1, float('nan'), float('inf'), True):
            policy = copy.deepcopy(self.policy)
            policy['arrival_limits_ms']['max_ms'] = value
            with self.subTest(value=value), self.assertRaises(ValueError):
                result_assessment(self.report(), policy, self.environment)


class QuietMetricsTest(unittest.TestCase):
    def quiet_no_database(self):
        return {'loops': [dict.fromkeys(('queued', 'running', 'resident', 'pending', 'connections'), 0)],
                'tasks': [dict.fromkeys(('ready', 'waiting', 'running', 'resident', 'timers'), 0)],
                'task_blocking': dict.fromkeys(('resident', 'queued', 'running', 'net_wait', 'net_step',
                                               'cancelled_still_running', 'output_charge_bytes'), 0),
                'buffer_bytes': 0}

    def test_database_metrics_optional_only_when_absent(self):
        value = self.quiet_no_database()
        self.assertFalse(quiet_metrics('ku', value))
        self.assertTrue(quiet_metrics('ku', value, require_database_metrics=False))
        value['database'] = {'pending_operations': 0, 'pending_bytes': 0}
        self.assertTrue(quiet_metrics('ku', value))
        self.assertTrue(quiet_metrics('ku', value, require_database_metrics=False))

    def test_present_database_metrics_are_never_ignored(self):
        for database in ({'pending_operations': 1, 'pending_bytes': 0},
                         {'pending_operations': 0, 'pending_bytes': 1}, {}, None):
            with self.subTest(database=database):
                value = self.quiet_no_database()
                value['database'] = database
                self.assertFalse(quiet_metrics('ku', value, require_database_metrics=False))

    def test_missing_or_empty_task_owner_lists_are_not_quiet(self):
        for field in ('loops', 'tasks'):
            for missing, rows in ((True, None), (False, None), (False, [])):
                with self.subTest(field=field, missing=missing, rows=rows):
                    value = self.quiet_no_database()
                    if missing:
                        value.pop(field)
                    else:
                        value[field] = rows
                    self.assertFalse(quiet_metrics('ku', value, require_database_metrics=False))

    def test_present_task_driver_requires_zero_resident(self):
        value = self.quiet_no_database()
        value['task_driver'] = {'resident': 0}
        self.assertTrue(quiet_metrics('ku', value, require_database_metrics=False))
        for task_driver in ({'resident': 1}, {'resident': None}, {}, None):
            with self.subTest(task_driver=task_driver):
                value['task_driver'] = task_driver
                self.assertFalse(quiet_metrics('ku', value, require_database_metrics=False))


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

    def preparation_case(self, folder, stage, bad=True, forced=False):
        case=Case.__new__(Case)
        case.path=Path(folder)/'case'; case.path.mkdir()
        case.workload='ok'; case.cores=4; case.memory=GIB
        case.args=types.SimpleNamespace(warmup=30,prime=3,keepalive_idle_ms=500)
        case.state={'history':[]}; case.save=mock.Mock(); case.check_memory=mock.Mock(); case.idle=mock.Mock()
        count=3000 if stage=='warmup' else 300
        report=self.report(count); report['ledger'].update(planned=count,http_500_total=0)
        report['ledger']['correct_scheduled_to_complete'].update(p99_ms=2928.64 if bad else 5,max_ms=2996.18 if bad else 6)
        ending={'exit_code':0,'forced':forced,'cgroup_removed':True,
                'after':{'process_count':0,'threads':0,'fd':0,'cpu_stat':{'usage_usec':300}}}
        service=types.SimpleNamespace(stopped=False,domain=types.SimpleNamespace(frozen=False),metrics=mock.Mock(return_value={}))
        def stop():
            service.stopped=True
            return ending
        service.stop=mock.Mock(side_effect=stop)
        def client(service,workload,rate,duration,output,**kwargs):
            result=copy.deepcopy(report); count=int(rate*duration)
            result['configuration'].update(workload=workload,rate=rate,duration_seconds=duration)
            result['ledger'].update(planned=count,correct_total=count,results={'correct':count})
            result['ledger']['correct_scheduled_to_complete']['count']=count
            write_json(Path(output)/'client.json',result)
            return result
        case.h=types.SimpleNamespace(start=mock.Mock(return_value=service),client=mock.Mock(side_effect=client))
        case.instances={} if stage=='warmup' else {'ku':service}
        return case,service,report

    def test_unqualified_prime_stops_before_target_window(self):
        with tempfile.TemporaryDirectory() as folder:
            case,service,original=self.preparation_case(folder,'prime')
            case.activate=mock.Mock(return_value=service)
            with self.assertRaisesRegex(RuntimeError,'preparation') as raised:
                case.observe('ku',512,5,'explore-05')
            self.assertEqual(type(raised.exception).__name__,'PreparationWindowFailure')
            self.assertEqual(case.h.client.call_count,1)
            self.assertEqual(case.h.client.call_args.args[2:4],(100,3))
            self.assertFalse((case.path/'explore-05/ku/client.json').exists())
            service.stop.assert_called_once(); case.idle.assert_not_called()
            receipt=read_json(case.path/'preparation-failure.json')
            self.assertEqual(receipt['stage'],'prime')
            self.assertEqual(receipt['preparation_gate_revision'],1)
            self.assertFalse(receipt['target_window_started'])
            self.assertTrue(receipt['cleanup']['natural_exit_verified'])
            preserved=read_json(case.path/'explore-05/ku/prime/client.json')
            self.assertEqual(preserved['ledger'],original['ledger'])
            self.assertFalse(preserved['preparation_gate']['qualification']['eligible_for_formal_candidate'])

    def test_unqualified_warmup_does_not_activate_candidate(self):
        with tempfile.TemporaryDirectory() as folder:
            case,service,original=self.preparation_case(folder,'warmup')
            with self.assertRaisesRegex(RuntimeError,'preparation'):
                case.activate('ku')
            self.assertEqual(case.h.client.call_count,1)
            self.assertEqual(case.h.client.call_args.args[2:4],(100,30))
            service.stop.assert_called_once()
            receipt=read_json(case.path/'preparation-failure.json')
            self.assertEqual(receipt['stage'],'warmup')
            self.assertEqual(receipt['cleanup']['termination']['exit_code'],0)
            self.assertEqual(read_json(case.path/'warmups/ku-001/client.json')['ledger'],original['ledger'])

    def test_target_cache_without_preparation_policy_is_not_reused(self):
        with tempfile.TemporaryDirectory() as folder:
            case,service,_=self.preparation_case(folder,'prime',bad=False)
            case.activate=mock.Mock(side_effect=AssertionError('old result must be rejected before any new request'))
            report=self.report(300)
            report.update(configuration={'workload':'ok','rate':512,'duration_seconds':5},measurement_identity={'harness_revision':5})
            write_json(case.path/'formal-common-00/ku/client.json',report)
            with self.assertRaisesRegex(RuntimeError,'preparation'):
                case.observe('ku',512,5,'formal-common-00')
            case.h.client.assert_not_called()

    def test_preparation_normal_path_and_cleanup_failure_remain_distinct(self):
        with tempfile.TemporaryDirectory() as folder:
            case,service,_=self.preparation_case(folder,'warmup',bad=False)
            self.assertIs(case.activate('ku'),service)
            service.stop.assert_not_called()
            self.assertEqual(read_json(case.path/'warmups/ku-001/client.json')['preparation_gate']['status'],'passed')
            case.h.client.reset_mock(); case.activate=mock.Mock(return_value=service)
            case.observe('ku',512,5,'explore-05')
            self.assertEqual([call.args[2:4] for call in case.h.client.call_args_list],[(100,3),(512,5)])
            case.idle.assert_called_once(); service.stop.assert_not_called()
        for scenario in ('forced_stop','client_error_without_report'):
            with self.subTest(scenario=scenario), tempfile.TemporaryDirectory() as folder:
                case,service,_=self.preparation_case(folder,'prime',forced=scenario=='forced_stop')
                case.activate=mock.Mock(return_value=service)
                if scenario=='client_error_without_report':
                    case.h.client.side_effect=RuntimeError('driver failed before a report')
                with self.assertRaisesRegex(RuntimeError,'preparation'):
                    case.observe('ku',512,5,'explore-05')
                service.stop.assert_called_once(); case.idle.assert_not_called()
                receipt=read_json(case.path/'preparation-failure.json')
                self.assertEqual(receipt['status'],'failed')
                self.assertIn('Case.prepare owns',receipt['stop_responsibility'])
                if scenario=='forced_stop':
                    self.assertFalse(receipt['cleanup']['natural_exit_verified'])
                else:
                    self.assertTrue(receipt['cleanup']['natural_exit_verified'])
                    self.assertFalse((case.path/'explore-05/ku/prime/client.json').exists())
                self.assertFalse((case.path/'explore-05/ku/client.json').exists())

    def test_client_cache_and_round_aggregation_include_preparation_policy(self):
        with tempfile.TemporaryDirectory() as folder:
            harness=Harness.__new__(Harness); harness.identity={'protocol_sha256':'protocol','manifest_sha256':'manifest'}
            service=types.SimpleNamespace(startup={'identity':{'stack':'ku'}})
            identity={'service':service.startup['identity'],'workload':'ok','rate':100,'duration':3,
                      'concurrency':256,'timeout':3,'events':True,'protocol_sha256':'protocol',
                      'driver_manifest_sha256':'manifest','harness_revision':5,'keepalive_idle_ms':500}
            output=Path(folder)/'cached'
            write_json(output/'client.json',{'service_cost':{},'measurement_identity':identity})
            with self.assertRaisesRegex(RuntimeError,'identity differs'):
                harness.client(service,'ok',100,3,output)
            identity['preparation_gate_revision']=1
            write_json(output/'client.json',{'service_cost':{},'measurement_identity':identity})
            self.assertEqual(harness.client(service,'ok',100,3,output)['measurement_identity'],identity)
            reports=self.aggregate(folder,150)
            for report in reports: report['measurement_identity']={'preparation_gate_revision':1}
            self.assertTrue(aggregate_rounds(reports,'ok')['qualified'])
            reports[-1]['measurement_identity']['preparation_gate_revision']=0
            with self.assertRaisesRegex(ValueError,'different driver'):
                aggregate_rounds(reports,'ok')

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
        self.assertEqual(list(cases(args)).count((2,GIB,'json_object')),1)
        self.assertEqual(declared_case_stacks(('ku','go','rust','node','java','dotnet','cpp'),2,GIB,'json_object'),('ku','go','rust','node'))
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


class DriverLifecycleCPUTest(unittest.TestCase):
    def test_export_tail_keeps_original_arrival_duration_denominator(self):
        before = {'monotonic_ns': 1000000000, 'cpu_stat': {'usage_usec': 0, 'user_usec': 0, 'system_usec': 0}}
        after = {'monotonic_ns': 21000000000, 'cpu_stat': {'usage_usec': 28000000, 'user_usec': 20000000, 'system_usec': 8000000}}
        result = driver_lifecycle_cpu(before, after, 10)
        self.assertTrue(result['available'])
        self.assertAlmostEqual(result['fraction_of_3CPU_original_arrival_duration'], 28/30)
        self.assertFalse(result['passed'])

    def test_startup_and_export_cost_remain_in_cgroup_delta(self):
        before = {'monotonic_ns': 1000000000, 'cpu_stat': {'usage_usec': 10000, 'user_usec': 8000, 'system_usec': 2000}}
        after = {'monotonic_ns': 13000000000, 'cpu_stat': {'usage_usec': 6010000, 'user_usec': 4008000, 'system_usec': 2002000}}
        result = driver_lifecycle_cpu(before, after, 10)
        self.assertTrue(result['passed'])
        self.assertEqual(result['CPU_seconds'], {'usage_usec': 6, 'user_usec': 4, 'system_usec': 2})
        self.assertEqual(result['fraction_of_3CPU_original_arrival_duration'], .2)

    def test_missing_reversed_and_zero_duration_cannot_pass(self):
        before = {'monotonic_ns': 100, 'cpu_stat': {'usage_usec': 1, 'user_usec': 1, 'system_usec': 0}}
        good = {'monotonic_ns': 200, 'cpu_stat': {'usage_usec': 2, 'user_usec': 1, 'system_usec': 1}}
        for first, last, duration in [(before, {}, 10), (good, before, 10), (before, good, 0)]:
            with self.subTest(first=first, last=last, duration=duration):
                result = driver_lifecycle_cpu(first, last, duration)
                self.assertFalse(result['available'])
                self.assertFalse(result['passed'])


if __name__ == '__main__':
    unittest.main()
