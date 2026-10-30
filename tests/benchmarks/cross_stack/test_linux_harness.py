"""Clock and recovery counterexamples for the permanent comparison runner."""
import unittest
import copy
import json
import signal
import csv
import gzip
import tempfile
import types
import os
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
import linux_harness
import focus_cycle


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
            'all_scheduled_to_terminal': {'count': 1000, 'p99_ms': 1}, 'failure_fraction_planned': 0,
            'results': {'correct': 1000}, 'http_500_total': 0,
            'scheduled_to_attempt': {'count': 1000, 'p99_ms': 1, 'p999_ms': 4, 'max_ms': 8},
            'correct_scheduled_to_complete': {'count': 1000, 'p99_ms': 1}},
            'service_cost': {'before': {'memory_events': {}}, 'after': {'memory_events': {}}}}

    def test_constant_diagnostic_never_qualifies_as_dynamic_business_work(self):
        for flags in ({'diagnostic_only': True}, {'normal_score_eligible': False}):
            result = dict(self.report(), **flags)
            self.assertEqual(qualification(result, 'ok')['reasons'],
                             ['diagnostic_contract_excluded_from_main_table'])
            self.assertFalse(qualification(result, 'ok')['eligible_for_formal_candidate'])
        self.assertTrue(qualification(self.report(), 'ok')['eligible_for_formal_candidate'])

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
        gate = qualification(r, 'json_object')
        self.assertFalse(gate['eligible_for_formal_candidate'])
        self.assertEqual(upper_point(500, r, gate, 'kept/client.json')['gate_kind'],
                         'quality_unknown_censored')
        self.assertEqual(overload_boundary(r, 'json_object')['status'], 'quality_unknown_censored')

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
        r['assessment'] = a
        self.assertEqual(overload_boundary(r, 'json_object')['status'],
                         'request_failure_without_capacity_attribution')

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


