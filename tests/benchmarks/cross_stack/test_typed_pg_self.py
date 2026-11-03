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

if __name__=='__main__':unittest.main()
