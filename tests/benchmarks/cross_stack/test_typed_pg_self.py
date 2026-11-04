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
        self.assertIn('default: db',choice);self.assertIn('options: [db, ok, db-typed, db-mechanism, db-memory-queue, db-memory-stock]',choice)
        prefix,job=workflow.split('\n  pg-typed-self:\n',1);job,suffix=job.split('\n  pg-input-guard:\n',1)
        self.assertIn("if: github.event_name == 'workflow_dispatch' && inputs.scope == 'pg-typed-self'",job)
        self.assertNotIn('KU_PG_SELF_WORKLOAD',prefix+suffix)
        self.assertNotIn('cargo +1.99.1 build',job);self.assertNotIn('typed_pg_self.py --prepare',job)
        setup=next(line.strip() for line in job.splitlines() if 'case "$KU_PG_SELF_WORKLOAD"' in line)
        observed=job.split('- name: Observe only the selected current caller question',1)[1].split('- name: Preserve',1)[0]
        args=textwrap.dedent(observed.split('run: |\n',1)[1].split('          sudo -n',1)[0])
        for workload,expected in (('db',['--only','BP']),('ok',['--workload','ok','--only','B']),('db-mechanism',['--mechanism','--mechanism-recipe','/checkout/tests/benchmarks/cross_stack/pg_owner_mechanism.py']),('other',None)):
            with self.subTest(workload=workload):
                checked=subprocess.run(['bash','-eu'],input=setup+'\n'+args+'\nprintf "%s\\n" "${pg_self_args[@]}"\n',
                    text=True,capture_output=True,env={'KU_PG_SELF_WORKLOAD':workload,'KU_PG_CURRENT_BUILD':'/cached','GITHUB_WORKSPACE':'/checkout'},timeout=5)
                if expected is None:self.assertNotEqual(checked.returncode,0)
                else:self.assertEqual(checked.returncode,0,checked.stderr);self.assertEqual(checked.stdout.splitlines(),expected)

    def test_mechanism_fixed_two_original_rate_windows_without_perf_or_gain(self):
        p=t.mechanism_plan({}, {'B':{'frozen':1},'BP':{'frozen':2}}, {'sha256':'a'*64})
        self.assertEqual(p['order'],[['B','mechanism'],['BP','mechanism']])
        self.assertEqual((p['rate'],p['warmup_seconds'],p['score_seconds'],p['maximum_windows'],p['maximum_replacements']),(200,3,15,2,0))
        self.assertTrue(p['instrumented']);self.assertFalse(p['normal_score_eligible']);self.assertFalse(p['formal_qualified'])
        self.assertNotIn('event',p);self.assertNotIn('perf_frequency',p)
        self.assertEqual(p['original_bindings']['B'],{'frozen':1})
        self.assertEqual(p['current_product'],t.h.PG_CURRENT_BASELINE)
        candidate='c'*40
        q=t.mechanism_plan({}, {}, {'sha256':'b'*64},product=candidate)
        self.assertEqual(q['current_product'],candidate)
        self.assertEqual(q['maximum_windows'],2);self.assertFalse(q['normal_score_eligible'])
        with self.assertRaises(ValueError):t.mechanism_plan({}, {}, {},product='short')

    def test_mechanism_records_are_bounded_and_no_missing_output_passes(self):
        with tempfile.TemporaryDirectory() as temp:
            p=Path(temp)/'service.log'
            p.write_text('ordinary log\nKU_MECHANISM {"kind":"aggregate","first":3}\n')
            result=t.mechanism_records(p)
            self.assertEqual(result['records'],[{'kind':'aggregate','first':3}])
            self.assertFalse(result['normal_score_eligible'])
            p.write_text('no mechanism output\n')
            with self.assertRaisesRegex(ValueError,'absent'):t.mechanism_records(p)
            p.write_text('KU_MECHANISM []\n')
            with self.assertRaisesRegex(ValueError,'object'):t.mechanism_records(p)
            p.write_text('KU_MECHANISM {}\n'*257)
            with self.assertRaisesRegex(ValueError,'bound'):t.mechanism_records(p)

    def test_mechanism_serial_windows_and_cleanup_keep_warm_score_distinct(self):
        for fail in (False,True):
            with self.subTest(fail=fail),tempfile.TemporaryDirectory() as temp:
                root=Path(temp);recipe=root/'recipe.py';recipe.write_text('def transform(source):return source')
                private=root/'private.json';state=root/'state.json';private.write_text('{"db_url":"x","db_cgroup":"x"}');state.write_text('{"limits":{}}')
                fixture=types.SimpleNamespace(start=lambda:None,stop=lambda:{'forced':False,'cgroup_removed':True,'after':{'process_count':0}},manifest_path=private,state_path=state)
                original=t.fc.Harness;calls=[];old={'B':{'old':'B'},'BP':{'old':'BP'}};derived={'B':{'new':'B'},'BP':{'new':'BP'}}
                plan=t.mechanism_plan({'B':{'root':'derivedB'},'BP':{'root':'derivedBP'}},old,{'sha256':'a'*64});plan['original_versions']={'B':{'root':'oldB'},'BP':{'root':'oldBP'}}
                def identity(p,n):return old[n] if p['versions']==plan['original_versions'] else derived[n]
                def window(p,n,w,rate,warm,score,out,**kw):
                    calls.append((n,w,rate,warm,score,kw['job']['contract']))
                    if fail:raise RuntimeError('retained original failure')
                    (out/'service').mkdir(parents=True);(out/'service/service.log').write_text('KU_MECHANISM {"kind":"aggregate"}\n')
                    t.h.save(out/'score/client.json',{'ledger':{'correct_total':3000,'scheduled_to_attempt':{}}})
                    return {'all_correct':True,'functional_valid':True,'natural_reclamation':True}
                with patch.object(t.h.db,'Database',return_value=fixture),patch.object(t.h,'sha',return_value='a'*64),patch.object(t.fc,'version_identity',side_effect=identity),patch.object(t.fc,'window',side_effect=window),patch.object(t.h,'normal_diagnostic_quality',return_value=(True,{})),patch.object(t.h,'normal_self_client') as perf:
                    result=t.observe_mechanism(types.SimpleNamespace(root=root,mechanism_recipe=recipe),plan,derived,{'uid':1,'gid':1})
                self.assertIs(t.fc.Harness,original);perf.assert_not_called()
                self.assertEqual(calls,[('B','db',200,3,15,'pg-strict')]+([] if fail else [('BP','db',200,3,15,'pg-strict')]))
                self.assertEqual(result['execution_complete'],not fail);self.assertFalse(result['cpu_gain_claimed'])
                self.assertFalse(result['normal_score_eligible']);self.assertFalse(result['candidate_accepted'])
                self.assertTrue(result['database_cleanup']['cgroup_removed'])
                if not fail:self.assertEqual(result['rows'][1]['correct_response_denominator'],3000)

    def test_mechanism_derivation_changes_only_private_native_artifacts(self):
        import compile_variants as cv
        import reuse_build as rb
        with tempfile.TemporaryDirectory() as temp:
            root=Path(temp);output=root/'out';output.mkdir();recipe=root/'recipe.py'
            recipe.write_text('def transform(source):return source+"\\n/*mechanism*/\\n"\n')
            common={}
            for name in ('compiler','libpq','cli','driver','protocol'):
                f=root/name;f.write_text(name);common[name]=f
            versions={};manifests={}
            for name,mode in (('B','int'),('BP','typed')):
                arm=root/name;arm.mkdir();source=arm/'server.ku';source.write_text('frozen business '+name)
                generated=arm/'server.c';generated.write_text('int frozen_'+name+';')
                binary=arm/'server';binary.write_bytes(b'original ELF '+name.encode())
                invocation={'compiler':{'path':str(common['compiler']),'sha256':t.h.sha(common['compiler'])},
                    'flags':['-std=c11','-O2','-pthread'],'library_inputs':[{'role':'libpq','selected_input':{'real_path':str(common['libpq'])}}]}
                build={'cli':str(common['cli']),'cli_sha256':t.h.sha(common['cli']),
                    'libpq_source':str(common['libpq']),'libpq_sha256':t.h.sha(common['libpq']),
                    'formatting_mode':'helpers','pg_read_mode':mode,
                    'native':{'server':{'binary_sha256':t.h.sha(binary),'generated_c':str(generated),'generated_c_sha256':t.h.sha(generated),
                        'source':str(source),'source_sha256':t.h.sha(source),'compiler_invocation':invocation}}}
                m={'candidate_base':t.h.PG_CURRENT_BASELINE,'stacks':{'ku':{'command':[str(binary)],'build':build}},
                    'client':{'command':[str(common['driver'])],'sha256':t.h.sha(common['driver'])},'protocol':str(common['protocol'])}
                t.h.save(arm/'manifest.json',m);manifests[name]=m
                versions[name]={'root':str(arm),'source_commit':t.h.PG_CURRENT_BASELINE,'binary_sha256':t.h.sha(binary)}
            plan={'versions':versions};bindings={n:t.fc.version_identity(plan,n)for n in versions}
            expected={n:{key:value['sha256']for key,value in bindings[n]['files'].items()}for n in versions}
            original={p:t.h.sha(p)for p in root.rglob('*')if p.is_file()};commands=[]
            class FakeCommands:
                def __init__(self,directory,environment):directory.mkdir();self.rows=[]
                def run(self,label,command,cwd,timeout):
                    commands.append(command);self.rows.append({'label':label,'exit_code':0})
                    if label=='mechanism-link':Path(command[-1]).write_bytes(b'\x7fELF\x02\x01'+b'\0'*12+(183).to_bytes(2,'little'))
                    return 'clang fake' if label=='compiler-version' else str(common['libpq']) if label=='linked-libraries' else ''
            with patch.object(rb,'CURRENT_PG_FILES',expected['B']),patch.object(rb,'CURRENT_TYPED_PG_FILES',expected['BP']),patch.object(cv,'Commands',FakeCommands):
                derived,new=t.prepare_mechanism(types.SimpleNamespace(root=output,tools=root,mechanism_recipe=recipe),plan,bindings)
            self.assertEqual(len(commands),6);self.assertEqual(derived['rate'],200)
            self.assertEqual({p:t.h.sha(p)for p in original},original)
            for name in versions:
                m=t.h.load(output/'mechanism-derived'/name/'manifest.json');build=m['stacks']['ku']['build']
                self.assertEqual(build['cli_sha256'],bindings[name]['files']['Ku CLI']['sha256'])
                self.assertEqual(build['native']['server']['source_sha256'],bindings[name]['files']['expanded source']['sha256'])
                self.assertNotEqual(new[name]['files']['ELF']['sha256'],bindings[name]['files']['ELF']['sha256'])
                self.assertTrue(build['instrumented']);self.assertFalse(m['normal_score_eligible'])
            # The alternate path must explicitly bind both real source commits
            # and every normal file; it cannot override the cached default.
            candidate='c'*40
            for name in versions:
                manifests[name]['candidate_base']=candidate
                t.h.save(root/name/'manifest.json',manifests[name])
                versions[name]['source_commit']=candidate
            bindings={n:t.fc.version_identity(plan,n)for n in versions}
            expected={n:{key:value['sha256']for key,value in bindings[n]['files'].items()}for n in versions}
            with patch.object(cv,'Commands',FakeCommands):
                args=types.SimpleNamespace(root=root/'candidate-out',tools=root,mechanism_recipe=recipe)
                args.root.mkdir()
                with self.assertRaisesRegex(ValueError,'explicit distinct full candidate'):
                    t.prepare_mechanism(args,plan,bindings,product='short',expected_files=expected)
                with self.assertRaisesRegex(ValueError,'explicit candidate'):
                    t.prepare_mechanism(args,plan,bindings,product=candidate)
                with self.assertRaisesRegex(ValueError,'explicit candidate product'):
                    t.prepare_mechanism(args,plan,bindings,expected_files=expected)
                with self.assertRaisesRegex(ValueError,'source product'):
                    t.prepare_mechanism(args,plan,bindings)
                mixed=copy.deepcopy(bindings);mixed['BP']['source_commit']=t.h.PG_CURRENT_BASELINE
                with self.assertRaisesRegex(ValueError,'source product'):
                    t.prepare_mechanism(args,plan,mixed,product=candidate,expected_files=expected)
                mixed_plan=copy.deepcopy(plan);mixed_plan['versions']['B']['source_commit']=t.h.PG_CURRENT_BASELINE
                with self.assertRaisesRegex(ValueError,'source product'):
                    t.prepare_mechanism(args,mixed_plan,bindings,product=candidate,expected_files=expected)
                incomplete=copy.deepcopy(expected);incomplete['B'].pop('driver ELF')
                with self.assertRaisesRegex(ValueError,'exact declared'):
                    t.prepare_mechanism(args,plan,bindings,product=candidate,expected_files=incomplete)
                self.assertEqual(len(commands),6)
                derived,new=t.prepare_mechanism(args,plan,bindings,product=candidate,expected_files=expected)
            self.assertEqual(len(commands),12);self.assertEqual(derived['current_product'],candidate)
            self.assertEqual({n:new[n]['source_commit'] for n in new},{'B':candidate,'BP':candidate})
            self.assertTrue(all(v['source_commit']==candidate for v in derived['versions'].values()))
            self.assertTrue(derived['instrumented']);self.assertFalse(derived['normal_score_eligible'])
            self.assertEqual(t.h.load(args.root/'mechanism-build.json')['product'],candidate)

    def test_mechanism_workflow_skips_unrelated_setup_and_large_archive(self):
        workflow=(Path(__file__).resolve().parents[3]/'.github/workflows/native-three-os.yml').read_text()
        job=workflow.split('\n  pg-typed-self:\n',1)[1].split('\n  pg-input-guard:\n',1)[0]
        self.assertIn("- if: inputs.self_workload != 'db-mechanism' && inputs.self_workload != 'db-memory-queue' && inputs.self_workload != 'db-memory-stock'\n        uses: actions/setup-go",job)
        self.assertIn("if: inputs.self_workload != 'db-mechanism' && inputs.self_workload != 'db-memory-queue' && inputs.self_workload != 'db-memory-stock'\n        timeout-minutes: 1",job)
        self.assertIn('pg_self_sampling=()',job)
        self.assertIn("if: always() && inputs.self_workload != 'db-mechanism'",job)
        self.assertIn('mechanism-probe/**/resources.json',job);self.assertIn('mechanism-derived/**/*.c',job)
        self.assertNotIn('mechanism-derived/**/bin',job);self.assertNotIn('cargo +1.99.1 build',job)
        self.assertIn("if: inputs.self_workload != 'db-memory-queue' && inputs.self_workload != 'db-memory-stock'",job)
        self.assertIn('stock_workqueue_probe.py',job);self.assertIn('drain-observer/*.txt',job)

    def test_direct_stock_mode_reuses_cache_and_installs_only_its_small_layout_tool(self):
        import hashlib,subprocess,textwrap
        checkout=Path(__file__).resolve().parents[3]
        workflow=(checkout/'.github/workflows/native-three-os.yml').read_text()
        job=workflow.split('\n  pg-typed-self:\n',1)[1].split('\n  pg-input-guard:\n',1)[0]
        setup=job.split('- name: Reuse the existing compiler, paths and software sampler setup',1)[1]
        setup=textwrap.dedent(setup.split('run: |\n',1)[1].split('          mkdir -p',1)[0])
        stubs='sudo() { printf "%s\\n" "$*"; }; rustup() { printf "rustup %s\\n" "$*"; }; apt-cache() { return 1; };\n'
        for workload in ('db-memory-stock','db-memory-queue','db-mechanism','db','ok'):
            with self.subTest(workload=workload):
                result=subprocess.run(['bash','-eu'],input=stubs+setup,text=True,capture_output=True,
                    env={'KU_PG_SELF_WORKLOAD':workload},timeout=5)
                self.assertEqual(result.returncode,0,result.stderr)
                self.assertEqual('install -y --no-install-recommends pahole' in result.stdout,workload=='db-memory-stock')
                if workload in ('db-memory-stock','db-memory-queue','db-mechanism'):
                    self.assertNotIn('rustup ',result.stdout);self.assertNotIn('linux-tools',result.stdout)
                    self.assertNotIn('llvm-18',result.stdout)
        self.assertIn('key: ku-pg-baseline-d05-rust1.99.1-',job)
        self.assertIn('reuse_build.prepare_current_typed_pg_cache(',job)
        source=checkout/'tests/benchmarks/cross_stack/direct_stock_probe.py'
        self.assertEqual(hashlib.sha256(source.read_bytes()).hexdigest(),
            '9ccc1c410dc6c7c02e99510d70fa6448857af1fc9c3543fad31b047de9955ba8')

    def test_direct_stock_definition_failure_keeps_exact_command_error_and_traceback(self):
        import direct_stock_probe as stock
        with tempfile.TemporaryDirectory() as temp:
            root=Path(temp);output=root/'output';output.mkdir();(root/'error_log').write_text('trace_kprobe: caret and exact parse reason\n')
            probe=stock.DirectStock(types.SimpleNamespace(),output);probe.base=root;probe.instance=None
            command='p:owned/ce memory_current_read css=$arg1:x64'
            with patch.object(stock.os,'open',side_effect=OSError(22,'fixture EINVAL')),self.assertRaises(OSError):
                probe._append_definition(command)
            failure=json.loads((output/'probe-definition-failure.json').read_text())['failures'][0]
            self.assertEqual(failure['definition'],command);self.assertEqual(failure['errno'],22)
            self.assertIn('fixture EINVAL',failure['traceback'])
            self.assertEqual(failure['error_log_before'],failure['error_log_after'])
            self.assertIn('caret and exact parse reason',failure['error_log_after'][str(root/'error_log')]['text'])
            self.assertTrue((output/'probe-definition-failure.txt').is_file())

    def test_direct_stock_single_write_supports_interface_which_rejects_append_seek(self):
        import direct_stock_probe as stock
        with tempfile.TemporaryDirectory() as temp:
            root=Path(temp);output=root/'output';output.mkdir()
            probe=stock.DirectStock(types.SimpleNamespace(),output);probe.base=root;probe.instance=None
            command='p:owned/ce memory_current_read css=$arg1:x64';payload=(command+'\n').encode('ascii')
            original_open=Path.open
            def opened(path,*args,**kwargs):
                if path==root/'kprobe_events':raise OSError(22,'fixture FileIO append seek rejected')
                return original_open(path,*args,**kwargs)
            with patch.object(Path,'open',opened),patch.object(stock.os,'open',return_value=17) as opened_fd,\
                    patch.object(stock.os,'write',return_value=len(payload)) as written,\
                    patch.object(stock.os,'lseek',side_effect=OSError(22,'fixture seek rejected')) as seek,\
                    patch.object(stock.os,'close') as close:
                probe._append_definition(command)
            flags=opened_fd.call_args.args[1]
            self.assertTrue(flags&stock.os.O_WRONLY);self.assertTrue(flags&stock.os.O_APPEND)
            self.assertFalse(flags&(stock.os.O_CREAT|stock.os.O_TRUNC))
            written.assert_called_once_with(17,payload);seek.assert_not_called();close.assert_called_once_with(17)
            self.assertTrue(probe.info['last_definition_attempt']['descriptor_closed'])

    def test_direct_stock_short_write_records_stops_closes_and_never_retries_tail(self):
        import direct_stock_probe as stock
        with tempfile.TemporaryDirectory() as temp:
            root=Path(temp);output=root/'output';output.mkdir()
            probe=stock.DirectStock(types.SimpleNamespace(),output);probe.base=root;probe.instance=None
            command='p:owned/ce memory_current_read css=$arg1:x64';payload=(command+'\n').encode('ascii')
            with patch.object(stock.os,'open',return_value=18),patch.object(stock.os,'write',return_value=3) as written,\
                    patch.object(stock.os,'close') as close,self.assertRaises(OSError):probe._append_definition(command)
            written.assert_called_once_with(18,payload);close.assert_called_once_with(18)
            failure=json.loads((output/'probe-definition-failure.json').read_text())['failures'][0]
            self.assertTrue(failure['short_write']);self.assertEqual(failure['bytes_written'],3)
            self.assertEqual(failure['bytes_expected'],len(payload));self.assertTrue(failure['descriptor_closed'])

    def test_direct_stock_fallback_reuses_arg_fields_and_does_not_fabricate_miss_profile(self):
        import direct_stock_probe as stock,io
        with tempfile.TemporaryDirectory() as temp:
            probe=stock.DirectStock(types.SimpleNamespace(),Path(temp));commands=[];writes=[]
            symbols=''.join('ffff000000000010 t '+name+'\n' for name in probe.SYMBOLS);original_open=Path.open
            def opened(path,*args,**kwargs):
                return io.StringIO(symbols) if str(path)=='/proc/kallsyms' else original_open(path,*args,**kwargs)
            def append(command):
                commands.append(command)
                if command.startswith('p:'):raise OSError(22,'fixture kprobe rejection')
            fields='css value stock index n0 n6 c0 c6 memcg pages'
            with patch.object(probe,'_btf_layout',return_value=({'offset':1},{'offset':8},{'offset':0})),\
                    patch.object(stock.platform,'machine',return_value='aarch64'),patch.object(stock.os,'cpu_count',return_value=4),\
                    patch.object(Path,'open',opened),patch.object(Path,'is_file',return_value=True),patch.object(Path,'mkdir'),\
                    patch.object(Path,'write_text',side_effect=lambda text: writes.append(text)),patch.object(Path,'read_text',return_value=fields),\
                    patch.object(probe,'_append_definition',side_effect=append):
                probe._prepare()
            self.assertEqual(len(commands),5);self.assertTrue(commands[0].startswith('p:'))
            self.assertTrue(all(command.startswith(('f:','f64:')) for command in commands[1:]))
            self.assertEqual(sum('%return ' in command for command in commands),2)
            self.assertTrue(all('$arg1' in command for command in commands))
            self.assertNotIn('%x0','\n'.join(commands));self.assertEqual(probe.info['backend'],'fprobe')
            self.assertTrue(probe.info['miss_counts_unknown']);self.assertEqual(probe.info['profile_before'],{})

    def test_direct_stock_partial_creation_retains_owned_name_for_cleanup_without_fallback(self):
        import direct_stock_probe as stock,io
        with tempfile.TemporaryDirectory() as temp:
            probe=stock.DirectStock(types.SimpleNamespace(),Path(temp));commands=[]
            symbols=''.join('ffff000000000010 t '+name+'\n' for name in probe.SYMBOLS);original_open=Path.open
            def opened(path,*args,**kwargs):
                return io.StringIO(symbols) if str(path)=='/proc/kallsyms' else original_open(path,*args,**kwargs)
            def partial(command):
                commands.append(command);probe.info['last_definition_attempt']={'short_write':True}
                raise OSError(5,'fixture partial write')
            with patch.object(probe,'_btf_layout',return_value=({'offset':1},{'offset':8},{'offset':0})),\
                    patch.object(stock.platform,'machine',return_value='aarch64'),patch.object(stock.os,'cpu_count',return_value=4),\
                    patch.object(Path,'open',opened),patch.object(Path,'is_file',return_value=True),patch.object(Path,'mkdir'),\
                    patch.object(Path,'write_text'),patch.object(Path,'exists',return_value=False),patch.object(Path,'is_dir',return_value=True),\
                    patch.object(probe,'_append_definition',side_effect=partial),self.assertRaisesRegex(RuntimeError,'partial definition'):
                probe._prepare()
            self.assertEqual(len(commands),1);self.assertEqual(probe.definitions,[probe.names['ce']])
            self.assertEqual(probe.info['backend'],'kprobe');self.assertIn('ce',probe.paths)

    def test_direct_stock_fprobe_pair_records_never_claim_complete_with_unknown_misses(self):
        import direct_stock_probe as stock
        with tempfile.TemporaryDirectory() as temp:
            domain=types.SimpleNamespace(__init__=object(),remove=object())
            probe=stock.DirectStock(types.SimpleNamespace(Domain=domain),Path(temp))
            probe.old_init=domain.__init__;probe.old_remove=domain.remove;probe.target={'memcg':'0x1'}
            probe.info.update(backend='fprobe',association={'target_pairs_complete':True},
                per_CPU_stats={'cpu0':'overrun: 0\ncommit overrun: 0\ndropped events: 0\n'})
            with patch.object(probe,'_cleanup'),patch.object(stock.time,'sleep'):
                probe.__exit__(None,None,None)
            info=json.loads((Path(temp)/'direct-stock-finish.json').read_text())['info']
            self.assertFalse(info['observation_complete']);self.assertTrue(info['positive_pair_only'])
            self.assertEqual(info['observation_failures'],['fprobe_miss_counts_unknown'])

    def test_direct_stock_four_profiles_accept_complete_target_drain_pairs_only(self):
        import direct_stock_probe as stock
        with tempfile.TemporaryDirectory() as temp:
            domain=types.SimpleNamespace(__init__=object(),remove=object())
            probe=stock.DirectStock(types.SimpleNamespace(Domain=domain),Path(temp))
            self.assertEqual(set(probe.names),{'ce','cr','de','dr'})
            raw=('worker-10 [002] d.... 1.000001: '+probe.names['de']+': stock=0x10 index=1 n1=65 c1=0x99\n'
                 'worker-10 [002] d.... 1.000002: '+probe.names['dr']+': stock=0x10 index=1 n1=0\n'
                 'other-11 [003] d.... 1.000003: '+probe.names['dr']+': stock=0x20 index=0 n0=0\n').encode()
            association=stock.trace_readback(raw,probe.names)
            self.assertTrue(association['target_pairs_complete']);self.assertEqual(len(association['pairs']),1)
            self.assertEqual(association['unknown_target_drain_returns'],1)
            pair=association['pairs'][0];self.assertEqual((pair['pages_before'],pair['pages_after']),(65,0))
            self.assertEqual(pair['cached_before'],0x99);self.assertTrue(pair['same_CPU'])
            probe.old_init=domain.__init__;probe.old_remove=domain.remove;probe.target={'memcg':'0x99'}
            probe.info.update(backend='kprobe',association=association,
                profile_after={key:{'hits':1,'missed':0}for key in probe.names},
                per_CPU_stats={'cpu0':'overrun: 0\ncommit overrun: 0\ndropped events: 0\n'})
            with patch.object(probe,'_cleanup'),patch.object(stock.time,'sleep'):probe.__exit__(None,None,None)
            info=json.loads((Path(temp)/'direct-stock-finish.json').read_text())['info']
            self.assertTrue(info['observation_complete']);self.assertFalse(info['positive_pair_only'])
            self.assertIn('deliberately not probed',info['refill_boundary'])

    def test_direct_stock_wrapper_retains_window_but_does_not_promote_unknown_misses(self):
        import direct_stock_probe as stock,memory_bracket_probe as bracket
        with tempfile.TemporaryDirectory() as temp:
            root=Path(temp);original=bracket.MemoryBracketProbe
            def observed(argv):
                stock.save(root/'summary.json',{'execution_complete':True,'window_qualified':True,'window':{'kept':True}})
                stock.save(root/'observer/direct-stock/direct-stock-finish.json',{'info':{
                    'observation_complete':False,'positive_pair_only':True,'miss_counts_unknown':True}})
                return 0
            with patch.object(bracket,'main',side_effect=observed):code=stock.main(['--root',str(root)])
            result=json.loads((root/'summary.json').read_text())
            self.assertEqual(code,2);self.assertFalse(result['execution_complete']);self.assertFalse(result['observation_complete'])
            self.assertTrue(result['window_qualified']);self.assertEqual(result['window'],{'kept':True})
            self.assertTrue(result['positive_pair_only']);self.assertIs(bracket.MemoryBracketProbe,original)

    def test_direct_stock_workflow_runs_once_and_preserves_nested_raw_and_lifecycle(self):
        workflow=(Path(__file__).resolve().parents[3]/'.github/workflows/native-three-os.yml').read_text()
        job=workflow.split('\n  pg-typed-self:\n',1)[1].split('\n  pg-input-guard:\n',1)[0]
        direct=job.split('- name: Observe one baseline target stock boundary once',1)[1].split('- name:',1)[0]
        self.assertIn("if: inputs.self_workload == 'db-memory-stock'",direct)
        self.assertEqual(direct.count('direct_stock_probe.py'),1)
        self.assertIn('--root "$RUNNER_TEMP/ku-normal-memory-observation"',direct)
        self.assertIn('--normal-manifest "$KU_PG_CURRENT_BUILD/B/manifest.json"',direct)
        self.assertIn('--typed-manifest "$KU_PG_CURRENT_BUILD/BP/manifest.json"',direct)
        self.assertNotIn('continue-on-error',direct);self.assertNotIn('typed_pg_self.py',direct)
        for name in ('Validate the exact fp sampling format on this boot',
                     'Observe only the selected current caller question',
                     'Preserve the existing build and observation archive formats'):
            selected=job.split('- name: '+name,1)[1].split('shell: bash',1)[0]
            self.assertIn("inputs.self_workload != 'db-memory-stock'",selected)
        for pattern in ('observer/direct-stock/*.json','observer/direct-stock/*.txt',
                        'observer/*.json','observer/*.txt','normal-memory-window/window.json',
                        'normal-memory-window/**/client.json','normal-memory-window/**/resources.json',
                        'normal-memory-window/**/service.json','normal-memory-window/**/full-lifecycle.json',
                        'normal-memory-window/**/termination.json'):
            self.assertIn('${{ runner.temp }}/ku-normal-memory-observation/'+pattern,job)

if __name__=='__main__':unittest.main()