class FocusCycleTests(unittest.TestCase):
    """All executions below use synthetic windows; no server/cgroup is started."""

    def plan(self):
        return {'versions': {n:{'root':'unused','binary_sha256':'a'*64} for n in ('A','B','R')},
                'resources': {'driver_cores':1,'driver_cpuset':'2','service_cores':2,'service_memory_bytes':1<<30},
                'quality_policy':{},'rates':{'ok':100},'control_cpu_max_min':1.10,
                'control':{'warmup_seconds':0,'score_seconds':1},
                'screen':{'warmup_seconds':0,'score_seconds':1},
                'final_orders':['ABR','BRA','RAB','ARB','BAR'],
                'bootstrap_resamples':100,'resume':{'maximum_replacements':1}}

    @staticmethod
    def binding(plan, name):
        return {'source_commit':'a'*40,'build_mode':{'native_flags':'O2'},
                'files':{'ELF':{'sha256':name*64}}}

    def fake_window(self, costs=None, invalid=False, failed=False, calls=None):
        values = iter(costs) if costs else None
        def run(plan,name,workload,rate,warmup,duration,output,**kwargs):
            if calls is not None:
                calls.append((name,kwargs['job']['phase']))
            return {'name':name,'workload':workload,'rate':rate,'status':'functional_failure' if failed else 'observed',
                    'functional_valid':not failed,'accounting_valid':True,'arrival_schedule_quality':'pass',
                    'environment_quality':'unknown' if invalid else 'pass','measurement_invalid':invalid,
                    'independent_collector_fault':invalid,'goal_status':'not_met' if failed else 'unknown',
                    'candidate_accepted':False,'cpu_us':next(values) if values else 1,
                    'qualification':{'qualified':not invalid and not failed},'natural_reclamation':True,
                    'formal_duration':kwargs['job']['phase']=='final','comparison_contract':{'same':'contract'}}
        return run

    def test_old_flags_and_final_duration_remain_explicit(self):
        p=self.plan()
        self.assertEqual(len(focus_cycle.jobs_for(p,'control','ok')),3)
        self.assertEqual(len(focus_cycle.jobs_for(p,'screen','ok')),3)
        final=focus_cycle.jobs_for(p,'final','ok')
        self.assertEqual(len(final),15)
        self.assertTrue(all(r['warmup']==10 and r['duration']==30 for r in final))
        p['final_orders']=['BR','RB','BR','RB','BR']
        self.assertEqual(len(focus_cycle.jobs_for(p,'final','ok',('B','R'))),10)
        p['schedule']=[{'phase':'final','workload':'ok','names':['A'],'warmup_seconds':0,'score_seconds':3}]
        with self.assertRaisesRegex(ValueError,'final requires'):
            focus_cycle.jobs_for(p,'final')

    def test_old_control_spread_does_not_block_planned_explore(self):
        p=self.plan();p['schedule']=[{'phase':'control','workload':'ok','orders':[['A']]*3,
            'warmup_seconds':0,'score_seconds':1},{'phase':'explore','workload':'ok','orders':[['B','R']],
            'warmup_seconds':0,'score_seconds':3}]
        with tempfile.TemporaryDirectory() as d:
            path=Path(d)/'plan.json';path.write_text(json.dumps(p));calls=[]
            with mock.patch.object(focus_cycle,'version_identity',side_effect=self.binding), \
                 mock.patch.object(focus_cycle,'window',side_effect=self.fake_window([1,1.05,1.105,1,1],calls=calls)):
                result=focus_cycle.execute(path,Path(d)/'out','all','ok')
            self.assertFalse(result['control_qualified'])
            self.assertTrue(result['exploration_allowed'])
            self.assertEqual(calls[-2:],[('B','explore'),('R','explore')])
            self.assertFalse(result['formal_qualified'])

    def test_unusable_score_stop_is_partial_and_resume_cannot_replace_it(self):
        p=self.plan();p['stop_after_consecutive_unusable_windows']=2
        p['schedule']=[{'phase':'final','workload':'ok','orders':[['B','R']]*5,
                       'warmup_seconds':10,'score_seconds':30}]
        p['capacity']={'must_not_run':True}
        calls=[];fake=self.fake_window(invalid=True,calls=calls)
        def scored(*args,**kwargs):
            return dict(fake(*args,**kwargs),score_started=True)
        with tempfile.TemporaryDirectory() as d:
            path=Path(d)/'plan.json';path.write_text(json.dumps(p));out=Path(d)/'out'
            with mock.patch.object(focus_cycle,'version_identity',side_effect=self.binding), \
                 mock.patch.object(focus_cycle,'window',side_effect=scored), \
                 mock.patch.object(focus_cycle,'capacity_jobs') as capacity:
                first=focus_cycle.execute(path,out,'all','ok')
                resumed=focus_cycle.execute(path,out,'all','ok',resume=True)
            self.assertEqual(len(calls),2);capacity.assert_not_called()
            for result in (first,resumed):
                self.assertEqual(len(result['rows']),2)
                self.assertEqual(len(result['unexecuted_jobs']),8)
                self.assertEqual(result['goal_status'],'blocked')
                self.assertTrue(result['capacity_deferred'])
                self.assertTrue(all(not result[key] for key in
                    ('dispatch_complete','execution_complete','formal_design_complete','formal_qualified')))
                self.assertEqual([c['cause'] for c in result['unusable_window_policy']['stop']['causes']],
                                 ['confirmed_fault','confirmed_fault'])
            state=read_json(out/'state.json')
            self.assertEqual([len(v) for v in state['windows'].values()],[1,1])

    def test_arrival_failure_stops_without_relabeling_unknown_cause_or_unscored_rows(self):
        p=self.plan();p['stop_after_consecutive_unusable_windows']=2
        p['schedule']=[{'phase':'explore','workload':'ok','orders':[['B'],['R'],['B'],['R'],['A']],
                       'warmup_seconds':0,'score_seconds':3}]
        sequence=iter([{'score_started':False,'measurement_invalid':True},
            {'score_started':True,'arrival_schedule_quality':'unknown'},
            {'score_started':True,'arrival_schedule_quality':'fail'},
            {'score_started':True,'arrival_schedule_quality':'fail'}])
        calls=[];fake=self.fake_window(calls=calls)
        def scored(*args,**kwargs):
            return dict(fake(*args,**kwargs),environment_quality='unknown',**next(sequence))
        with tempfile.TemporaryDirectory() as d:
            path=Path(d)/'plan.json';path.write_text(json.dumps(p))
            with mock.patch.object(focus_cycle,'version_identity',side_effect=self.binding), \
                 mock.patch.object(focus_cycle,'window',side_effect=scored):
                result=focus_cycle.execute(path,Path(d)/'out','all','ok')
        self.assertEqual(len(calls),4)
        self.assertEqual(len(result['unexecuted_jobs']),1)
        self.assertEqual([c['cause'] for c in result['unusable_window_policy']['stop']['causes']],
                         ['arrival_failed_unknown_cause','arrival_failed_unknown_cause'])
        self.assertTrue(all(row['measurement_invalid'] is False and row['environment_quality']=='unknown'
                            for row in result['rows'][2:]))

    def test_effective_score_resets_and_product_failures_are_not_environment_stops(self):
        p=self.plan();p['stop_after_consecutive_unusable_windows']=2
        p['schedule']=[{'phase':'explore','workload':'ok','orders':[['B'],['R'],['A'],['B'],['R']],
                       'warmup_seconds':0,'score_seconds':3}]
        sequence=iter([{'measurement_invalid':True},
            {'measurement_invalid':False,'environment_quality':'unknown'},
            {'measurement_invalid':True,'functional_valid':False,'status':'functional_failure'},
            {'measurement_invalid':True,'accounting_valid':False,'status':'accounting_failure'},
            {'measurement_invalid':True}])
        calls=[];fake=self.fake_window(calls=calls)
        def scored(*args,**kwargs):
            return dict(fake(*args,**kwargs),score_started=True,**next(sequence))
        with tempfile.TemporaryDirectory() as d:
            path=Path(d)/'plan.json';path.write_text(json.dumps(p))
            with mock.patch.object(focus_cycle,'version_identity',side_effect=self.binding), \
                 mock.patch.object(focus_cycle,'window',side_effect=scored):
                result=focus_cycle.execute(path,Path(d)/'out','all','ok')
            for limit in (0,11,True):
                p['stop_after_consecutive_unusable_windows']=limit;path.write_text(json.dumps(p))
                with self.assertRaises(ValueError):
                    focus_cycle.execute(path,Path(d)/('invalid-'+str(limit)),'all','ok')
        self.assertEqual(len(calls),5)
        self.assertIsNone(result['unusable_window_policy']['stop'])
        self.assertEqual(result['unusable_window_policy']['consecutive'],1)
        self.assertEqual(result['goal_status'],'not_met')

    def test_resume_keeps_bad_scores_and_allows_only_retained_independent_fault(self):
        p=self.plan();p['schedule']=[{'phase':'explore','workload':'ok','names':['B'],'warmup_seconds':0,'score_seconds':3}]
        for invalid,failed,expected in ((False,False,1),(False,True,1),(True,True,1),(True,False,2)):
            with self.subTest(invalid=invalid,failed=failed), tempfile.TemporaryDirectory() as d:
                path=Path(d)/'plan.json';path.write_text(json.dumps(p));out=Path(d)/'out';calls=[]
                with mock.patch.object(focus_cycle,'version_identity',side_effect=self.binding), \
                     mock.patch.object(focus_cycle,'window',side_effect=self.fake_window(invalid=invalid,failed=failed,calls=calls)):
                    first=focus_cycle.execute(path,out,'explore','ok')
                    state=json.loads((out/'state.json').read_text());key=first['rows'][0]['id']
                    old=Path(state['windows'][key][0]['result_path']);before=old.read_bytes()
                    focus_cycle.execute(path,out,'explore','ok',resume=True)
                state=json.loads((out/'state.json').read_text())
                self.assertEqual(len(calls),expected)
                self.assertEqual(len(state['windows'][key]),expected)
                self.assertEqual(old.read_bytes(),before)
                self.assertFalse(first['candidate_accepted'])

    def test_unknown_running_window_is_blocked_and_changed_plan_is_preserved(self):
        p=self.plan();p['schedule']=[{'phase':'explore','workload':'ok','names':['B'],'warmup_seconds':0,'score_seconds':3}]
        with tempfile.TemporaryDirectory() as d:
            path=Path(d)/'plan.json';path.write_text(json.dumps(p));out=Path(d)/'out';calls=[]
            with mock.patch.object(focus_cycle,'version_identity',side_effect=self.binding), \
                 mock.patch.object(focus_cycle,'window',side_effect=self.fake_window(calls=calls)):
                focus_cycle.execute(path,out,'explore','ok')
                state=json.loads((out/'state.json').read_text());key=next(iter(state['windows']))
                state['windows'][key][-1]['state']='running';write_json(out/'state.json',state)
                result=focus_cycle.execute(path,out,'explore','ok',resume=True)
                self.assertEqual(result['rows'][0]['status'],'blocked_resume')
                self.assertEqual(len(calls),1)
                p['rates']['ok']=101;path.write_text(json.dumps(p))
                with self.assertRaisesRegex(ValueError,'changed identity'):
                    focus_cycle.execute(path,out,'explore','ok',resume=True)
            self.assertEqual(len(list(out.glob('invalidation-*.json'))),1)
            self.assertTrue((out/key/'attempt-00/result.json').is_file())

    def test_identity_checks_cli_c_source_elf_driver_and_dependencies(self):
        with tempfile.TemporaryDirectory() as d:
            root=Path(d);files={}
            for name in ('cli','server.ku','server.c','server','libpq','driver'):
                files[name]=root/name;files[name].write_bytes(name.encode())
            manifest={'candidate_base':'a'*40,'client':{'command':[str(files['driver'])],'sha256':focus_cycle.digest(files['driver'])},
                'stacks':{'ku':{'command':[str(files['server'])],'build':{
                    'cli':str(files['cli']),'cli_sha256':focus_cycle.digest(files['cli']),
                    'libpq_source':str(files['libpq']),'libpq_sha256':focus_cycle.digest(files['libpq']),
                    'native':{'server':{'source':str(files['server.ku']),'source_sha256':focus_cycle.digest(files['server.ku']),
                        'generated_c':str(files['server.c']),'generated_c_sha256':focus_cycle.digest(files['server.c'])}}}}}}
            write_json(root/'manifest.json',manifest)
            p={'versions':{'A':{'root':str(root),'binary_sha256':focus_cycle.digest(files['server'])}}}
            self.assertEqual(len(focus_cycle.version_identity(p,'A')['files']),6)
            for name,path in files.items():
                original=path.read_bytes();path.write_bytes(original+b'changed')
                with self.subTest(name=name), self.assertRaisesRegex(ValueError,'identity changed'):
                    focus_cycle.version_identity(p,'A')
                path.write_bytes(original)

    def test_replacement_needs_independent_receipt_and_never_erases500_or_bad_shutdown(self):
        with tempfile.TemporaryDirectory() as d:
            evidence=Path(d)/'collector-error.log';evidence.write_text('independent collector exception')
            attempt={'row':{'functional_valid':True},'result_sha256':'a'*64}
            receipt={'measurement_fault_confirmed':True,'result_sha256':'a'*64,'cause':'collector_failure',
                     'evidence_files':[{'path':str(evidence),'sha256':focus_cycle.digest(evidence)}]}
            self.assertTrue(focus_cycle.replacement_allowed(attempt,receipt,2,0))
            receipt['cause']='arrival_lateness'
            self.assertFalse(focus_cycle.replacement_allowed(attempt,receipt,2,0))
            receipt['cause']='collector_failure';attempt['row']['functional_valid']=False
            self.assertFalse(focus_cycle.replacement_allowed(attempt,receipt,2,0))
            attempt['row'].update(functional_valid=True,termination={'forced':True})
            self.assertFalse(focus_cycle.replacement_allowed(attempt,receipt,2,0))
            attempt['row']={'functional_valid':True}
            self.assertFalse(focus_cycle.replacement_allowed(attempt,receipt,2,2))

    def test_capacity_stops_at_finite_bracket_and_never_calls503_success(self):
        p=self.plan();p['capacity']={'final_frozen':True,'workloads':['ok'],'names':['B'],
            'maximum_points':4,'refinements':1,'maximum_rate':1000,'factor':2}
        for fault in (False,True):
            calls=[]
            def point(plan,job,*args,**kwargs):
                rate=job['rate'];calls.append(rate);good=rate<=200
                return dict(job,status='observed',functional_valid=not fault or good,accounting_valid=True,
                    qualification={'qualified':good},recovery={'passed':True},
                    results={'correct':100 if good else 90,'rejected_503':0 if good else 10},
                    upper_point={'rate':rate,'censored':False},overload_boundary={'boundary_observed':not good and not fault})
            with tempfile.TemporaryDirectory() as d, mock.patch.object(focus_cycle,'run_one',side_effect=point):
                rows=focus_cycle.capacity_jobs(p,Path(d),{'windows':{}},{})
            self.assertEqual(rows[0]['qualified_lower_rate'],200)
            self.assertLessEqual(len(calls),4)
            self.assertEqual(calls,[100,200,400] if fault else [100,200,400,300])
            self.assertEqual(rows[0]['boundary_observed'],not fault)
            if fault:
                self.assertNotEqual(rows[0]['claim'],'observed bracket')

    def test_goals_resample_whole_matching_groups_and_keep_adoption_separate(self):
        p=self.plan();p['targets']=[dict(phase='final',workload='ok',contract='dom',numerator='B',denominator='R',minimum_pairs=5)]
        rows=[]
        for group in range(5):
            for name,cpu in (('B',.8),('R',1)):
                rows.append(dict(name=name,entry=0,group=group,rate=100,phase='final',workload='ok',contract='dom',
                    cpu_us=cpu,qualification={'qualified':True},formal_duration=True,natural_reclamation=True,
                    functional_valid=True,accounting_valid=True,comparison_contract={'same':'input'}))
        value=focus_cycle.compare_goals(p,rows)[0]
        self.assertEqual(value['paired_windows'],5)
        self.assertEqual(value['interval']['unit'],'paired interleaved window')
        self.assertEqual(value['goal_status'],'met');self.assertFalse(value['candidate_accepted'])
        rows[0]['comparison_contract']={'different':'input'}
        self.assertEqual(focus_cycle.compare_goals(p,rows)[0]['goal_status'],'unknown')
        with self.assertRaisesRegex(ValueError,'duplicate window'):
            focus_cycle.compare_goals(p,rows+[rows[0]])


