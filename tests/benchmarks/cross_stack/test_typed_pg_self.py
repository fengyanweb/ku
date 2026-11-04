"""Pure plan/control/cleanup tests. No perf, service, compiler or DB execution."""
import copy, json, tempfile, types, unittest
from pathlib import Path
from unittest.mock import patch
import typed_pg_self as t

class TypedPgSelfTests(unittest.TestCase):
    def test_fixed_six_windows_and_no_performance_claim(self):
        p=t.frozen_plan({})
        self.assertEqual(p['order'],[[n,x] for n in ('B','BP') for x in ('before','sample','after')])
        self.assertEqual((p['rate'],p['warmup_seconds'],p['score_seconds'],p['maximum_replacements']),(800,3,15,0))
        self.assertEqual((p['event'],p['perf_frequency']),('cpu-clock',499))
        self.assertFalse(p['formal_qualified']);self.assertFalse(p['normal_score_eligible'])

    def test_explicit_only_BP_keeps_sampling_work_and_limits(self):
        normal=t.frozen_plan({});only=t.frozen_plan({},only='BP')
        self.assertEqual(only['order'],[['BP',turn] for turn in ('before','sample','after')])
        self.assertEqual(only['maximum_windows'],3)
        for key in ('rate','warmup_seconds','score_seconds','maximum_replacements','event','perf_frequency',
                'maximum_control_cpu_ratio','observation_alarm_seconds','quality_policy','resources'):
            self.assertEqual(only[key],normal[key])
        self.assertNotIn('only',normal)
        with self.assertRaises(ValueError):t.frozen_plan({},only='B')

    def test_ok_is_only_current_ordinary_three_windows_with_fp_and_no_score(self):
        pg=t.frozen_plan({},only='BP')
        p=t.frozen_plan({},current_product=t.h.PG_CURRENT_BASELINE,callchains=True,only='B',workload='ok')
        self.assertEqual((p['workload'],p['contract'],p['rate'],p['warmup_seconds'],p['score_seconds']),('ok','text',2000,3,15))
        self.assertEqual(p['order'],[['B',turn] for turn in ('before','sample','after')])
        self.assertEqual((p['maximum_windows'],p['maximum_replacements']),(3,0))
        for key in ('resources','quality_policy','event','perf_frequency','maximum_control_cpu_ratio','observation_alarm_seconds'):
            self.assertEqual(p[key],pg[key])
        self.assertTrue(p['callchains']);self.assertFalse(p['formal_qualified']);self.assertFalse(p['normal_score_eligible'])
        for changes in ({'only':None},{'only':'BP'},{'current_product':None},{'current_product':'b'*40},{'callchains':False}):
            with self.subTest(changes=changes),self.assertRaises(ValueError):
                t.frozen_plan({},**dict(dict(current_product=t.h.PG_CURRENT_BASELINE,callchains=True,only='B',workload='ok'),**changes))

    def test_ok_requires_exact_cached_cli_elf_library_and_driver_for_both_arms(self):
        import reuse_build as rb
        with tempfile.TemporaryDirectory() as temp:
            root=Path(temp);protocol=root/'protocol.json';protocol.write_text('{}');receipt=root/'build.json'
            versions={'rustc':'rustc 1.99.1 (fixed)','cargo':'cargo 1.99.1 (fixed)'}
            receipt.write_text(json.dumps(dict(rust_toolchain='1.99.1',failure=None,exact_versions=versions,
                commands=[dict(label='ku-cli',command=['cargo','+1.99.1','build','--locked','--release'],exit_code=0)])))
            mode=dict(formatting_mode='helpers',json_response_mode='text',json_execution_mode='worker',pg_execution_mode='async',
                native_flags={'batch':['-std=c11','-O2'],'server':['-std=c11','-O2','-pthread']})
            bindings={};paths=[]
            for name,files in (('B',rb.CURRENT_PG_FILES),('BP',rb.CURRENT_TYPED_PG_FILES)):
                path=root/name/'manifest.json';path.parent.mkdir();paths.append(path)
                path.write_text(json.dumps(dict(candidate_base=t.h.PG_CURRENT_BASELINE,protocol=str(protocol),client={'command':['driver']},
                    rust_toolchain='1.99.1',versions=versions,build_receipt=str(receipt),stacks={'ku':{'build':dict(
                        rust_toolchain='1.99.1',cli_build_receipt=str(receipt),cli_build_receipt_sha256=t.h.sha(receipt),
                        native={'server':{'binary_sha256':files['ELF']}})}})))
                bindings[name]=dict(build_mode={**mode,'pg_read_mode':'int' if name=='B' else 'typed'},
                    files={key:{'sha256':value} for key,value in files.items()})
            with patch.object(t.fc,'version_identity',side_effect=lambda p,n:bindings[n]):
                plan,actual,_=t.identify(*paths,current_product=t.h.PG_CURRENT_BASELINE,callchains=True,only='B',workload='ok')
                self.assertEqual(set(actual),{'B','BP'});self.assertEqual(plan['only'],'B')
            for name,key in (('B','ELF'),('BP','ELF'),('B','Ku CLI'),('B','driver ELF'),('B','libpq')):
                changed=copy.deepcopy(bindings)
                # Shared dependencies may agree with each other and still be the wrong cached build.
                for arm in (('B','BP') if key in ('Ku CLI','driver ELF','libpq') else (name,)):
                    changed[arm]['files'][key]['sha256']='0'*64
                with self.subTest(name=name,key=key),patch.object(t.fc,'version_identity',side_effect=lambda p,n:changed[n]):
                    with self.assertRaisesRegex(ValueError,'exact cached'):
                        t.identify(*paths,current_product=t.h.PG_CURRENT_BASELINE,callchains=True,only='B',workload='ok')

    def test_drift_uses_same_cpu_field_and_unknown_never_passes(self):
        rows=[dict(turn=x,cpu_us=v,full_cpu_us=v+1) for x,v in [('before',100),('sample',120),('after',105)]]
        self.assertTrue(t.drift(rows)['drift_valid'])
        self.assertAlmostEqual(t.drift(rows)['sampled_cpu_over_control_geomean'],120/(100*105)**.5)
        rows[-1]['cpu_us']=111;self.assertFalse(t.drift(rows)['drift_valid'])
        rows[-1]['cpu_us']=None;self.assertFalse(t.drift(rows)['drift_valid'])

    def test_existing_archive_preserves_raw_symbols_and_elf_but_excludes_private_data(self):
        root=Path('/owned/probe')
        for name in ('B','BP'):
            for relative in (f'normal-self-probe/{name}/sample/score/perf.data',
                    f'normal-self-probe/{name}/sample/score/symfs/usr/lib/libpq.so.5.18',
                    f'inputs/{name}/bin/ku-server'):
                self.assertTrue(t.h.evidence_file(root/relative,root),relative)
        self.assertFalse(t.h.evidence_file(root/'fixture/database/data/pg_control',root))
        self.assertFalse(t.h.evidence_file(root/'fixture/database/socket/private.json',root))

    def test_identify_rejects_wrong_mode_input_or_driver(self):
        with tempfile.TemporaryDirectory() as temp:
            root=Path(temp);protocol=root/'protocol.json';protocol.write_text('{}')
            mode=dict(formatting_mode='helpers',json_response_mode='text',json_execution_mode='worker',pg_execution_mode='async',
                native_flags={'batch':['-std=c11','-O2'],'server':['-std=c11','-O2','-pthread']})
            bindings={};paths=[]
            for name in ('B','BP'):
                p=root/name/'manifest.json';p.parent.mkdir();paths.append(p)
                manifest=dict(candidate_base=t.FROZEN_BASE,protocol=str(protocol),client={'command':['driver']},
                    stacks={'ku':{'command':['server'],'build':{'native':{'server':{'binary_sha256':
                        '234300bba9b6fce603a178701ac289a82108398d5f35de3c0a7ad585f082367b' if name=='B' else
                        'faf34cec074f10b225ad880f149fb60f2fda9068fd192872ee00222344bd5b93'}}}}})
                p.write_text(json.dumps(manifest))
                bindings[name]=dict(build_mode={**mode,'pg_read_mode':'int' if name=='B' else 'typed'},
                    files={k:{'sha256':'c'*64} for k in ('Ku CLI','driver ELF','libpq')})
                bindings[name]['files']['expanded source']={'sha256':t.INPUTS[name]}
            for issue in ('none','mode','input','driver'):
                selected=copy.deepcopy(bindings)
                if issue=='mode':selected['BP']['build_mode']['pg_execution_mode']='sync'
                if issue=='input':selected['BP']['files']['expanded source']['sha256']='0'*64
                if issue=='driver':selected['BP']['files']['driver ELF']['sha256']='0'*64
                with patch.object(t.fc,'version_identity',side_effect=lambda p,n:selected[n]):
                    if issue=='none':self.assertEqual(set(t.identify(*paths)[0]['versions']),{'B','BP'})
                    else:
                        with self.assertRaises(ValueError):t.identify(*paths)
            with patch.object(t.fc,'version_identity',side_effect=lambda p,n:bindings[n]):
                for base in (t.FROZEN_DEFAULT,t.FROZEN_BASE):
                    for path in paths:
                        manifest=t.h.load(path);manifest['candidate_base']=base;path.write_text(json.dumps(manifest))
                    self.assertEqual(t.identify(*paths)[0]['versions']['BP']['source_commit'],base)
                changed=t.h.load(paths[1]);changed['candidate_base']=t.FROZEN_DEFAULT;paths[1].write_text(json.dumps(changed))
                with self.assertRaisesRegex(ValueError,'same fixed source'):t.identify(*paths)
                changed['candidate_base']='0'*40;paths[1].write_text(json.dumps(changed))
                with self.assertRaisesRegex(ValueError,'fixed archived source'):t.identify(*paths)

    def test_fresh_current_paths_require_actual_1991_receipt_and_keep_original_contract(self):
        with tempfile.TemporaryDirectory() as temp:
            root=Path(temp);product='d'*40;protocol=root/'protocol.json';protocol.write_text('{}')
            receipt=root/'CLI-build.json'
            versions={'rustc':'rustc 1.99.1 (fixed)','cargo':'cargo 1.99.1 (fixed)'}
            record=dict(rust_toolchain='1.99.1',failure=None,exact_versions=versions,
                commands=[dict(label='ku-cli',command=['cargo','+1.99.1','build','--locked','--release'],exit_code=0)])
            receipt.write_text(json.dumps(record));bindings={};paths=[]
            mode=dict(formatting_mode='helpers',json_response_mode='text',json_execution_mode='worker',pg_execution_mode='async',
                native_flags={'batch':['-std=c11','-O2'],'server':['-std=c11','-O2','-pthread']})
            for name in ('B','BP'):
                path=root/name/'manifest.json';path.parent.mkdir();paths.append(path)
                path.write_text(json.dumps(dict(candidate_base=product,protocol=str(protocol),client={'command':['driver']},
                    rust_toolchain='1.99.1',versions=versions,build_receipt=str(receipt),stacks={'ku':{'build':dict(
                        rust_toolchain='1.99.1',cli_build_receipt=str(receipt),cli_build_receipt_sha256=t.h.sha(receipt),
                        native={'server':{'binary_sha256':'f'*64}})}})))
                bindings[name]=dict(build_mode={**mode,'pg_read_mode':'int' if name=='B' else 'typed'},
                    files={k:{'sha256':'c'*64}for k in ('Ku CLI','driver ELF','libpq')})
                bindings[name]['files']['expanded source']={'sha256':t.INPUTS[name]}
            with patch.object(t.fc,'version_identity',side_effect=lambda p,n:bindings[n]):
                plan,_,_=t.identify(*paths,current_product=product,callchains=True)
                self.assertEqual(plan['current_product'],product);self.assertTrue(plan['callchains'])
                self.assertFalse(plan['formal_qualified']);self.assertEqual(plan['rate'],800)
                record['exact_versions']={'rustc':'rustc 1.89.0','cargo':'cargo 1.89.0'};receipt.write_text(json.dumps(record))
                for path in paths:
                    manifest=t.h.load(path);manifest['stacks']['ku']['build']['cli_build_receipt_sha256']=t.h.sha(receipt);path.write_text(json.dumps(manifest))
                with self.assertRaisesRegex(ValueError,'1.99.1'):
                    t.identify(*paths,current_product=product,callchains=True)

    def test_failed_preflight_starts_no_windows(self):
        with tempfile.TemporaryDirectory() as temp:
            root=Path(temp);paths=[];bindings={};versions={}
            for name in ('B','BP'):
                directory=root/name;directory.mkdir();m=directory/'manifest.json';m.write_text('{}');paths.append(m)
                files={}
                for key,label in [('expanded source','server.ku'),('generated C','server.c'),('ELF','ku-server'),('declared:protocol','protocol.json')]:
                    f=directory/label;f.write_text('pure fixture');files[key]={'path':str(f)}
                versions[name]={'root':str(directory)};bindings[name]={'files':files,'source_commit':t.FROZEN_BASE}
            with patch.object(t.os,'geteuid',return_value=0),patch.object(t.platform,'system',return_value='Linux'),                patch.object(t.platform,'machine',return_value='aarch64'),patch.object(t.signal,'signal'),patch.object(t.signal,'alarm'),                patch.object(t,'identify',return_value=(t.frozen_plan(versions),bindings,{})),                patch.object(t.h,'normal_perf_preflight',return_value={'complete':False}),patch.object(t,'observe') as observe:
                self.assertEqual(t.main(['--root',str(root/'output'),'--normal-manifest',str(paths[0]),'--typed-manifest',str(paths[1])]),2)
                observe.assert_not_called();self.assertFalse(t.h.load(root/'output/summary.json')['execution_complete'])
                self.assertTrue((root/'output/inputs/BP/server.c').is_file())

    def test_window_failure_restores_harness_and_stops_owned_database(self):
        with tempfile.TemporaryDirectory() as temp:
            root=Path(temp);private=root/'private.json';state=root/'state.json';private.write_text(json.dumps({'db_url':'x','db_cgroup':'x'}));state.write_text('{"limits":{}}')
            fixture=types.SimpleNamespace(start=lambda:None,stop=lambda:{'forced':False,'cgroup_removed':True,'after':{'process_count':0}},manifest_path=private,state_path=state)
            original=t.fc.Harness
            with patch.object(t.h.db,'Database',return_value=fixture),patch.object(t.h,'sha',return_value='a'*64),\
                patch.object(t.fc,'version_identity',return_value={}),patch.object(t.fc,'window',side_effect=RuntimeError('retained failure')) as window:
                result=t.observe(types.SimpleNamespace(root=root,tools=root),t.frozen_plan({}),{'B':{},'BP':{}},{'uid':1,'gid':1})
            self.assertIs(t.fc.Harness,original);self.assertEqual(window.call_count,1)
            self.assertFalse(result['execution_complete']);self.assertFalse(result['attribution_eligible'])
            self.assertEqual(len(result['stopped']),1)

    def test_only_BP_completes_three_windows_or_stops_at_original_drift_limit(self):
        for after in (100,111):
            with self.subTest(after=after),tempfile.TemporaryDirectory() as temp:
                root=Path(temp);private=root/'private.json';state=root/'state.json'
                private.write_text(json.dumps({'db_url':'x','db_cgroup':'x'}));state.write_text('{"limits":{}}')
                fixture=types.SimpleNamespace(start=lambda:None,stop=lambda:{'forced':False,'cgroup_removed':True,'after':{'process_count':0}},manifest_path=private,state_path=state)
                calls=[];original=t.fc.Harness
                def window(plan,name,workload,rate,warm,score,output,**kwargs):
                    calls.append((name,output.name,rate,warm,score))
                    t.h.save(output/'score/normal-self-observations.json',{'record_complete':True,'symbols':{'complete':True}})
                    t.h.save(output/'score/client.json',{'ledger':{'correct_total':12000,'scheduled_to_attempt':{}}})
                    return dict(cpu_us=after if output.name=='after' else 100,full_cpu_us=102)
                with patch.object(t.h.db,'Database',return_value=fixture),patch.object(t.h,'sha',return_value='a'*64),\
                        patch.object(t.fc,'version_identity',return_value={}),patch.object(t.fc,'window',side_effect=window),\
                        patch.object(t.h,'normal_diagnostic_quality',return_value=(True,{})),patch.object(t,'exports',return_value=True):
                    result=t.observe(types.SimpleNamespace(root=root,tools=root),t.frozen_plan({},only='BP'),
                        {'B':{},'BP':{}},{'uid':1,'gid':1})
                self.assertEqual(calls,[('BP',turn,800,3,15) for turn in ('before','sample','after')])
                self.assertEqual(result['execution_complete'],after==100)
                self.assertEqual(result['attribution_controls_passed'],after==100)
                self.assertFalse(result['attribution_eligible']);self.assertIs(t.fc.Harness,original)
                self.assertFalse((root/'normal-self-probe/B').exists())
                self.assertEqual(set(result['controls']),{'BP'})

    def test_ok_runs_only_b_and_preserves_pc_fp_exports_and_cleanup(self):
        import io
        with tempfile.TemporaryDirectory() as temp:
            root=Path(temp);private=root/'private.json';state=root/'state.json'
            private.write_text(json.dumps({'db_url':'x','db_cgroup':'x'}));state.write_text('{"limits":{}}')
            fixture=types.SimpleNamespace(start=lambda:None,stop=lambda:{'forced':False,'cgroup_removed':True,'after':{'process_count':0}},manifest_path=private,state_path=state)
            calls=[];original_open=Path.open;original=t.fc.Harness
            def open_file(path,*args,**kwargs):
                return io.BytesIO(b'bounded kernel fixture') if str(path) in ('/proc/kallsyms','/sys/kernel/notes') else original_open(path,*args,**kwargs)
            def window(plan,name,workload,rate,warm,score,output,**kwargs):
                calls.append((name,output.name,workload,rate,warm,score,kwargs['job']['contract']))
                t.h.save(output/'score/normal-self-observations.json',{'record_complete':True,'symbols':{'complete':True}})
                t.h.save(output/'score/client.json',{'ledger':{'correct_total':30000,'scheduled_to_attempt':{}}})
                return dict(cpu_us=100,full_cpu_us=102)
            with patch.object(t.h.db,'Database',return_value=fixture),patch.object(t.h,'sha',return_value='a'*64),\
                    patch.object(Path,'open',open_file),patch.object(t.fc,'version_identity',return_value={}),\
                    patch.object(t.fc,'window',side_effect=window),patch.object(t.h,'normal_diagnostic_quality',return_value=(True,{})),\
                    patch.object(t,'exports',return_value=True) as exports:
                result=t.observe(types.SimpleNamespace(root=root,tools=root),
                    t.frozen_plan({},current_product=t.h.PG_CURRENT_BASELINE,only='B',workload='ok',callchains=True),
                    {'B':{},'BP':{}},{'uid':1,'gid':1})
            self.assertEqual(calls,[('B',turn,'ok',2000,3,15,'text') for turn in ('before','sample','after')])
            self.assertTrue(result['execution_complete']);self.assertTrue(result['attribution_controls_passed'])
            self.assertFalse(result['candidate_accepted']);self.assertFalse(result['normal_score_eligible'])
            self.assertFalse((root/'normal-self-probe/BP').exists());self.assertIs(t.fc.Harness,original)
            exports.assert_called_once();self.assertEqual(exports.call_args.kwargs,{'callchains':True})

    def test_workflow_ok_choice_only_selects_b_inside_the_cached_typed_self_job(self):
        import subprocess,textwrap
        workflow=(Path(__file__).resolve().parents[3]/'.github/workflows/native-three-os.yml').read_text()
        choice=workflow.split('\n      self_workload:\n',1)[1].split('\n      screen_variants:',1)[0]
        self.assertIn('default: db',choice);self.assertIn('options: [db, ok]',choice)
        prefix,job=workflow.split('\n  pg-typed-self:\n',1);job,suffix=job.split('\n  pg-input-guard:\n',1)
        self.assertIn("if: github.event_name == 'workflow_dispatch' && inputs.scope == 'pg-typed-self'",job)
        self.assertNotIn('KU_PG_SELF_WORKLOAD',prefix+suffix)
        self.assertNotIn('cargo +1.99.1 build',job);self.assertNotIn('typed_pg_self.py --prepare',job)
        setup=next(line.strip() for line in job.splitlines() if 'case "$KU_PG_SELF_WORKLOAD"' in line)
        observed=job.split('- name: Observe only the selected current caller question',1)[1].split('- name: Preserve',1)[0]
        args=textwrap.dedent(observed.split('run: |\n',1)[1].split('          sudo -n',1)[0])
        for workload,expected in (('db',['--only','BP']),('ok',['--workload','ok','--only','B']),('other',None)):
            with self.subTest(workload=workload):
                checked=subprocess.run(['bash','-eu'],input=setup+'\n'+args+'\nprintf "%s\\n" "${pg_self_args[@]}"\n',
                    text=True,capture_output=True,env={'KU_PG_SELF_WORKLOAD':workload},timeout=5)
                if expected is None:self.assertNotEqual(checked.returncode,0)
                else:self.assertEqual(checked.returncode,0,checked.stderr);self.assertEqual(checked.stdout.splitlines(),expected)

if __name__=='__main__':unittest.main()
