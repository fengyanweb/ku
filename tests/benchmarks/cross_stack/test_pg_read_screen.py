"""Pure contracts for the existing bounded PG/HTTP normal screen; no services."""
import copy
from contextlib import ExitStack
import tempfile
from pathlib import Path
import types
import unittest
from unittest.mock import patch

import pg_read_screen as s

s.helpers(Path(__file__).resolve().parents[3])
CANDIDATE='c'*40


def versions(root,workload):
    return {name:dict(root=str(root/name),source_commit=s.BASELINE if name in ('A','AP') else CANDIDATE)
        for name in (('A','B') if workload=='ok' else ('A','B','AP','BP'))}


def row(name,workload,contract):
    factor=1 if name in ('A','AP') else .95
    return dict(name=name,full_cpu_us=100*factor,cpu_us=99*factor,
        memory={'steady_median_bytes':4000000},latency={'p99_ms':1.,'p999_ms':2.},
        attempt_delay={'p99_ms':.1,'p999_ms':.2},send_delay={'p99_ms':.1,'p999_ms':.2},
        comparison_contract={'workload':workload,'contract':contract})


class NormalReadScreenTests(unittest.TestCase):
    def test_default_db_plan_keeps_twelve_original_windows_and_thresholds(self):
        v=versions(Path('/frozen'),'db');p=s.make_plan(v,CANDIDATE,s.h.RESOURCES,s.h.POLICY)
        self.assertEqual(p,s.make_plan(v,CANDIDATE,s.h.RESOURCES,s.h.POLICY,'db'))
        self.assertEqual((p['maximum_windows'],p['maximum_replacements'],p['observation_alarm_seconds']),(12,0,480))
        self.assertEqual([w['name']for w in p['windows']],['A','B','B','A','A','B','AP','BP','BP','AP','AP','BP'])
        self.assertTrue(all((w['workload'],w['rate'],w['warmup_seconds'],w['score_seconds'])==('db',200,3,15)for w in p['windows']))
        self.assertEqual(p['expansion_rule']['at_least_one_path_CPU_point_maximum'],.98)
        self.assertEqual(p['expansion_rule']['at_least_one_path_CPU_upper95_strictly_below'],1)

    def test_ok_plan_only_six_ordinary_windows_and_no_adoption(self):
        p=s.make_plan(versions(Path('/frozen'),'ok'),CANDIDATE,s.h.RESOURCES,s.h.POLICY,'ok')
        self.assertEqual((p['maximum_windows'],p['maximum_replacements']),(6,0))
        self.assertEqual([w['name']for w in p['windows']],['A','B','B','A','A','B'])
        self.assertTrue(all((w['path'],w['workload'],w['rate'],w['warmup_seconds'],w['score_seconds'],w['contract'])==('ok','ok',2000,3,15,'text')for w in p['windows']))
        self.assertFalse(p['normal_score_eligible']);self.assertFalse(p['candidate_accepted']);self.assertFalse(p['formal_qualified'])
        with self.assertRaises(ValueError):s.make_plan(versions(Path('/frozen'),'db'),CANDIDATE,{}, {},'ok')

    def test_ok_prepare_never_reads_or_builds_typed_inputs(self):
        with tempfile.TemporaryDirectory() as temp,ExitStack() as mocks:
            root=Path(temp);protocol=root/'protocol';protocol.write_text('frozen protocol');v=versions(root,'ok')
            original=dict(prepatch=s.BASELINE,candidate=CANDIDATE,versions=v,resources=s.h.RESOURCES,
                quality_policy=s.h.POLICY,pg_input_guard_workload='ok')
            bindings={}
            for name in ('A','B'):
                source=root/(name+'.c');source.write_text('normal '+name)
                bindings[name]={'files':{k:{'sha256':'same'}for k in ('expanded source','driver ELF','libpq')}}
                bindings[name]['files']['generated C']={'path':str(source),'sha256':s.h.sha(source)}
                s.h.save(root/name/'manifest.json',{'protocol':str(protocol),'stacks':{'ku':{'build':{'native':{'server':{'unchanged':name}}}}}})
            s.h.save(root/'Driver/manifest.json',{'client':{},'protocol':str(protocol)})
            s.h.save(root/'frozen-plan.json',original);s.h.save(root/'build-identities.json',bindings)
            s.h.save(root/'hosted-state.json',{'identity':{'candidate':CANDIDATE,'baseline':s.BASELINE,'rust_toolchain':'1.99.1'},'steps':{},'uid':1000,'gid':1000})
            old_sha=s.h.sha
            mocks.enter_context(patch.object(s.os,'geteuid',return_value=1000))
            mocks.enter_context(patch.object(s.h,'sha',side_effect=lambda p:s.reuse.CURRENT_PG_FILES['declared:protocol']if Path(p)==protocol else old_sha(p)))
            check=mocks.enter_context(patch.object(s,'check_binding',side_effect=lambda p,n,**kw:copy.deepcopy(bindings[n])))
            mocks.enter_context(patch.object(s.fc,'version_identity',side_effect=lambda p,n:copy.deepcopy(bindings[n])))
            typed=mocks.enter_context(patch.object(s.h,'prepare_common_typed_guards'))
            result=s.prepare(types.SimpleNamespace(root=root,workload='ok'))
            self.assertEqual(result['window_count'],6);typed.assert_not_called()
            self.assertTrue(all(call.args[1]in('A','B')for call in check.call_args_list))
            self.assertFalse((root/'AP').exists());self.assertFalse((root/'BP').exists())
            prepared=s.h.load(root/'pg-read-screen/prepare.json')
            self.assertIsNone(prepared['cached_typed_binding']);self.assertFalse(prepared['candidate_typed_native_only'])
            self.assertEqual(set(s.h.load(root/'pg-read-screen/build-identities.json')),{'A','B'})
            self.assertEqual(sorted(p.name for p in(root/'pg-read-screen/inputs').iterdir()),['B'])

    def run_fixed(self,workload,invalid=False):
        with tempfile.TemporaryDirectory() as temp,ExitStack() as mocks:
            root=Path(temp);directory=root/'pg-read-screen';directory.mkdir()
            p=s.make_plan(versions(root,workload),CANDIDATE,s.h.RESOURCES,s.h.POLICY,workload)
            bindings={name:{'frozen':name}for name in p['versions']}
            s.h.save(root/'frozen-plan.json',{'original':True});s.h.save(directory/'frozen-plan.json',p)
            s.h.save(directory/'build-identities.json',bindings)
            s.h.save(directory/'prepare.json',{'entry_sha256':s.h.sha(s.__file__),'normal_plan_sha256':s.h.sha(root/'frozen-plan.json'),'helper_sha256':{},'uid':1000,'gid':1000})
            private=root/'private.json';state=root/'database-state.json';s.h.save(private,{'db_url':'fixture','db_cgroup':'fixture'});s.h.save(state,{'limits':{}})
            fixture=types.SimpleNamespace(start=lambda:None,stop=lambda:{'forced':False,'cgroup_removed':True,'after':{'process_count':0}},manifest_path=private,state_path=state)
            calls=[]
            def window(plan,name,business,rate,warm,score,dest,**kw):
                calls.append((name,business,rate,warm,score,kw['job']['contract']))
                return row(name,business,kw['job']['contract'])
            old_sha=s.h.sha
            mocks.enter_context(patch.object(s.h,'sha',side_effect=lambda p:'boot'if str(p)=='/proc/sys/kernel/random/boot_id'else old_sha(p)))
            mocks.enter_context(patch.object(s.os,'geteuid',return_value=0))
            mocks.enter_context(patch.object(s,'check_binding',side_effect=lambda p,n,**kw:bindings[n]))
            mocks.enter_context(patch.object(s.fc,'version_identity',side_effect=lambda p,n:bindings[n]))
            mocks.enter_context(patch.object(s.h.db,'Database',return_value=fixture))
            mocks.enter_context(patch.object(s.fc,'window',side_effect=window))
            mocks.enter_context(patch.object(s.h,'normal_diagnostic_quality',return_value=(not invalid,{})))
            mocks.enter_context(patch.object(s.signal,'signal'));mocks.enter_context(patch.object(s.signal,'alarm'))
            outcome=s.run(types.SimpleNamespace(root=root,workload=workload))
            return calls,outcome,s.h.load(directory/'summary.json')

    def test_run_uses_existing_window_contracts_for_ok_and_default_db(self):
        for workload,count,rate in(('ok',6,2000),('db',12,200)):
            with self.subTest(workload=workload):
                calls,outcome,summary=self.run_fixed(workload)
                self.assertEqual(len(calls),count);self.assertTrue(outcome['execution_complete'])
                self.assertTrue(all(call[1:5]==(workload,rate,3,15)for call in calls))
                self.assertEqual([call[0]for call in calls[:6]],['A','B','B','A','A','B'])
                self.assertFalse(summary['comparison']['original_3pct_guard_passed']);self.assertFalse(summary['comparison']['candidate_accepted'])
                self.assertIn('memory.steady_median_bytes',next(iter(summary['comparison']['paths'].values()))['metrics'])

    def test_invalid_ok_window_stops_with_no_replacement_and_keeps_cleanup(self):
        calls,outcome,summary=self.run_fixed('ok',invalid=True)
        self.assertEqual(len(calls),1);self.assertFalse(outcome['execution_complete'])
        self.assertEqual(len(summary['stopped']),1);self.assertTrue(summary['database_cleanup']['cgroup_removed'])
        self.assertFalse(summary['comparison']['worth_expanded_validation'])

    def test_ok_workflow_routes_only_its_contract_not_pg_specific_observations(self):
        text=(Path(__file__).resolve().parents[3]/'.github/workflows/native-three-os.yml').read_text()
        job=text.split('\n  pg-input-guard:\n',1)[1].split('\n  pg-input-windows:\n',1)[0]
        self.assertIn('pg_read_screen.py --phase prepare --workload "$KU_PG_GUARD_WORKLOAD"',job)
        self.assertIn('pg_read_screen.py --phase run --workload "$KU_PG_GUARD_WORKLOAD"',job)
        for name in('Check the new read boundary and related PG contracts','Prepare native candidate-only I/O observations without rebuilding Rust','Confirm candidate I/O and event counts once per PG path'):
            step=job.split('- name: '+name,1)[1].split('shell: bash',1)[0]
            self.assertIn("inputs.self_workload != 'ok'",step)
        http=job.split('- name: Check the current HTTP first-read contract',1)[1].split('- name:',1)[0]
        self.assertIn("inputs.self_workload == 'ok'",http)
        self.assertIn('native_http_validates_unmaterialized_headers_and_owns_route_snapshot',http)
        self.assertEqual(http.count('--test native_http_test "$http_read_test" -- --exact'),2)
        self.assertNotIn('native_pg_',http);self.assertIn('clang-18 -O2 -g -fno-omit-frame-pointer -fsanitize=address,undefined',http)
        self.assertIn('KU_BIN="$KU_PG_GUARD_ROOT/B/build/ku-cargo/release/ku"',http)
        self.assertEqual(http.count("grep -F 'http-owner-first-read-no-peek'"),2)
        for name in('native_http_reuses_framed_requests_and_yields_idle_connections',
                    'native_http_async_idle_probe_preserves_reuse_partial_pipeline_and_half_close',
                    'native_http_reset_retires_async_work_but_write_half_close_keeps_response'):
            self.assertIn(name,http)
        self.assertEqual(http.count('-fsanitize=address,undefined'),1)
        self.assertIn('if grep -Ei \'skip(ped)?:\'',http)


if __name__=='__main__':unittest.main()