class FocusStabilityTests(unittest.TestCase):
    """Only synthetic reports/threads; these never create cgroups or services."""

    def test_each_stability_consumer_gets_its_own_frozen_protocol_and_real_corpora(self):
        source=Path(__file__).with_name('protocol.json').resolve(); original=source.read_bytes()
        with tempfile.TemporaryDirectory() as directory:
            mixed=types.SimpleNamespace(protocol=source,output=Path(directory)/'mixed',
                identity={'protocol_sha256':focus_cycle.digest(source),'manifest_sha256':'f'*64})
            initial=copy.deepcopy(mixed.identity)
            receipt=focus_cycle.stability_client_protocol(mixed,'mixed')
            self.assertEqual(mixed.protocol,source);self.assertEqual(mixed.identity,initial)
            self.assertEqual(receipt['sha256'],focus_cycle.digest(source))
            task=types.SimpleNamespace(protocol=source,output=Path(directory)/'task-mix',identity=initial.copy())
            receipt=focus_cycle.stability_client_protocol(task,'task_mix')
            derived=json.loads(task.protocol.read_text());base=json.loads(original)
            self.assertEqual(derived['endpoints']['cpu']['work_units'],[512])
            self.assertEqual(derived['endpoints']['cpu']['seed_lengths'],[3,11])
            self.assertEqual(derived['SLO']['task_mix_short_p99_ms'],20)
            self.assertEqual(task.identity['protocol_sha256'],focus_cycle.digest(task.protocol))
            self.assertEqual(task.identity['manifest_sha256'],initial['manifest_sha256'])
            self.assertEqual(receipt['source_sha256'],focus_cycle.digest(source))
            for name,item in derived['corpora'].items():
                corpus=(task.protocol.parent/item['path']).resolve()
                self.assertEqual(corpus,(source.parent/base['corpora'][name]['path']).resolve())
                self.assertEqual(focus_cycle.digest(corpus),item['sha256'])
                self.assertEqual(json.loads(corpus.read_bytes())['counter'],item['counter'])
                self.assertEqual(receipt['corpora'][name]['source_path'],str(corpus))
                self.assertEqual(receipt['corpora'][name]['sha256'],item['sha256'])
            self.assertEqual(derived['endpoints']['mixed'],base['endpoints']['mixed'])
            self.assertEqual(source.read_bytes(),original)

    def test_task_mix_refuses_changed_corpus_or_route_before_writing_protocol(self):
        source=Path(__file__).with_name('protocol.json').resolve()
        for change in ('sha','envelope','route'):
            with self.subTest(change=change),tempfile.TemporaryDirectory() as directory:
                root=Path(directory);protocol=json.loads(source.read_text())
                for item in protocol['corpora'].values():item['path']=str((source.parent/item['path']).resolve())
                if change=='sha':protocol['corpora']['object']['sha256']='0'*64
                elif change=='route':protocol['endpoints']['cpu']['method']='POST'
                else:
                    corpus=root/'bad.json';corpus.write_text('{"counter":7,"wrong":null}')
                    protocol['corpora']['object'].update(path=str(corpus),sha256=focus_cycle.digest(corpus))
                original=root/'source.json';original.write_text(json.dumps(protocol))
                client=types.SimpleNamespace(protocol=original,output=root/'derived',identity={})
                with self.assertRaises(ValueError):focus_cycle.stability_client_protocol(client,'task_mix')
                self.assertFalse((client.output/'task-mix-protocol.json').exists())

    def plan(self):
        plan = FocusCycleTests().plan()
        plan['stability'] = {'name': 'B', 'frozen_final': True, 'seconds': 1800,
            'segment_seconds': 300, 'mixed_rate': 500, 'short_rate': 800, 'cpu_rate': 10,
            'short_slo_ms': 20, 'maximum_start_skew_seconds': 2, 'maximum_segment_gap_seconds': 10,
            'growth_limits': {'service_memory_bytes': 64<<20, 'coordinator_rss_bytes': 32<<20,
                              'service_fd': 32, 'service_threads': 8}}
        return plan

    def task_report(self):
        report = AssessmentQualityTest().report()
        report['configuration'].update(cpu_cancel_every=10, cpu_cancel_delay_us=250,
                                       task_mix_short_SLO_ms=20)
        report['ledger'].update(correct_total=990, failure_fraction_planned=.01,
                                results={'correct': 990, 'planned_reset': 10})
        report['by_type'] = {
            'ok': {'normal_planned': 900, 'normal_failure_fraction_planned': 0,
                   'correct_scheduled_to_complete': {'count': 900, 'p99_ms': 19}},
            'cpu': {'normal_planned': 90, 'normal_failure_fraction_planned': 0,
                    'correct_scheduled_to_complete': {'count': 90, 'p99_ms': 100},
                    'planned_cancellation': {'planned': 10, 'sent': 10, 'reset_issued': 10,
                                             'results': {'planned_reset': 10}}}}
        report['assessment'] = result_assessment(report, AssessmentQualityTest.policy,
                                                AssessmentQualityTest.environment)
        return report

    def test_continuous_duration_and_one_shared_parent_are_required(self):
        plan = self.plan()
        self.assertEqual(focus_cycle.stability_config(plan)['seconds'], 1800)
        for field, value in (('seconds', 1799), ('segment_seconds', 301),
                             ('short_slo_ms', 21), ('frozen_final', False),
                             ('maximum_start_skew_seconds', 6)):
            changed = copy.deepcopy(plan)
            changed['stability'][field] = value
            with self.assertRaises(ValueError):
                focus_cycle.stability_config(changed)
        for key, value in (('driver_cores', 2), ('driver_cpuset', '2-3'),
                           ('driver_memory_bytes', 2<<30)):
            changed = copy.deepcopy(plan)
            changed['resources'][key] = value
            with self.assertRaisesRegex(ValueError, 'shared driver parent'):
                focus_cycle.stability_config(changed)

    def test_overlapping_clients_never_double_count_service_cpu(self):
        before = {'cpu_stat': {'usage_usec': 1_000_000}}
        after = {'cpu_stat': {'usage_usec': 4_000_000}}
        reports = [{'ledger': {'correct_total': n}, 'service_cost': {
            'lifetime_cpu_seconds': 1000, 'cpu_seconds_per_correct': 1000}}
                   for n in (10, 20)]
        cost = focus_cycle.stability_service_cost(before, after, reports)
        self.assertEqual(cost['lifetime_cpu_seconds'], 3)
        self.assertEqual(cost['correct_total'], 30)
        self.assertAlmostEqual(cost['cpu_seconds_per_correct'], .1)
        with self.assertRaisesRegex(ValueError, 'counter reset'):
            focus_cycle.stability_service_cost(after, before, reports)

    def test_planned_resets_use_normal_denominator_but_remain_in_original_gate(self):
        report = self.task_report()
        stream = focus_cycle.stability_stream(report, 'task_mix')
        self.assertTrue(stream['qualified'])
        self.assertFalse(stream['original_qualification']['qualified'])
        self.assertEqual(stream['results']['planned_reset'], 10)
        report['by_type']['cpu']['planned_cancellation']['reset_issued'] = 9
        self.assertFalse(focus_cycle.stability_stream(report, 'task_mix')['qualified'])

    def test_errors_and_timeouts_survive_observer_invalidity_and_rst(self):
        for failure in ('timeout', 'connection_error', 'body_error', 'server_error'):
            report = self.task_report()
            report['ledger']['results'][failure] = 1
            report['service_cost']['sampler_error'] = 'recorded observer fault'
            report['assessment'] = result_assessment(report, AssessmentQualityTest.policy,
                                                    AssessmentQualityTest.environment)
            stream = focus_cycle.stability_stream(report, 'task_mix')
            self.assertFalse(stream['qualified'])
            self.assertFalse(stream['assessment']['functional_valid'])
            self.assertEqual(stream['assessment']['failed_request_outcomes'][failure], 1)
            self.assertEqual(stream['results'][failure], 1)

    def test_short_tail_does_not_hide_behind_cpu_or_invalid_histogram(self):
        for value in (None, 20.01, float('nan')):
            report = self.task_report()
            report['by_type']['ok']['correct_scheduled_to_complete']['p99_ms'] = value
            self.assertFalse(focus_cycle.stability_stream(report, 'task_mix')['qualified'])

    def test_cancel_kills_private_parent_before_join(self):
        trace = []
        class Thread:
            def join(self, timeout): trace.append('join')
            def is_alive(self): return False
        parent = types.SimpleNamespace(kill=lambda: trace.append('owned-parent-kill'))
        focus_cycle.stability_join([Thread(), Thread()], parent, cancel=True)
        self.assertEqual(trace, ['owned-parent-kill', 'join', 'join'])

    def test_child_failure_cancels_and_joins_both_owned_clients(self):
        trace = []
        parent = types.SimpleNamespace(kill=lambda: trace.append('owned-parent-kill'))
        class Client:
            def __init__(self, fail): self.fail = fail
            def launch(self): return None
            def client(self, *args, **kwargs):
                if self.fail: raise RuntimeError('synthetic driver failure')
                return {'raw': 'retained'}
        with tempfile.TemporaryDirectory() as directory:
            with self.assertRaisesRegex(RuntimeError, 'driver failure'):
                focus_cycle.stability_clients({'mixed': Client(True), 'task_mix': Client(False)},
                    object(), self.plan()['stability'], Path(directory), parent)
        self.assertIn('owned-parent-kill', trace)
        import threading
        self.assertFalse(any(thread.name.startswith('stability-') for thread in threading.enumerate()))

    def test_cancel_prevents_driver_launch_after_parent_kill(self):
        import threading
        killed, late_prepared = threading.Event(), threading.Event()
        trace = []
        parent = types.SimpleNamespace(kill=lambda: (trace.append('kill'), killed.set()))
        class Client:
            def __init__(self, fail): self.fail = fail
            def launch(self): trace.append('late-launch-must-not-happen')
            def client(self, *args, **kwargs):
                if self.fail:
                    self.prepared = late_prepared.wait(1)
                    raise RuntimeError('synthetic cancellation')
                late_prepared.set()
                if not killed.wait(1): raise RuntimeError('expected parent kill')
                self.launch()
        with tempfile.TemporaryDirectory() as directory:
            with self.assertRaisesRegex(RuntimeError, 'driver failure'):
                focus_cycle.stability_clients({'mixed': Client(True), 'task_mix': Client(False)},
                    object(), self.plan()['stability'], Path(directory), parent)
        self.assertIn('kill', trace)
        self.assertNotIn('late-launch-must-not-happen', trace)
        self.assertFalse(any(thread.name.startswith('stability-') for thread in threading.enumerate()))

    def test_growth_uses_observed_endpoints_and_frozen_thresholds(self):
        checkpoints = [{'service': {'memory_current': n, 'fd': 5, 'threads': 2},
                        'coordinator': {'rss_bytes': 10}} for n in (10, 11, 12, 50, 51, 52)]
        limits = dict(self.plan()['stability']['growth_limits'], service_memory_bytes=20)
        growth = focus_cycle.stability_growth(checkpoints, limits)
        self.assertFalse(growth['passed'])
        self.assertEqual(growth['metrics']['service_memory_bytes']['last3_minus_first3_median'], 40)
        self.assertFalse(focus_cycle.stability_growth(checkpoints[:5], limits)['passed'])

    def test_absent_optional_event_and_driver_metrics_never_become_zero_evidence(self):
        coverage = focus_cycle.stability_metrics_coverage({'loops': [{'queued': 0}]})
        self.assertEqual(coverage['groups']['task_driver'], 'not_exposed')
        self.assertEqual(coverage['completion_event_queue'], 'not_exposed')
        self.assertFalse(coverage['completion_event_queue_zero_proven'])
        coverage = focus_cycle.stability_metrics_coverage({'loops': [{'completion_events': 7}]})
        self.assertEqual(coverage['event_related_fields'], ['loops[*].completion_events'])
        self.assertFalse(coverage['completion_event_queue_zero_proven'])

    def test_stability_cannot_resume_or_overwrite_previous_attempt(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory)/'plan.json'
            path.write_text(json.dumps(self.plan()))
            with self.assertRaisesRegex(ValueError, 'once-only'):
                focus_cycle.execute_stability(path, Path(directory)/'unused', resume=True)
            output = Path(directory)/'output'
            output.mkdir()
            (output/'prior-failure.json').write_text('{}')
            with self.assertRaisesRegex(ValueError, 'empty output'):
                focus_cycle.execute_stability(path, output)
            self.assertTrue((output/'prior-failure.json').exists())


