"""Pure A/A orchestration/decision tests; never starts a service or cgroup."""
from contextlib import ExitStack,redirect_stdout
import io
from pathlib import Path
import tempfile
import types
import unittest
from unittest.mock import Mock,patch

import memory_aa as m

SOURCE=Path(__file__).resolve().parents[3]
m.helpers(SOURCE)


def plan():return m.plan_for(dict(root='/frozen/B',stack='ku',source_commit=m.BASELINE,binary_sha256='a'*64))


def rows(ratios=None):
    p=plan();ratios=ratios or[1]*6
    return [dict(name=w['name'],group=w['group'],order=w['order'],memory={
        'steady_median_bytes':3400000*(ratios[w['group']-1]if w['name']=='B'else 1),'sample_count':30})for w in p['windows']]


class MemoryAATests(unittest.TestCase):
    def test_plan_fixes_same_binding_twelve_windows_and_all_thresholds(self):
        p=plan();self.assertEqual(p['versions']['A'],p['versions']['B'])
        self.assertEqual([x['name']for x in p['windows']],['A','B','B','A']*3)
        self.assertTrue(all((w['workload'],w['rate'],w['warmup_seconds'],w['score_seconds'],w['contract'])==('db',200,10,30,'pg-strict')for w in p['windows']))
        self.assertEqual((p['cooldown_seconds'],p['observer_cpu'],p['maximum_windows'],p['maximum_replacements'],p['operation_alarm_seconds']),(60,3,12,0,720))
        self.assertEqual(p['required_kernel'],m.KERNEL);self.assertEqual(p['required_online_cpus'],[0,1,2,3])
        self.assertEqual(p['thresholds'],{'each_pair':[.97,1.03],'overall_95':[.985,1.015],'AB_over_BA_geomeans':[.985,1.015]})
        self.assertEqual(p['minimum_memory_samples'],5);self.assertEqual(p['latency_p99_SLO_ms'],250)
        self.assertFalse(p['candidate_accepted']);self.assertFalse(p['normal_score_eligible'])

    def test_prepare_reuses_only_exact_normal_B_driver_without_builds(self):
        with tempfile.TemporaryDirectory()as temp:
            root=Path(temp)/'new';binding={'same':'normal'}
            manifest={'protocol':'/verified/protocol','stacks':{'ku':{}},'uid':1000,'gid':1000}
            with patch.object(m.os,'geteuid',return_value=1000),patch.object(m.reuse,'restore_current_pg_builds',
                    return_value=({'B':manifest,'Driver':{}},{'B':{'old':'normal'}},{'B':binding}))as restore,\
                    patch.object(m.fc,'version_identity',return_value=binding):
                result=m.prepare(types.SimpleNamespace(root=root,source=SOURCE,archive=Path('/one/existing.tar.gz')))
            restore.assert_called_once_with(Path('/one/existing.tar.gz'),root/'cache',SOURCE,typed=False)
            self.assertEqual(result['new_builds'],0);self.assertEqual(result['maximum_windows'],12)
            p=m.h.load(root/'frozen-plan.json');self.assertEqual(p['versions']['A'],p['versions']['B'])
            self.assertEqual(m.h.load(root/'prepare.json')['imported_measurement_windows'],0)
            self.assertFalse((root/'BP').exists())

    def test_decision_accepts_exact_identity_repeatability_without_adoption(self):
        result=m.decision(plan(),rows(),True)
        self.assertTrue(result['conditions_adjudicable']);self.assertEqual(result['window_ratios'],[1]*6)
        self.assertEqual(result['AB_over_BA_geomeans'],1)
        self.assertFalse(result['candidate_accepted']);self.assertFalse(result['original_3pct_guard_passed'])
        self.assertFalse(result['formal_qualified']);self.assertFalse(result['product_benefit'])

    def test_decision_rejects_pair_interval_and_hidden_order_effect(self):
        for ratios,low,high,reason in (([1.04,1,1,1,1,1],.99,1.01,'individual'),
                ([1]*6,.98,1.02,'A/A95%'),([1.02,.98]*3,.99,1.01,'AB/BA')):
            with self.subTest(reason=reason),patch.object(m.fc,'paired_interval',return_value={
                    'status':'estimated','ratios':ratios,'lower95':low,'upper95':high,'estimate':1}):
                result=m.decision(plan(),rows(ratios),True)
            self.assertFalse(result['conditions_adjudicable']);self.assertTrue(any(reason in r for r in result['reasons']))
        self.assertFalse(m.decision(plan(),rows()[:-1],False)['conditions_adjudicable'])

    def run_mock(self,*,samples=30,p99=1,fixture_error=False):
        with tempfile.TemporaryDirectory()as temp,ExitStack()as mocks:
            root=Path(temp);p=plan();binding={'same':'normal'}
            m.h.save(root/'frozen-plan.json',p)
            m.h.save(root/'prepare.json',{'entry_sha256':m.h.sha(m.__file__),'same_binary_binding':binding,
                'helper_sha256':{n:m.h.sha(SOURCE/'tests/benchmarks/cross_stack'/n)for n in m.HELPERS},'uid':1000,'gid':1000})
            private=root/'private.json';m.h.save(private,{'db_url':'local-fixture','db_cgroup':'owned-db'})
            fixture=types.SimpleNamespace(harness=object(),manifest_path=private,start=Mock(),
                stop=Mock(return_value={'forced':False,'cgroup_removed':True,'after':{'process_count':0}}))
            observer=types.SimpleNamespace(domain=object(),record={'restored':True,'removed':True},
                validate=Mock(return_value={'tid_affinities':{'100':[3]}}),__enter__=Mock(),__exit__=Mock())
            if fixture_error:
                fixture.start.side_effect=RuntimeError('fixture original failure')
                fixture.stop.side_effect=RuntimeError('database cleanup failure')
                observer.__exit__.side_effect=RuntimeError('observer cleanup failure')
            calls=[]
            def window(p,name,workload,rate,warm,score,output,**kw):
                calls.append((name,workload,rate,warm,score,kw['binding']))
                return {'name':name,'memory':{'steady_median_bytes':3400000,'sample_count':samples},
                        'latency':{'p99_ms':p99},'environment_quality':'unknown'}
            mocks.enter_context(patch.object(m.os,'geteuid',return_value=0))
            mocks.enter_context(patch.object(m.fc,'version_identity',return_value=binding))
            mocks.enter_context(patch.object(m,'platform_identity',return_value={'boot_sha256':'one-fixed-boot','kernel':m.KERNEL}))
            mocks.enter_context(patch.object(m.h.db,'Database',return_value=fixture))
            mocks.enter_context(patch.object(m,'ObserverPlacement',return_value=observer))
            mocks.enter_context(patch.object(m.fc,'window',side_effect=window))
            mocks.enter_context(patch.object(m.h,'normal_diagnostic_quality',return_value=(True,{})))
            sleep=mocks.enter_context(patch.object(m.time,'sleep'))
            alarms=mocks.enter_context(patch.object(m.signal,'alarm'));mocks.enter_context(patch.object(m.signal,'signal'))
            with redirect_stdout(io.StringIO()):result=m.run(types.SimpleNamespace(root=root,source=SOURCE))
            return result,calls,sleep.call_args_list,alarms.call_args_list,fixture,observer

    def test_run_reuses_original_window_fixed_cooldown_and_cleans(self):
        result,calls,sleeps,alarms,fixture,observer=self.run_mock()
        self.assertTrue(result['execution_complete']);self.assertTrue(result['decision']['conditions_adjudicable'])
        self.assertEqual(len(calls),12);self.assertEqual([c[0]for c in calls],['A','B','B','A']*3)
        self.assertTrue(all(c[1:5]==('db',200,10,30)for c in calls));self.assertEqual(sleeps[0].args,(60,))
        self.assertEqual(len(sleeps),1);self.assertEqual([c.args[0]for c in alarms],[720,0])
        fixture.stop.assert_called_once();observer.__exit__.assert_called_once()
        self.assertTrue(all(r['environment_quality']=='unknown'for r in result['rows']))

    def test_original_sample_and_latency_gates_stop_without_replacement(self):
        for samples,p99 in((4,1),(30,251)):
            with self.subTest(samples=samples,p99=p99):
                result,calls,*_=self.run_mock(samples=samples,p99=p99)
                self.assertEqual(len(calls),1);self.assertFalse(result['execution_complete'])
                self.assertFalse(result['decision']['conditions_adjudicable']);self.assertTrue(result['database_cleanup']['cgroup_removed'])

    def test_original_and_both_cleanup_errors_are_retained(self):
        result,calls,*_=self.run_mock(fixture_error=True)
        self.assertFalse(calls);self.assertIn('fixture original failure',result['error'])
        self.assertIn('database cleanup failure',result['database_cleanup']['error'])
        self.assertIn('observer cleanup failure',result['observer_cleanup']['error'])
        self.assertFalse(result['execution_complete'])

    def test_observer_restore_attempts_all_owned_masks_after_error(self):
        with tempfile.TemporaryDirectory()as temp:
            observer=m.ObserverPlacement(object(),Path(temp)/'record.json')
            observer.origin=Path(temp)/'origin';observer.origin.mkdir()
            observer.affinities={111:{0,1},222:{2,3}};observer.domain=types.SimpleNamespace(remove=Mock())
            old_iter=Path.iterdir
            def entries(path):return iter([Path('/owned/111'),Path('/owned/222')])if str(path)=='/proc/self/task'else old_iter(path)
            def affinity(tid,mask):
                if tid==111:raise OSError('fixture restore failure')
            with patch.object(Path,'iterdir',entries),patch.object(m.os,'sched_setaffinity',side_effect=affinity,create=True)as setter,\
                    self.assertRaises(RuntimeError):observer.__exit__(None,None,None)
            self.assertEqual([c.args[0]for c in setter.call_args_list],[111,222])
            observer.domain.remove.assert_not_called();self.assertFalse(observer.record['restored'])
            self.assertIn('fixture restore failure',observer.record['cleanup_errors'][0])


if __name__=='__main__':unittest.main()
