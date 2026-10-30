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
    def test_screen_freezes_each_candidate_without_mixing_direct_or_diagnostic(self):
        names=['A','B','R','RS','D',*[f'C{i}' for i in range(1,7)]]
        versions={n:{'source_commit':str(i)*40,'root':'/unused/'+n} for i,n in enumerate(names)}
        plan=h.followup_plan(versions,'f'*40)
        self.assertEqual(plan['prepatch'],'f'*40)
        self.assertEqual(plan['candidate_versions'],['B','C1','C2','C3','C4','C5','C6'])
        self.assertEqual(plan['maximum_windows'],111)
        for order in plan['paired_orders']:
            self.assertEqual(set(order),{'A','B','C1','C2','C3','C4','C5','C6','reference'})
            self.assertEqual(len(order),len(set(order)))
            self.assertNotIn('D',order)
        self.assertNotIn('capacity',plan)
        self.assertNotIn('stability',plan)
        self.assertFalse(plan['formal_qualified'])
        for case in plan['cases']:
            self.assertIn(case['workload'],plan['latency_slo_ms'])
            self.assertIn(case['reference'],versions)
        for name in plan['paired_orders'][0]:
            self.assertEqual(len({order.index(name) for order in plan['paired_orders']}),3)
        versions['C7']={'root':'/unused/C7','source_commit':'7'*40}
        versions['BF']={'root':'/unused/BF','source_commit':'b'*40}
        versions['HC']={'root':'/unused/HC','diagnostic_only':True}
        full=h.followup_plan(versions,'f'*40)
        self.assertEqual(full['maximum_windows'],142)
        self.assertIn('BF',full['candidate_versions'])
        self.assertNotIn('HC',full['candidate_versions'])
        self.assertTrue(full['transport_control']['diagnostic_only'])
        self.assertFalse(full['transport_control']['normal_score_eligible'])

    def test_combination_without_old_ablations_still_checks_unicode(self):
        versions={n:{'source_commit':'b'*40,'root':'/unused/'+n} for n in ['A','B','BF','R','RS','D','HC']}
        plan=h.followup_plan(versions,'f'*40)
        case=next(c for c in plan['cases'] if c['workload']=='json_escape')
        self.assertEqual(case['paired_orders'],plan['paired_orders'])
        self.assertNotIn('C1',str(case))
        self.assertEqual(len(h.diagnostic_turns(plan,case,True)),12)
        self.assertEqual(plan['maximum_windows'],76)

    def test_task_json_is_separate_from_worker_goals_and_does_not_repeat_pg_or_ablations(self):
        names=['A','B','BT','BF','R','RS','D','HC','C1','C2']
        versions={n:{'source_commit':'b'*40,'root':'/unused/'+n} for n in names}
        plan=h.followup_plan(versions,'f'*40,task_json_screen=True)
        self.assertEqual(plan['candidate_versions'],['B','BT'])
        self.assertTrue(plan['task_json_screen'])
        self.assertEqual([c['workload'] for c in plan['cases']],['json_object','json_escape','cpu'])
        for case in plan['cases'][:2]:
            for order in case.get('paired_orders',plan['paired_orders']):
                self.assertEqual(set(order),{'A','B','BT','reference'})
                self.assertEqual(len(order),4)
        cpu=plan['cases'][2]
        self.assertEqual({name for _,name in h.diagnostic_turns(plan,cpu,True)},{'A','B','R'})
        self.assertEqual(plan['maximum_windows'],3+13+12+9+12+1)
        self.assertEqual(plan['admission']['name'],'B')
        self.assertFalse(plan['task_json_comparison']['original_worker_score_eligible'])
        self.assertIn('BT admission is not verified',plan['task_json_comparison']['admission_scope'])
        self.assertEqual(plan['worker_growth']['names'],['B','BF'])
        self.assertNotIn('targets',plan)
        ordinary=h.followup_plan(versions,'f'*40)
        self.assertFalse(ordinary['task_json_screen'])
        self.assertNotIn('BT',ordinary['candidate_versions'])
        self.assertIn('db',[c['workload'] for c in ordinary['cases']])
        with self.assertRaisesRegex(ValueError,'identified BT build'):
            h.followup_plan({n:v for n,v in versions.items() if n!='BT'},'f'*40,task_json_screen=True)

    def test_task_json_screen_uses_current_worker_denominator_and_all_quality_checks(self):
        rows=[]
        for group in range(1,4):
            for name,cpu in [('A',20),('B',10),('BT',8),('R',9)]:
                rows.append(dict(turn=f'group-{group}-{name}',name=name,full_cpu_us=cpu,
                    latency={'p99_ms':1},memory={'steady_median_bytes':100},comparison_contract='same',
                    functional_valid=True,accounting_valid=True,all_correct=True,natural_reclamation=True,
                    arrival_schedule_quality='pass',environment_quality='pass',measurement_invalid=False,
                    observed_environment_faults=[],diagnostic_only=name=='BT'))
        case={'reference':'R'}
        result=h.task_json_screen(rows,case,100)
        self.assertEqual((result['numerator_name'],result['denominator_name']),('BT','B'))
        self.assertEqual(result['valid_pair_count'],3)
        self.assertAlmostEqual(result['cpu_paired_bootstrap95']['estimate'],.8)
        self.assertFalse(result['original_worker_score_eligible'])
        self.assertFalse(result['formal_qualified'])
        self.assertAlmostEqual(h.screen_pairs(rows,case,100)['cpu_paired_bootstrap95']['estimate'],.5)
        self.assertIsNone(h.task_json_screen([r for r in rows if r['name']!='BT'],case,100))
        plan={'json_layer_diagnosis':{'execution_complete':True}}
        gate={'passed':True,'execution_complete':True}
        control=rows[0]
        for issue,value in [('environment_quality','fail'),('measurement_invalid',True),
                            ('arrival_schedule_quality','fail'),('natural_reclamation',False)]:
            broken=[dict(row) for row in rows]
            for row in broken:
                if row['name']=='BT':row[issue]=value
            with self.subTest(issue=issue):
                self.assertEqual(h.task_json_screen(broken,case,100)['valid_pair_count'],0)
                self.assertFalse(h.diagnostic_axes(plan,broken,len(broken),[],control,[],True,gate)['normal_windows_qualified'])

    def test_build_derives_task_json_only_from_the_frozen_product_snapshot(self):
        from unittest.mock import Mock
        for supported,requested in ((False,None),(True,None),(False,False),(True,False),(False,True),(True,True)):
            with self.subTest(supported=supported,requested=requested),tempfile.TemporaryDirectory() as directory,ExitStack() as patches:
                root=Path(directory);tools=root/'tools';tools.mkdir()
                source=root/'checkout';source.mkdir()
                # Deliberately disagree with the frozen B snapshot.
                ir=source/'src/ir/task.rs';ir.parent.mkdir(parents=True)
                ir.write_text('enum TaskOp {\n    JsonDecode {\n}\n' if not supported else 'old tools source')
                calls=[]
                def snapshot(checkout,commit,destination):
                    ir=destination/'src/ir/task.rs';ir.parent.mkdir(parents=True)
                    ir.write_text('enum TaskOp {\n    JsonDecode {\n}\n' if supported else 'old product')
                class Builder:
                    def __init__(self,root,source,**mode):
                        self.root=root;self.build=root/'build';self.build.mkdir();self.mode=mode
                    def ku(self):
                        calls.append(('ku',self.root.name))
                        cli=self.build/'ku';cli.write_text('inert CLI identity')
                        h.save(self.root/'manifest.json',{'stacks':{'ku':{'build':{'cli':str(cli)}}}})
                    def rust(self):calls.append(('rust',self.root.name))
                    def go_build(self,client=False):calls.append(('client' if client else 'go',self.root.name))
                    def ku_native(self,cli):calls.append(('native',self.root.name,cli,self.mode.get('ku_json_execution')))
                    def finish(self,failed=None):return {'execution_complete':failed is None}
                patches.enter_context(patch.object(h.os,'geteuid',return_value=1))
                patches.enter_context(patch.object(h.bl,'Builder',Builder))
                patches.enter_context(patch.object(h,'snapshot',side_effect=snapshot))
                complete=Mock(return_value={'execution_complete':True})
                patches.enter_context(patch.object(h,'complete_build',complete))
                args=types.SimpleNamespace(root=root,source=source,tools=tools,baseline='f'*40,
                    candidate='b'*40,prepatch='f'*40,build_set='followup-screen',screen_variants=[])
                if requested is not None:args.task_json_screen=requested
                if requested and not supported:
                    with self.assertRaisesRegex(ValueError,'frozen B product'):
                        h.build(args,{'steps':{}})
                    complete.assert_not_called()
                else:
                    self.assertTrue(h.build(args,{'steps':{}})['execution_complete'])
                    complete.assert_called_once()
                native=[c for c in calls if c[0]=='native']
                self.assertEqual(native,[('native','BT',root/'B/build/ku','task')] if requested and supported else [])
                self.assertEqual([c[:2] for c in calls[:4]],[('ku','A'),('ku','B'),('rust','R'),('client','R')])

    def test_task_json_selection_is_part_of_identity_and_cannot_select_other_phases(self):
        import io
        from contextlib import redirect_stderr,redirect_stdout
        args=types.SimpleNamespace(baseline=h.BASELINE,candidate='b'*40,prepatch='f'*40,screen_variants=[])
        absent=h.hosted_identity(args,'followup-screen')
        self.assertIs(absent['task_json_screen'],False)
        args.task_json_screen=False
        self.assertEqual(absent,h.hosted_identity(args,'followup-screen'))
        args.task_json_screen=True
        self.assertNotEqual(absent,h.hosted_identity(args,'followup-screen'))
        self.assertIs(h.hosted_identity(args,'followup-screen')['task_json_screen'],True)
        for build_set in ('full','diagnostic'):
            error=io.StringIO()
            with self.subTest(build_set=build_set),redirect_stderr(error),self.assertRaises(SystemExit) as exit:
                h.main(['--phase','build','--root','/unused-root','--source','/unused-source',
                    '--tools','/unused-tools','--candidate','b'*40,'--build-set',build_set,'--task-json-screen'])
            self.assertEqual(exit.exception.code,2)
            self.assertIn('--task-json-screen requires followup-screen',error.getvalue())
        with tempfile.TemporaryDirectory() as directory,ExitStack() as patches:
            root=Path(directory);source=root/'source';source.mkdir();tools=root/'tools';tools.mkdir()
            output=root/'output';error=io.StringIO()
            patches.enter_context(patch.object(h.platform,'system',return_value='Linux'))
            patches.enter_context(patch.object(h.platform,'machine',return_value='aarch64'))
            patches.enter_context(patch.object(h.signal,'signal'));patches.enter_context(patch.object(h.signal,'alarm'))
            build=patches.enter_context(patch.object(h,'build',return_value={'execution_complete':True}))
            argv=['--phase','build','--root',str(output),'--source',str(source),'--tools',str(tools),
                '--candidate','b'*40,'--prepatch','f'*40,'--build-set','followup-screen']
            with redirect_stdout(io.StringIO()):self.assertEqual(h.main(argv),0)
            with redirect_stderr(error),self.assertRaises(SystemExit) as exit:
                h.main([*argv,'--task-json-screen'])
            self.assertEqual(exit.exception.code,2);self.assertIn('identity changed',error.getvalue())
            build.assert_called_once()
            self.assertIs(h.load(output/'hosted-state.json')['identity']['task_json_screen'],False)

    def test_task_json_workflow_switch_is_explicit_and_identical_for_build_reuse_and_run(self):
        workflow=(Path(__file__).resolve().parents[3]/'.github/workflows/native-three-os.yml').read_text()
        entry=workflow.split('\n      task_json_screen:\n',1)[1].split('\n      reuse_build_run:',1)[0]
        self.assertIn('type: boolean',entry);self.assertIn('default: false',entry)
        job=workflow.split('\n  performance-v2:\n',1)[1].split('\n  full-tests:\n',1)[0]
        self.assertIn("KU_PERFORMANCE_V2_TASK_JSON_SCREEN: ${{ inputs.task_json_screen && 'true' || 'false' }}",job)
        build=job.split('- name: Build frozen',1)[1].split('- name: Run the one',1)[0]
        run=job.split('- name: Run the one',1)[1].split('- name: Always archive',1)[0]
        for phase,count in ((build,2),(run,1)):
            self.assertIn('performance_task_json_args=()',phase)
            self.assertIn('if [ "$KU_PERFORMANCE_V2_TASK_JSON_SCREEN" = true ]; then',phase)
            self.assertIn('test "$performance_build_set" = followup-screen',phase)
            self.assertIn('performance_task_json_args+=(--task-json-screen)',phase)
            self.assertEqual(phase.count('"${performance_task_json_args[@]}"'),count)

    def test_screen_uses_selected_candidate_normalized_cpu_and_its_own_tails(self):
        rows=[]
        for group in range(1,4):
            for name,cpu,p99 in [('A',10,1),('B',30,4),('C1',8,2),('R',9,1)]:
                rows.append(dict(turn=f'group-{group}-{name}',name=name,full_cpu_us=cpu,
                    latency={'p99_ms':p99},memory={'steady_median_bytes':100},comparison_contract='same',
                    functional_valid=True,accounting_valid=True,all_correct=True,natural_reclamation=True,
                    arrival_schedule_quality='pass',environment_quality='pass',observed_environment_faults=[]))
        result=h.screen_pairs(rows,{'reference':'R'},20,'C1')
        self.assertEqual(result['numerator_name'],'C1')
        self.assertEqual(result['valid_pair_count'],3)
        self.assertEqual([p['p99_B_ms'] for p in result['pairs']],[2]*3)
        self.assertAlmostEqual(result['cpu_paired_bootstrap95']['estimate'],.8)
        self.assertFalse(result['adopted'])
        self.assertFalse(result['formal_qualified'])

    def test_followup_keeps_valid_normal_rows_but_requires_json_and_control_axes(self):
        good=dict(functional_valid=True,accounting_valid=True,all_correct=True,natural_reclamation=True,
            arrival_schedule_quality='pass',environment_quality='pass',observed_environment_faults=[])
        plan={'json_layer_diagnosis':{'execution_complete':True}}
        admission={'passed':True,'execution_complete':True}
        result=h.diagnostic_axes(plan,[good],1,[],good,[],True,admission)
        self.assertTrue(result['execution_complete'])
        plan['json_layer_diagnosis']['execution_complete']=False
        result=h.diagnostic_axes(plan,[good],1,[],good,[],True,admission)
        self.assertTrue(result['normal_windows_complete'])
        self.assertTrue(result['normal_windows_qualified'])
        self.assertFalse(result['json_layers_complete'])
        self.assertFalse(result['execution_complete'])
        plan['json_layer_diagnosis']['execution_complete']=True
        bad=dict(good,all_correct=False)
        result=h.diagnostic_axes(plan,[good],1,[],bad,[],True,admission)
        self.assertFalse(result['transport_control_complete'])
        self.assertFalse(result['execution_complete'])
        bad=dict(good,arrival_schedule_quality='fail')
        self.assertFalse(h.diagnostic_axes(plan,[bad],1,[],good,[],True,admission)['normal_windows_qualified'])
        missing=h.diagnostic_axes(plan,[good],1,[],good,[],True)
        self.assertTrue(missing['normal_windows_complete']);self.assertFalse(missing['admission_complete'])
        self.assertFalse(missing['execution_complete'])

    def test_admission_gate_requires_actual_gate_identity_and_natural_retirement(self):
        from unittest.mock import Mock
        for issue in (None,'gate','forced','identity','driver_exception','interrupt'):
            with self.subTest(issue=issue),tempfile.TemporaryDirectory() as d,ExitStack() as patches:
                root=Path(d);plan={'versions':{'B':{'root':str(root),'stack':'ku'}},'resources':h.RESOURCES}
                binding={'source_commit':'a'*40}
                identities=[binding,{'source_commit':'b'*40} if issue=='identity' else binding]
                patches.enter_context(patch.object(fc,'version_identity',side_effect=identities))
                service=Mock();service.stop.return_value={'natural':issue!='forced'}
                harness=Mock();harness.start.return_value=service;harness.close.return_value=[]
                harness.admission_pressure.return_value={'passed':issue!='gate'}
                if issue in ('driver_exception','interrupt'):
                    harness.admission_pressure.side_effect=KeyboardInterrupt() if issue=='interrupt' else RuntimeError('retained driver failure')
                patches.enter_context(patch.object(fc,'Harness',return_value=harness))
                patches.enter_context(patch.object(fc,'service_termination_succeeded',side_effect=lambda stack,r:r.get('natural') is True))
                if issue=='interrupt':
                    with self.assertRaises(KeyboardInterrupt):h.admission_window(plan,root/'gate')
                else:
                    result=h.admission_window(plan,root/'gate')
                    self.assertEqual(result['passed'],issue is None)
                    self.assertFalse(result['normal_score_eligible'])
                service.stop.assert_called_once();harness.close.assert_called_once()
                retained=h.load(root/'gate/admission-window.json')
                self.assertEqual(retained['passed'],issue is None)
                if issue in ('driver_exception','interrupt'):self.assertIn('error',retained)

    def test_json_attachment_programming_error_is_incomplete_without_swallowing_interrupt(self):
        import json_dense
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory);compiler=root/'compiler';compiler.write_bytes(b'compiler fixture')
            cli=root/'cli';cli.write_bytes(b'CLI fixture')
            normal={'generated_c':str(root/'normal.c'),'compiler_invocation':{
                'compiler':{'path':str(compiler),'sha256':h.sha(compiler)},'library_inputs':[]}}
            manifest={'candidate_base':'a'*40,'stacks':{'ku':{'build':{
                'cli':str(cli),'native':{'server':normal}}}}}
            for name in ('A','B'):h.save(root/name/'manifest.json',manifest)
            with patch.object(json_dense,'main',side_effect=TypeError('missing environment')):
                result=h.build_json_layers(types.SimpleNamespace(root=root))
            self.assertFalse(result['execution_complete']);self.assertIn('TypeError',result['error'])
            with patch.object(json_dense,'main',side_effect=KeyboardInterrupt):
                with self.assertRaises(KeyboardInterrupt):h.build_json_layers(types.SimpleNamespace(root=root))

    def plan(self):
        return h.frozen_plan({n:{'root':'/unused/'+n,'stack':'rust' if n.startswith('R') else 'ku'}
            for n in ['A0','B','R','BJ','BP','BD','RS','RJ','RP','RPS']})

    def test_admission_reuse_requires_same_boot_build_and_resources_and_never_retries_failure(self):
        from unittest.mock import Mock
        binding={'source_commit':'a'*40,'files':{'ELF':{'sha256':'b'*64}}}
        for issue in (None,'boot','binding','resources','failed','interrupted'):
            with self.subTest(issue=issue),tempfile.TemporaryDirectory() as directory:
                root=Path(directory);plan=self.plan();state={'steps':{}}
                gate=Mock(return_value={'passed':issue!='failed','execution_complete':True,'binding':binding})
                with patch.object(h,'admission_window',gate),patch.object(fc,'version_identity',return_value=binding):
                    first=h.required_admission(plan,root,state,'boot',root/'admission')
                    self.assertEqual(gate.call_count,1)
                    if issue=='boot':boot='other-boot'
                    else:boot='boot'
                    if issue=='binding':state['steps']['admission']['result']['binding']={'source_commit':'c'*40}
                    if issue=='resources':state['steps']['admission']['result']['resources']={'service_cores':1}
                    if issue=='interrupted':state['steps']['admission']={'state':'failed','error':'KeyboardInterrupt'}
                    second=h.required_admission(plan,root,state,boot,root/'admission')
                    self.assertEqual(gate.call_count,1)
                    self.assertEqual(second['passed'],issue is None)
                    if issue is None:self.assertTrue(second['reused_same_host_build'])
                    else:self.assertEqual(second['goal_status'],'blocked');self.assertTrue(second['no_retry'])

    def test_diagnostic_and_screen_admission_are_required_but_not_timed_windows(self):
        from unittest.mock import Mock
        for followup,gate_passed,task in ((False,True,False),(False,False,False),(True,True,False),(True,False,False),(True,True,True)):
            with self.subTest(followup=followup,gate_passed=gate_passed,task=task),tempfile.TemporaryDirectory() as directory,ExitStack() as patches:
                root=Path(directory)
                versions={name:{'root':str(root/name),'source_commit':'a'*40}
                    for name in ['A','B','R','RS','D','HC',*(['BT'] if task else [])]}
                plan=h.followup_plan(versions,task_json_screen=task) if followup else h.diagnostic_plan(versions)
                plan['json_layer_diagnosis']={'execution_complete':True}
                h.save(root/'frozen-plan.json',plan)
                manifest=root/'fixture/manifest.json';dbstate=root/'fixture/state.json'
                h.save(manifest,{'db_url':'private-fixture','db_cgroup':'/unused','database':{}})
                h.save(dbstate,{'limits':{'cpuset.cpus':'3'}})
                cleanup=[];fixture=types.SimpleNamespace(start=lambda:None,manifest_path=manifest,state_path=dbstate,
                    stop=lambda:cleanup.append('database') or {'stopped':True})
                state={'uid':os.getuid(),'gid':os.getgid(),'steps':{}}
                gate=Mock(return_value={'passed':gate_passed,'execution_complete':True,'binding':{'source_commit':'a'*40}})
                good=dict(status='observed',functional_valid=True,accounting_valid=True,all_correct=True,
                    natural_reclamation=True,arrival_schedule_quality='pass',environment_quality='pass',
                    measurement_invalid=False,observed_environment_faults=[],full_cpu_us=10,latency={'p99_ms':1},
                    memory={'steady_median_bytes':100},comparison_contract='same')
                window=Mock(side_effect=lambda *args,**kwargs:dict(good))
                patches.enter_context(patch.object(h.os,'geteuid',return_value=0))
                patches.enter_context(patch.object(h.os,'sched_getaffinity',return_value={0,1,2,3},create=True))
                patches.enter_context(patch.object(h,'sha',return_value='boot'))
                patches.enter_context(patch.object(h.db,'Database',return_value=fixture))
                patches.enter_context(patch.object(h,'diagnostic_command',return_value={}))
                patches.enter_context(patch.object(h,'perf_capability',return_value={'available':False}))
                patches.enter_context(patch.object(h.shutil,'which',return_value=None))
                patches.enter_context(patch.object(h,'environment_record',return_value={'quality':'pass'}))
                patches.enter_context(patch.object(h,'admission_window',gate))
                patches.enter_context(patch.object(h,'window_quality',return_value=(False,None)))
                patches.enter_context(patch.object(fc,'window',window))
                result=h.diagnostic(types.SimpleNamespace(root=root,tools=root/'tools'),state,followup=followup)
                self.assertEqual(gate.call_count,1);self.assertEqual(result['execution_complete'],gate_passed)
                controls=plan['environment_controls']['count'] if followup else 0
                expected=plan['maximum_windows'] if followup else sum(len(h.diagnostic_turns(plan,c,False)) for c in plan['cases'])
                # Supplementary gate failure must preserve already valid normal windows.
                self.assertEqual(window.call_count,expected)
                self.assertEqual(result['scope_window_count'],expected)
                self.assertEqual(cleanup,['database']);self.assertEqual(result['admission']['passed'],gate_passed)
                if not gate_passed:self.assertEqual(result['goal_status'],'blocked')
                if task:
                    comparison=h.load(root/'diagnostic/json_object/comparison.json')
                    self.assertEqual(comparison['task_json_screen']['denominator_name'],'B')
                    self.assertEqual(comparison['candidate_screens']['BT']['denominator_name'],'B')
                    self.assertEqual(comparison['paired_screen']['denominator_name'],'A')
                    task_rows=[r for r in result['rows'] if r['name']=='BT']
                    self.assertEqual(len(task_rows),6)
                    self.assertTrue(all(r['diagnostic_only'] and not r['normal_score_eligible']
                        and not r['original_worker_score_eligible'] for r in task_rows))
                    self.assertEqual({r['workload'] for r in task_rows},{'json_object','json_escape'})

    def test_five_registered_groups_and_pg_sharing_are_actual_windows(self):
        plan=self.plan();jobs=fc.jobs_for(plan,'final');self.assertEqual(len(jobs),80)
        self.assertEqual(len(plan['targets']),16)
        self.assertTrue(plan['admission']['required']);self.assertFalse(plan['admission']['counts_as_timed_window'])
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
        admission={'passed':True,'execution_complete':True}
        result=h.acceptance_result(final,cap,stable,'met',admission)
        self.assertTrue(result['functional_acceptance']);self.assertEqual(result['goal_status'],'unknown')
        result=h.acceptance_result(final,cap,{},'met',admission)
        self.assertFalse(result['functional_acceptance']);self.assertFalse(result['execution_complete'])
        cap['capacity'][0]['functional_valid']=False
        self.assertEqual(h.acceptance_result(final,cap,stable,'met',admission)['goal_status'],'not_met')
        self.assertEqual(h.acceptance_result(final,cap,stable,'met')['goal_status'],'blocked')

    def test_coarse_search_cannot_exceed_six_points_before_refinement(self):
        cap=self.plan()['capacity']
        for kind,start in cap['start_rates'].items():
            self.assertEqual(cap['maximum_rates'][kind],start*cap['factor']**5)
        strict=[x for x in self.plan()['targets'] if x.get('display_contract')=='pg-strict']
        self.assertEqual({x['denominator'] for x in strict},{'RS'})
        self.assertEqual({x['maximum_ratio'] for x in strict if x['metric']=='full_cpu_us'},{.90})

    def test_archive_preserves_frozen_input_bytes_and_keeps_receipt_redaction(self):
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory)
            corpus=root/'B/src/tests/benchmarks/cross_stack/corpora/object.json'
            corpus.parent.mkdir(parents=True)
            raw=b'{ "counter":7, "data": {"token":"synthetic fixture"} }\n'
            corpus.write_bytes(raw)
            protocol=root/'B/src/tests/benchmarks/cross_stack/protocol.json'
            protocol.write_bytes(b'{ "schema": 1 }\n')
            h.save(root/'private.json',{'db_url':'postgres://private-fixture','public':'retained'})
            driver=root/'fresh-driver/bin/cross-stack-client';driver.parent.mkdir(parents=True)
            driver.write_bytes(b'raw driver fixture')
            driver_source=root/'fresh-driver/source/main.go';driver_source.parent.mkdir(parents=True)
            driver_source.write_bytes(b'package main\n')
            result=h.full_archive(types.SimpleNamespace(root=root),{'steps':{}})
            self.assertTrue(result['complete'])
            with tarfile.open(result['path']) as archive:
                self.assertEqual(archive.extractfile(corpus.relative_to(root).as_posix()).read(),raw)
                self.assertEqual(archive.extractfile(protocol.relative_to(root).as_posix()).read(),protocol.read_bytes())
                self.assertEqual(json.load(archive.extractfile('private.json')),{'public':'retained'})
                self.assertEqual(archive.extractfile('fresh-driver/bin/cross-stack-client').read(),driver.read_bytes())
                self.assertEqual(archive.extractfile('fresh-driver/source/main.go').read(),driver_source.read_bytes())
                mapping={row['path']:row for row in json.load(archive.extractfile('portable-file-map.json'))['files']}
                row=mapping[corpus.relative_to(root).as_posix()]
                self.assertEqual(row['original_sha256'],row['archived_sha256'])

    def test_oversize_archive_returns_explicit_failure_not_silent_truncation(self):
        with tempfile.TemporaryDirectory() as d:
            root=Path(d);path=root/'normal.c';path.write_bytes(bytes(range(256))*100)
            with patch.object(h,'ARCHIVE_LIMIT',100):result=h.archive(types.SimpleNamespace(root=root),{'steps':{}})
            self.assertFalse(result['complete']);self.assertEqual(result['goal_status'],'blocked')
            self.assertTrue(path.exists())
            with tarfile.open(result['path']) as tar:self.assertIn('archive-failure.json',tar.getnames())


    def test_run_dispatches_capacity_only_once_after_fixed_windows_and_always_cleans_db(self):
        cases=[(False,False,False,'pass',None),(False,True,False,'pass',None),
               (True,False,False,'pass',None),(True,False,True,'pass',None),
               (False,False,False,'fail',None),(False,False,False,'unknown',None),
               (True,False,True,'unknown',None)]
        cases += [(False,False,False,'pass',issue) for issue in
                  [('execution_complete',False),('arrival_schedule_quality','fail'),
                   ('environment_quality','unknown'),('measurement_invalid',True),('admission',False)]]
        for tail_only,fail_final,stability_only,quality,final_issue in cases:
            with self.subTest(tail_only=tail_only,fail_final=fail_final,stability_only=stability_only,
                             quality=quality,final_issue=final_issue),tempfile.TemporaryDirectory() as d,ExitStack() as patches:
                root=Path(d);frozen=root/'frozen-plan.json';h.save(frozen,self.plan());original_hash=h.sha(frozen)
                manifest=root/'fixture/manifest.json';dbstate=root/'fixture/state.json'
                h.save(manifest,{'db_url':'fixture','db_cgroup':'/unused','database':{}})
                h.save(dbstate,{'limits':{'cpuset.cpus':'3'}})
                cleaned=[];owned=types.SimpleNamespace(start=lambda:None,manifest_path=manifest,state_path=dbstate,
                    stop=lambda:cleaned.append('stop') or {'stopped':True})
                state={'uid':os.getuid(),'gid':os.getgid(),'steps':{}}
                read_bytes,read_text=Path.read_bytes,Path.read_text;old_harness=fc.Harness;calls=[]
                patches.enter_context(patch.object(Path,'read_bytes',lambda p:b'fixture-boot' if str(p)=='/proc/sys/kernel/random/boot_id' else read_bytes(p)))
                patches.enter_context(patch.object(Path,'read_text',lambda p,*a,**k:'MemTotal: 16777216 kB' if str(p)=='/proc/meminfo' else read_text(p,*a,**k)))
                patches.enter_context(patch.object(h.os,'geteuid',return_value=0))
                patches.enter_context(patch.object(h.os,'sched_getaffinity',return_value={0,1,2,3},create=True))
                patches.enter_context(patch.object(h.db,'Database',return_value=owned))
                patches.enter_context(patch.object(h,'environment_record',return_value={'quality':quality}))
                gate_issue=final_issue==('admission',False)
                patches.enter_context(patch.object(h,'admission_window',return_value={'passed':not gate_issue,'execution_complete':True,
                    'goal_status':'met','binding':{'source_commit':'a'*40}}))
                def measured(plan_path,output,phase,**unused):
                    config=h.load(plan_path);label=Path(output).name;calls.append((label,phase))
                    if label in ('control','final'):
                        self.assertEqual(phase,'all');self.assertNotIn('capacity',config);self.assertNotIn('stability',config)
                        if label=='control':self.assertEqual(config['stop_after_consecutive_unusable_windows'],2)
                        if label=='final' and fail_final:raise ValueError('fixed-window failure retained')
                        row=dict(status='observed',functional_valid=True,accounting_valid=True,
                            arrival_schedule_quality='pass',environment_quality='unknown' if label=='control' else 'pass',
                            measurement_invalid=False,natural_reclamation=True)
                        result={'execution_complete':True,'goal_status':'not_met' if label=='final' else 'unknown','rows':[row]}
                        if label=='final' and final_issue:
                            key,value=final_issue
                            (result if key=='execution_complete' else row)[key]=value
                        return result
                    if label.startswith('capacity-'):
                        kind=label.removeprefix('capacity-');self.assertEqual(phase,'capacity')
                        self.assertEqual(config['capacity']['workloads'],[kind])
                        return {'execution_complete':True,'capacity':[dict(name=n,workload=kind,functional_valid=True,
                            accounting_valid=True,all_recovery_passed=True,qualified_lower_rate=100,upper_rate=None,boundary_observed=False) for n in ['B','R']]}
                    self.assertEqual((label,phase),('stability','stability'))
                    return {'execution_complete':True,'formal_qualified':False,'rows':[{'natural_reclamation':True,
                        'segments':[{'streams':{'mixed':{'assessment':{'functional_valid':True,'accounting_valid':True}}}}]}]}
                patches.enter_context(patch.object(fc,'execute',side_effect=measured))
                args=types.SimpleNamespace(root=root)
                if fail_final:
                    with self.assertRaisesRegex(ValueError,'fixed-window failure'):h.run(args,state)
                    self.assertEqual(calls,[('control','all'),('final','all')])
                elif quality!='pass' or final_issue:
                    result=h.run(args,state,tail_only=tail_only,stability_only=stability_only)
                    self.assertFalse(result['execution_complete']);self.assertEqual(result['goal_status'],'blocked')
                    self.assertEqual(calls,[('control','all')]+([('final','all')] if quality=='pass' and not gate_issue else []))
                    self.assertNotIn('capacity',state['steps']);self.assertNotIn('stability',state['steps'])
                    self.assertFalse((root/'results/stability').exists())
                    if final_issue:
                        self.assertEqual(set(result['axes']),{'control','admission'} if gate_issue else {'control','admission','final'})
                        self.assertEqual(set(state['deferred']),{'final','capacity','stability'} if gate_issue else {'capacity','stability'})
                    else:
                        self.assertNotIn('final',state['steps']);self.assertFalse((root/'final-plan.json').exists())
                else:
                    result=h.run(args,state,tail_only=tail_only,stability_only=stability_only);self.assertTrue(result['execution_complete'])
                    expected=[('control','all')]+([] if tail_only else [('final','all')])+([] if stability_only else [('capacity-json_object','capacity'),('capacity-db','capacity')])+[('stability','stability')]
                    self.assertEqual(calls,expected)
                    if not tail_only:self.assertEqual(state['steps']['final']['result']['goal_status'],'not_met')
                    if tail_only:
                        self.assertFalse(result['fixed_matrix_executed']);self.assertFalse(result['cross_host_pooling'])
                        self.assertEqual(result['goal_status'],'unknown');self.assertNotIn('final',state['steps'])
                        self.assertFalse((root/'final-plan.json').exists())
                        self.assertEqual(result['capacity_executed'],not stability_only)
                        self.assertEqual(set(result['axes']),{'control','admission','stability'} if stability_only else {'control','admission','capacity','stability'})
                self.assertEqual(cleaned,['stop']);self.assertIs(fc.Harness,old_harness)
                self.assertEqual(h.sha(frozen),original_hash)
                self.assertIn('capacity',h.load(frozen));self.assertIn('stability',h.load(frozen))

    def test_incomplete_run_returns_nonzero_without_treating_goal_failure_as_incomplete(self):
        for complete in (False,True):
            with self.subTest(complete=complete),tempfile.TemporaryDirectory() as d,ExitStack() as patches:
                root=Path(d);source=root/'source';tools=root/'tools'
                source.mkdir();tools.mkdir()
                patches.enter_context(patch.object(h.platform,'system',return_value='Linux'))
                patches.enter_context(patch.object(h.platform,'machine',return_value='aarch64'))
                patches.enter_context(patch.object(h.signal,'signal'));patches.enter_context(patch.object(h.signal,'alarm'))
                patches.enter_context(patch.object(h,'run',return_value={'execution_complete':complete,'goal_status':'not_met'}))
                patches.enter_context(patch('builtins.print'))
                code=h.main(['--phase','run','--root',str(root/'output'),'--source',str(source),
                             '--tools',str(tools),'--candidate','1'*40])
                self.assertEqual(code,0 if complete else 2)


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
                    self.assertTrue(all(c['observational_quality_passed'] is False for c in report['comparisons']))
                self.assertIs(fc.Harness,old_fc);self.assertIs(pt.Harness,old_pt)
                self.assertEqual(state['compiler_database_cleanup'],{'stopped':True})

    def test_compiler_archive_keeps_profile_and_elf_but_excludes_target_cache(self):
        root=Path('/unused')
        self.assertTrue(h.evidence_file(root/'compiler/RPGO/target/release/ku-cross-stack-rust',root))
        self.assertFalse(h.evidence_file(root/'compiler/RPGO/target/release/.fingerprint/dependency.json',root))
        self.assertTrue(h.evidence_file(root/'compiler/http-train-rust/merged.profdata',root))


if __name__=='__main__':unittest.main()