class FocusLifecycleRecoveryTests(unittest.TestCase):
    """Pure accounting counterexamples; no service/build or timed load."""

    def inputs(self):
        score = {'ledger': {'correct_total': 10, 'planned': 12, 'results': {'correct': 10, 'timeout': 2},
                            'correct_scheduled_to_complete': {'count': 10, 'p99_ms': 19}},
                 'qualification': {'qualified': False}, 'service_cost': {
                     'before': {'cpu_stat': {'usage_usec': 100}},
                     'after': {'cpu_stat': {'usage_usec': 200}}}}
        recovery = {'ledger': {'correct_total': 30, 'planned': 31,
                               'results': {'correct': 30, 'timeout': 1}}}
        physical = {'cpu_stat': {'usage_usec': 1000}, 'process_count': 1}
        termination = {'forced': False, 'cgroup_removed': True,
                       'after': {'cpu_stat': {'usage_usec': 1600}, 'process_count': 0}}
        return score, recovery, physical, termination

    def test_recovery_lifetime_cpu_uses_both_correct_denominators_without_mutation(self):
        score, recovery, physical, termination = self.inputs()
        original = copy.deepcopy((score, recovery))
        result = focus_cycle.window_lifecycle_cost(score, physical, True, termination, {}, recovery)
        self.assertAlmostEqual(result['cpu_seconds'], .0015)
        self.assertAlmostEqual(result['cpu_seconds_per_correct'], .0015/40)
        self.assertEqual(result['correct_response_denominator'], {'score': 10, 'recovery': 30, 'total': 40})
        self.assertAlmostEqual(result['post_driver_cpu_seconds'], .0014)
        self.assertIn('including recovery', result['post_driver_cpu_scope'])
        self.assertIn('warmup excluded', result['scope'])
        self.assertEqual((score, recovery), original)
        self.assertEqual(score['ledger']['correct_total'], 10)
        self.assertEqual(score['ledger']['correct_scheduled_to_complete']['p99_ms'], 19)
        self.assertFalse(score['qualification']['qualified'])

    def test_recovery_failures_keep_cpu_and_do_not_increase_correct_denominator(self):
        score, recovery, physical, termination = self.inputs()
        recovery['ledger'].update(correct_total=0, results={'timeout': 31})
        result = focus_cycle.window_lifecycle_cost(score, physical, False, termination, {}, recovery)
        self.assertAlmostEqual(result['cpu_seconds'], .0015)
        self.assertAlmostEqual(result['cpu_seconds_per_correct'], .0015/10)
        self.assertEqual(result['correct_response_denominator']['recovery'], 0)
        score['ledger']['correct_total'] = 0
        result = focus_cycle.window_lifecycle_cost(score, physical, False, termination, {}, recovery)
        self.assertIsNone(result['cpu_seconds_per_correct'])
        self.assertEqual(result['correct_response_denominator']['total'], 0)

    def test_without_recovery_entire_existing_receipt_is_unchanged(self):
        score, _, physical, termination = self.inputs()
        expected = full_lifecycle_cost(score, physical, True, termination, {})
        result = focus_cycle.window_lifecycle_cost(score, physical, True, termination, {})
        self.assertEqual(result, expected)
        self.assertNotIn('correct_response_denominator', result)


