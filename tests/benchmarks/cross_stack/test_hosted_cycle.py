"""Pure receipts/plans. No compiler, cgroup, database or service is started."""
from pathlib import Path
import json
import os
from contextlib import ExitStack
import tarfile
import tempfile
import types
from unittest.mock import patch
import unittest
import hosted_cycle as h
import focus_cycle as fc


class HostedCycleTests(unittest.TestCase):
    def plan(self):
        return h.frozen_plan({n:{'root':'/unused/'+n,'stack':'rust' if n.startswith('R') else 'ku'}
            for n in ['A0','B','R','BJ','BP','BD','RS','RJ','RP','RPS']})

    def test_five_registered_groups_and_pg_sharing_are_actual_windows(self):
        plan=self.plan();jobs=fc.jobs_for(plan,'final');self.assertEqual(len(jobs),80)
        self.assertEqual({t['metric'] for t in plan['targets']},{'full_cpu_us','memory.steady_median_bytes'})
        pg=[x for x in jobs if x['contract']=='pg-shared']
        for group in range(1,6):self.assertEqual(sorted(x['name'] for x in pg if x['group']==group),['A0','B','R','RS'])
        self.assertFalse(any(x['name']=='BD' for x in jobs))
        self.assertTrue(all(x['warmup']==10 and x['duration']==30 for x in jobs))

    def test_capacity_and_continuous_limits_are_frozen(self):
        plan=self.plan();self.assertEqual(plan['capacity']['maximum_points'],8)
        self.assertEqual(plan['capacity']['refinements'],2);self.assertEqual(plan['capacity']['recovery_maximum_seconds'],5)
        self.assertEqual(fc.stability_config(plan)['seconds'],1800);self.assertEqual(plan['resume']['maximum_replacements'],0)

    def controls(self,root,*,fault=None,missing=False,spread=False):
        rows=[]
        for i in range(3):
            domains=[]
            for label in ['service','driver','database']:
                domains.append([{'label':label,'cpu_stat':{'throttled_usec':0},'memory_events':{'oom':0,'oom_kill':0},
                    'memory_stat':{'pgmajfault':1 if fault==label and end else 0}} for end in [0,1]])
            resources=root/('resources%d.json'%i)
            h.save(resources,{'samples':[{'domains':[d[k] for d in domains]} for k in [0,1]]})
            report=root/('client%d.json'%i)
            h.save(report,{'service_cost':{'before':{} if missing else {'endpoint':True},'after':{'endpoint':True},'samples_path':str(resources)}})
            rows.append(dict(score_report=str(report),cpu_us=(130 if spread and i==2 else 100+i),status='observed',
                functional_valid=True,accounting_valid=True,arrival_schedule_quality='pass',natural_reclamation=True))
        return {'rows':rows}

    def test_environment_pass_is_derived_from_three_actual_reports(self):
        with tempfile.TemporaryDirectory() as d:
            root=Path(d);record=h.environment_record(self.controls(root),root/'qualification.json')
            self.assertEqual(record['quality'],'pass');self.assertEqual(len(record['source_reports']),3)

    def test_major_fault_in_driver_is_not_hidden(self):
        with tempfile.TemporaryDirectory() as d:
            root=Path(d);record=h.environment_record(self.controls(root,fault='driver'),root/'qualification.json')
            self.assertEqual(record['quality'],'fail');self.assertIn('driver:memory_stat.pgmajfault',record['faults'])

    def test_missing_endpoints_and_control_drift_cannot_pass(self):
        with tempfile.TemporaryDirectory() as d:
            root=Path(d);self.assertEqual(h.environment_record(self.controls(root,missing=True),root/'q.json')['quality'],'unknown')
            self.assertEqual(h.environment_record(self.controls(root,spread=True),root/'q2.json')['quality'],'fail')

    def test_completed_stage_never_reexecuted_for_better_result(self):
        with tempfile.TemporaryDirectory() as d:
            root=Path(d);state={'steps':{}};calls=[]
            first=h.step(state,root,'test',lambda:calls.append('first') or {'goal_status':'not_met'})
            second=h.step(state,root,'test',lambda:calls.append('retry') or {},resume=True)
            self.assertEqual(first,second);self.assertEqual(calls,['first'])

    def test_incomplete_build_is_retained_without_implicit_retry(self):
        with tempfile.TemporaryDirectory() as d:
            with self.assertRaises(ValueError):h.step({'steps':{'build':{'state':'failed'}}},Path(d),'build',lambda:None)

    def test_archive_keeps_normal_artifacts_excludes_cache_and_redacts(self):
        with tempfile.TemporaryDirectory() as d:
            root=Path(d)
            for name in ['B/build/ku-cargo/release/ku','R/build/rust-cargo/release/ku-cross-stack-rust','B/build/ku-native/normal.c',
                         'B/src/tests/benchmarks/cross_stack/rust/src/main.rs','cli-cache/release/ku','B/src/src/runtime.rs',
                         'compiler/http-train-ku/http-profiles/train.profraw','compiler/KGenerate/build-time-profiles/build.profraw']:
                path=root/name;path.parent.mkdir(parents=True,exist_ok=True);path.write_bytes(b'normal bytes')
            (root/'B/build/ku-cargo/release/ku').chmod(0o755)
            h.save(root/'state.json',{'db_url':'postgresql://private:pw@host/db','owner_token':'secret',
                'command':['postgresql://private/db'],'cancel_token_count':3,'root':str(root)})
            result=h.archive(types.SimpleNamespace(root=root),{'steps':{}})
            with tarfile.open(result['path']) as tar:
                names=tar.getnames();self.assertIn('B/build/ku-cargo/release/ku',names)
                self.assertIn('R/build/rust-cargo/release/ku-cross-stack-rust',names)
                self.assertIn('B/src/tests/benchmarks/cross_stack/rust/src/main.rs',names)
                self.assertNotIn('cli-cache/release/ku',names);self.assertNotIn('B/src/src/runtime.rs',names)
                profile='compiler/http-train-ku/http-profiles/train.profraw'
                self.assertIn(profile,names);self.assertNotIn('compiler/KGenerate/build-time-profiles/build.profraw',names)
                mapping={x['path']:x for x in json.load(tar.extractfile('portable-file-map.json'))['files']}
                self.assertEqual(tar.getmember('B/build/ku-cargo/release/ku').mode,0o755)
                self.assertEqual(mapping['B/build/ku-cargo/release/ku']['mode'],0o755)
                self.assertEqual(mapping[profile]['original_sha256'],h.sha(root/profile))
                redacted=json.load(tar.extractfile('state.json'));self.assertNotIn('db_url',redacted);self.assertNotIn('owner_token',redacted)
                self.assertEqual(redacted['cancel_token_count'],3);self.assertNotIn('postgresql://',str(redacted))
                self.assertEqual(tar.extractfile('B/build/ku-native/normal.c').read(),b'normal bytes')
            with self.assertRaises(ValueError):h.archive(types.SimpleNamespace(root=root),{})

    def test_capacity_and_stability_are_required_for_acceptance(self):
        final={'rows':[{'functional_valid':True,'accounting_valid':True}],'execution_complete':True}
        cap={'execution_complete':True,'capacity':[dict(name=n,workload=w,functional_valid=True,accounting_valid=True,
            all_recovery_passed=True,qualified_lower_rate=100,upper_rate=None,boundary_observed=False)
            for w in ['json_object','db'] for n in ['B','R']]}
        stable={'execution_complete':True,'formal_qualified':True,'rows':[{'natural_reclamation':True,
            'segments':[{'streams':{'mixed':{'assessment':{'functional_valid':True,'accounting_valid':True}}}}]}]}
        result=h.acceptance_result(final,cap,stable,'met')
        self.assertTrue(result['functional_acceptance']);self.assertEqual(result['goal_status'],'unknown')
        result=h.acceptance_result(final,cap,{},'met')
        self.assertFalse(result['functional_acceptance']);self.assertFalse(result['execution_complete'])
        cap['capacity'][0]['functional_valid']=False
        self.assertEqual(h.acceptance_result(final,cap,stable,'met')['goal_status'],'not_met')

    def test_coarse_search_cannot_exceed_six_points_before_refinement(self):
        cap=self.plan()['capacity']
        for kind,start in cap['start_rates'].items():
            self.assertEqual(cap['maximum_rates'][kind],start*cap['factor']**5)
        strict=[x for x in self.plan()['targets'] if x.get('display_contract')=='pg-strict']
        self.assertEqual({x['denominator'] for x in strict},{'RS'})
        self.assertEqual({x['maximum_ratio'] for x in strict if x['metric']=='full_cpu_us'},{.90})

    def test_oversize_archive_returns_explicit_failure_not_silent_truncation(self):
        with tempfile.TemporaryDirectory() as d:
            root=Path(d);path=root/'normal.c';path.write_bytes(bytes(range(256))*100)
            with patch.object(h,'ARCHIVE_LIMIT',100):result=h.archive(types.SimpleNamespace(root=root),{'steps':{}})
            self.assertFalse(result['complete']);self.assertEqual(result['goal_status'],'blocked')
            self.assertTrue(path.exists())
            with tarfile.open(result['path']) as tar:self.assertIn('archive-failure.json',tar.getnames())


    def test_compiler_phase_has_48_short_balanced_comparisons_no_formal_or_capacity(self):
        versions=self.plan()['versions']
        for n in ['KO3','KThin','KPGO','RThin','RPGO']:versions[n]={'root':'/unused/'+n}
        plan=h.compiler_plan(versions);jobs=fc.jobs_for(plan,'all')
        self.assertEqual(len(jobs),48)
        self.assertTrue(all(j['phase']=='explore' and j['warmup']==5 and j['duration']==15 for j in jobs))
        self.assertEqual(sum(j['contract']=='direct-diagnostic' for j in jobs),6)
        self.assertFalse(plan['targets']);self.assertNotIn('capacity',plan);self.assertNotIn('stability',plan)
        self.assertEqual(plan['stop_after_consecutive_unusable_windows'],2)
        for entry in plan['schedule'][:2]:
            for name in entry['orders'][0]:self.assertEqual(len({order.index(name) for order in entry['orders']}),3)

    def test_compiler_http_training_both_sides_and_harness_restored_on_failure(self):
        import profile_training as pt
        for failed in [False,True]:
            with self.subTest(failed=failed),tempfile.TemporaryDirectory() as d,ExitStack() as patches:
                root=Path(d);(root/'compiler').mkdir();binary=root/'ELF';binary.write_bytes(b'fixture')
                fixture=root/'fixture/manifest.json';h.save(fixture,{'db_url':'fixture','db_cgroup':'/unused'})
                base=self.plan();h.save(root/'frozen-plan.json',base)
                state={'uid':os.getuid(),'gid':os.getgid(),'steps':{}}
                a=types.SimpleNamespace(root=root,source=root,tools=root)
                def fake_step(state,root,name,call,**unused):
                    if name.startswith('compiler-build-'):
                        variant=name.removeprefix('compiler-build-');stack='ku' if variant.startswith('K') else 'rust'
                        h.save(root/'compiler'/variant/'manifest.json',{'candidate_base':'1'*40,'stacks':{stack:{'command':[str(binary)]}}})
                        return {'passed':True}
                    return call()
                calls=[]
                def profile(manifest,out,stack,mode,tools):
                    calls.append((stack,mode))
                    if failed and mode=='validate':raise ValueError('heldout failure retained')
                    return {'passed':True,'mode':mode}
                old_fc,old_pt=fc.Harness,pt.Harness
                read_bytes=Path.read_bytes
                patches.enter_context(patch.object(Path,'read_bytes',lambda p:b'fixture-boot' if str(p)=='/proc/sys/kernel/random/boot_id' else read_bytes(p)))
                owned=types.SimpleNamespace(start=lambda:None,manifest_path=fixture,stop=lambda:{'stopped':True})
                patches.enter_context(patch.object(h.os,'geteuid',return_value=0))
                patches.enter_context(patch.object(h.db,'Database',return_value=owned))
                patches.enter_context(patch.object(h,'step',side_effect=fake_step))
                patches.enter_context(patch.object(h,'environment_record',return_value={'quality':'unknown'}))
                def measured(plan_path,*unused,**kwargs):
                    jobs=fc.jobs_for(h.load(plan_path),'all')
                    return {'execution_complete':False,'rows':[dict(j,cpu_us=1 if j['name']=='KO3' else 100,full_cpu_us=100,
                        functional_valid=False,accounting_valid=True,status='observed',arrival_schedule_quality='pass',environment_quality='pass') for j in jobs]}
                patches.enter_context(patch.object(fc,'execute',side_effect=measured))
                patches.enter_context(patch.object(fc,'paired_interval',side_effect=lambda a,b,**kw:{'estimate':sum(a)/sum(b) if a and b else None}))
                patches.enter_context(patch.object(fc,'version_identity',return_value={'fixture':True}))
                patches.enter_context(patch.object(pt,'run_http',side_effect=profile))
                if failed:
                    with self.assertRaisesRegex(ValueError,'heldout failure'):h.compiler(a,state)
                else:
                    result=h.compiler(a,state);self.assertFalse(result['functional_acceptance']);self.assertFalse(result['default_changed'])
                    self.assertEqual(calls,[('ku','train'),('ku','validate'),('rust','train'),('rust','validate')])
                    report=h.load(root/'compiler/comparisons.json')
                    self.assertTrue(all(c['metric']=='full_cpu_us' for c in report['comparisons']))
                    self.assertTrue(all(c['interval']['estimate']==1 for c in report['comparisons']))
                self.assertIs(fc.Harness,old_fc);self.assertIs(pt.Harness,old_pt)
                self.assertEqual(state['compiler_database_cleanup'],{'stopped':True})

    def test_compiler_archive_keeps_profile_and_elf_but_excludes_target_cache(self):
        root=Path('/unused')
        self.assertTrue(h.evidence_file(root/'compiler/RPGO/target/release/ku-cross-stack-rust',root))
        self.assertFalse(h.evidence_file(root/'compiler/RPGO/target/release/.fingerprint/dependency.json',root))
        self.assertTrue(h.evidence_file(root/'compiler/http-train-rust/merged.profdata',root))


if __name__=='__main__':unittest.main()
