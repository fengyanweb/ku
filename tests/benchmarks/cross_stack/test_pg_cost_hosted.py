"""Pure plan, reuse and observer cleanup checks; no compiler or service runs."""
from contextlib import ExitStack
import json
from pathlib import Path
import tempfile
import types
import unittest
from unittest.mock import patch

import hosted_cycle as h
import pg_cost_diagnostic as pg
import reuse_build as reuse


class PgCostHostedTests(unittest.TestCase):
    def args(self, root):
        return types.SimpleNamespace(root=root,source=root/'source',tools=root/'tools',
            archive=root/'inert.tgz',pg_cost_probe=True,source_run=reuse.OWNER_RUN,
            archive_sha256=reuse.OWNER_ARCHIVE_SHA,candidate=reuse.OWNER_PRODUCT,
            baseline=h.BASELINE,prepatch=h.FOLLOWUP_PREPATCH,screen_variants=[])

    def test_opt_in_is_distinct_and_rejects_all_other_modes(self):
        args=self.args(Path('/inert'))
        self.assertTrue(h.hosted_identity(args,'diagnostic')['pg_cost_probe'])
        args.pg_cost_probe=False
        self.assertNotIn('pg_cost_probe',h.hosted_identity(args,'diagnostic'))
        args.pg_cost_probe=True
        for other in ('memory_owner_probe','normal_self_probe','pg_failure_probe','pg_driver_screen',
                      'pg_queue_screen','http_short_screen','task_json_screen','common_http_screen','final_task_json'):
            setattr(args,other,True)
            with self.subTest(other=other),self.assertRaisesRegex(ValueError,'--pg-cost-probe'):
                h.validate_screen_selection(args,'diagnostic')
            setattr(args,other,False)
        with self.assertRaises(ValueError):h.validate_screen_selection(args,'followup-screen')

    def test_fixed_archive_reuse_binds_runtime_and_does_not_build_driver(self):
        with tempfile.TemporaryDirectory() as temp,ExitStack() as patches:
            root=Path(temp)/'output';args=self.args(root)
            def extract(*unused,**kwargs):
                root.mkdir()
                return {}
            patches.enter_context(patch.object(reuse.os,'geteuid',return_value=1))
            patches.enter_context(patch.object(reuse.platform,'system',return_value='Linux'))
            patches.enter_context(patch.object(reuse.platform,'machine',return_value='aarch64'))
            extract_mock=patches.enter_context(patch.object(reuse,'extract_builds',side_effect=extract))
            patches.enter_context(patch.object(reuse,'restore_json_inputs'))
            patches.enter_context(patch.object(reuse,'validate_protocol_inputs'))
            patches.enter_context(patch.object(reuse,'verify_manifest'))
            diff=patches.enter_context(patch.object(reuse.subprocess,'check_output',return_value=''))
            driver=patches.enter_context(patch.object(reuse,'new_driver'))
            build=patches.enter_context(patch.object(h,'prepare_pg_cost',return_value={'execution_complete':True}))
            self.assertTrue(reuse.prepare(args)['execution_complete'])
            self.assertEqual(extract_mock.call_args.kwargs,{'pg_cost_observation':True})
            self.assertEqual(set(extract_mock.call_args.args[3]),{'B','R'})
            driver.assert_not_called();build.assert_called_once()
            self.assertIn(reuse.PG_COST_RUNTIME,diff.call_args.args[0])
            state=h.load(root/'hosted-state.json')
            self.assertEqual(state['reused_build']['runtime_equivalent'],reuse.PG_COST_RUNTIME)
            self.assertEqual(state['reused_build']['imported_measurement_windows'],0)
        args=self.args(Path('/inert'));args.source_run='wrong'
        with patch.object(reuse,'extract_builds') as extract,self.assertRaisesRegex(ValueError,'fixed verified'):
            reuse.prepare(args)
        extract.assert_not_called()

    def test_frozen_plan_has_only_three_pg200_windows_and_control_math(self):
        plan=h.pg_cost_plan({'B':{},'PC':{}})
        self.assertEqual(plan['order'],[['B','before'],['PC','probe'],['B','after']])
        self.assertEqual((plan['rate'],plan['warmup_seconds'],plan['score_seconds']),(200,3,15))
        self.assertEqual((plan['maximum_windows'],plan['maximum_replacements']),(3,0))
        rows=[dict(turn=turn,cpu_us=cpu,full_cpu_us=cpu) for turn,cpu in [('before',10),('probe',15),('after',10)]]
        controls=h.pg_cost_controls(rows)
        self.assertTrue(controls['drift_valid'])
        self.assertEqual(controls['cpu_us']['probe_over_unsampled_geomean'],1.5)
        rows[-1]['cpu_us']=12
        self.assertFalse(h.pg_cost_controls(rows)['drift_valid'])

    def test_prepare_links_only_the_derived_c_and_keeps_original_normal_bytes(self):
        import compile_variants as cv
        with tempfile.TemporaryDirectory() as temp,ExitStack() as patches:
            root=Path(temp);args=self.args(root)
            binary=root/'normal-ELF';source=root/'normal.c';binary.write_bytes(b'normal');source.write_bytes(b'normal C')
            invocation=dict(flags=['-std=c11','-O2','-pthread'],compiler={'path':'clang-18'},
                library_inputs=[{'role':'libpq','selected_input':{'real_path':'/frozen/libpq.so'}}])
            native=dict(binary=str(binary),binary_sha256=h.sha(binary),generated_c=str(source),
                generated_c_sha256=h.sha(source),compiler_invocation=invocation)
            manifest=dict(protocol=str(root/'protocol.json'),client={'command':['frozen-driver']},
                candidate_base=reuse.OWNER_PRODUCT,stacks={'ku':{'command':[str(binary)],'build':{'native':{'server':native}}}})
            h.save(root/'B/manifest.json',manifest)
            def prepare(source,destination,commit):
                destination.write_bytes(b'diagnostic C')
                return {'diagnostic_c_sha256':h.sha(destination)}
            commands=[]
            class FakeCommands:
                rows=[]
                def __init__(self,*unused):pass
                def run(self,label,command,cwd,**kwargs):
                    commands.append(command)
                    Path(command[-1]).write_bytes(b'\x7fELF\x02\x01'+b'\0'*12+(183).to_bytes(2,'little'))
            patches.enter_context(patch.object(pg,'prepare',side_effect=prepare))
            patches.enter_context(patch.object(cv,'Commands',FakeCommands))
            patches.enter_context(patch.object(cv,'clean_environment',return_value={}))
            patches.enter_context(patch.object(h.fc,'version_identity',return_value={}))
            result=h.prepare_pg_cost(args,{})
            self.assertTrue(result['execution_complete']);self.assertEqual(len(commands),1)
            self.assertEqual(commands[0][2:5],['-std=c11','-O2','-pthread'])
            self.assertIn('-Wl,--wrap=pthread_create',commands[0])
            self.assertIn('/frozen/libpq.so',commands[0])
            self.assertEqual(binary.read_bytes(),b'normal');self.assertEqual(source.read_bytes(),b'normal C')
            self.assertEqual(h.load(root/'B/manifest.json'),manifest)
            self.assertFalse(h.load(root/'PC/manifest.json')['normal_score_eligible'])

    def test_three_windows_share_private_database_and_fail_closed_with_cleanup(self):
        for issue in (None,'drift','quality','ledger','database','exception'):
            with self.subTest(issue=issue),tempfile.TemporaryDirectory() as temp,ExitStack() as patches:
                root=Path(temp);args=self.args(root);state={'uid':1,'gid':1}
                h.save(root/'frozen-plan.json',h.pg_cost_plan({'B':{},'PC':{}}))
                h.save(root/'fixture/manifest.json',{'db_url':'private','db_cgroup':'private'})
                h.save(root/'fixture/state.json',{'limits':{}})
                fixture=types.SimpleNamespace(manifest_path=root/'fixture/manifest.json',state_path=root/'fixture/state.json',
                    start=lambda:None,stop=lambda:dict(forced=issue=='database',cgroup_removed=True,after={'process_count':0}))
                patches.enter_context(patch.object(h.os,'geteuid',return_value=0))
                patches.enter_context(patch.object(h.db,'Database',return_value=fixture))
                patches.enter_context(patch.object(h.fc,'version_identity',return_value={'identity':'same'}))
                patches.enter_context(patch.object(h,'sha',return_value='hash'))
                patches.enter_context(patch.object(h,'normal_diagnostic_quality',return_value=(issue!='quality',[])))
                patches.enter_context(patch.object(pg,'summarize',return_value={'valid':issue!='ledger'}))
                patches.enter_context(patch.object(h,'pg_failure_export',return_value={}))
                def window(plan,name,workload,rate,warmup,duration,output,**kwargs):
                    self.assertEqual((workload,rate,warmup,duration),('db',200,3,15))
                    if issue=='exception':raise RuntimeError('retained observer failure')
                    if name=='PC':
                        path=output/'service/service.log';path.parent.mkdir(parents=True)
                        path.write_text('KU_FLOW_DIAG '+json.dumps({'schema':'stub'})+'\n')
                    return dict(correct=3000,all_correct=True,natural_reclamation=True,
                        cpu_us=20 if issue=='drift' and output.name=='after' else 10,full_cpu_us=10)
                run=patches.enter_context(patch.object(h.fc,'window',side_effect=window))
                original=h.fc.Harness
                result=h.pg_cost_probe(args,state)
                self.assertIs(h.fc.Harness,original)
                self.assertIn('database_cleanup',result)
                self.assertEqual(result['execution_complete'],issue is None)
                self.assertLessEqual(run.call_count,3)
                self.assertFalse(result['candidate_accepted'])

    def test_workflow_has_bounded_inputs_and_fixed_reuse_no_fresh_build_mode(self):
        text=(Path(__file__).resolve().parents[3]/'.github/workflows/native-three-os.yml').read_text()
        inputs=text.split('    inputs:\n',1)[1].split('\npermissions:',1)[0]
        import re
        self.assertLessEqual(len(re.findall(r'^      [a-z][a-z0-9_]*:',inputs,re.M)),25)
        self.assertIn('      pg_cost_probe:\n',text)
        self.assertEqual(text.count('performance_task_json_args+=(--pg-cost-probe)'),2)
        self.assertIn('name: ku-pg-cost-observation',text)
        self.assertIn('PG_COST_PROBE" != true ] && [ "$KU_PERFORMANCE_V2_HTTP_SHORT_GOAL" != pg-ok-guard ]; then test "$performance_build_set" = followup-screen',text)


if __name__=='__main__':unittest.main()