class DriverOwnedCleanupTests(unittest.TestCase):
    def test_finalizer_recovers_owned_domain_after_interrupted_stop_flag(self):
        with tempfile.TemporaryDirectory() as d:
            trace=[]
            domain=types.SimpleNamespace(path=Path(d),kill=lambda:trace.append('kill'),
                snapshot=lambda:{'process_count':0},remove=lambda:True)
            process=types.SimpleNamespace(wait=lambda timeout:trace.append('reap'))
            service=types.SimpleNamespace(stopped=True,domain=domain,process=process,stack='ku')
            h=Harness.__new__(Harness);h.services=[service]
            result=h.close()
            self.assertEqual(trace,['kill','reap'])
            self.assertTrue(result[0]['forced_owned_cleanup'])
            self.assertTrue(result[0]['cgroup_removed'])

    def test_launch_failure_and_coordinator_interrupt_reap_only_owned_driver(self):
        for interrupt in (False,True):
            with self.subTest(interrupt=interrupt), tempfile.TemporaryDirectory() as d:
                trace=[]
                class DomainFake:
                    label='driver'
                    def __init__(self,*args):pass
                    def snapshot(self):return {'monotonic_ns':1,'cpu_stat':{'usage_usec':0},'process_count':0}
                    def kill(self):trace.append('kill')
                    def remove(self):trace.append('remove');return True
                class SamplerFake:
                    def __init__(self,*args,**kwargs):pass
                    def start(self):return self
                    def stop(self):trace.append('sampler_stop');return []
                class ProcessFake:
                    returncode=None
                    calls=0
                    def wait(self,timeout):
                        self.calls+=1
                        if self.calls==1:raise KeyboardInterrupt('coordinator')
                        trace.append('reap');self.returncode=-9;return -9
                    def poll(self):return self.returncode
                h=Harness.__new__(Harness);h.root=Path(d);h.protocol=Path(d)/'protocol.json'
                h.uid=os.getuid();h.gid=os.getgid();h.db_domain=None
                h.identity={'protocol_sha256':'a'*64,'manifest_sha256':'b'*64}
                h.manifest={'client':{'command':['not-executed']},'driver_cores':1,'driver_cpuset':'2'}
                h.launch=mock.Mock(return_value=ProcessFake()) if interrupt else mock.Mock(side_effect=OSError('launch failed'))
                s=types.SimpleNamespace(startup={'identity':{}},address='unused:1',metrics=lambda:{},domain=DomainFake())
                expected=KeyboardInterrupt if interrupt else OSError
                with mock.patch.object(linux_harness,'Domain',DomainFake),mock.patch.object(linux_harness,'Sampler',SamplerFake), \
                     mock.patch.object(linux_harness.os,'chown'), self.assertRaises(expected):
                    h.client(s,'ok',1,1,Path(d)/'driver',events=False)
                self.assertIn('sampler_stop',trace);self.assertIn('kill',trace);self.assertIn('remove',trace)
                receipt=read_json(Path(d)/'driver/driver-cleanup.json')
                self.assertTrue(receipt['cgroup_removed'])
                self.assertEqual(receipt['process_reaped'],interrupt)
                if interrupt:self.assertIn('reap',trace)

    def test_known_shutdown_cpu_is_included_and_separately_reported(self):
        report={'ledger':{'correct_total':10},'service_cost':{
            'before':{'cpu_stat':{'usage_usec':100,'user_usec':60,'system_usec':40}},
            'after':{'cpu_stat':{'usage_usec':200,'user_usec':100,'system_usec':100}}}}
        physical={'cpu_stat':{'usage_usec':1000,'user_usec':600,'system_usec':400},'process_count':1}
        termination={'forced':False,'cgroup_removed':True,'after':{
            'cpu_stat':{'usage_usec':1600,'user_usec':1000,'system_usec':600},'process_count':0}}
        result=full_lifecycle_cost(report,physical,True,termination,{})
        self.assertTrue(result['termination_cpu_accounted'])
        self.assertAlmostEqual(result['cpu_seconds'],.0015)
        self.assertAlmostEqual(result['post_driver_cpu_seconds'],.0014)
        self.assertAlmostEqual(result['post_quiet_shutdown_cpu_seconds'],.0006)
        self.assertAlmostEqual(sum(result['cpu_seconds_by_mode'].values()),.0015)


if __name__ == '__main__':
    unittest.main()
