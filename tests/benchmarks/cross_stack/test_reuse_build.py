"""Portable cache identity/path tests; no executable, compiler or service runs."""
import hashlib
import io
import json
from pathlib import Path
import tarfile
import tempfile
import subprocess
import unittest
import types
from contextlib import ExitStack
from unittest.mock import patch
import reuse_build as r


def archive(path, changes=None, old_run=False):
    files={'A/build/bin/ku-server':b'normal immutable artifact',
           'hosted-state.json':json.dumps({'steps':{'build-A':{'state':'completed'},
               **({'run':{'state':'completed'}} if old_run else {})}}).encode()}
    index={'files':[{'path':n,'bytes':len(b),'original_sha256':hashlib.sha256(b).hexdigest(),
                    'archived_sha256':hashlib.sha256(b).hexdigest()} for n,b in files.items()],
           'path_root':r.TOKEN,'raw_and_redacted_hashes_distinct':True}
    files['portable-file-map.json']=json.dumps(index).encode()
    with tarfile.open(path,'w:gz') as tar:
        for name,raw in files.items():
            info=tarfile.TarInfo(name);info.size=len(raw);info.mode=0o755
            tar.addfile(info,io.BytesIO(raw))
        if changes is not None:
            info=changes;tar.addfile(info,io.BytesIO(b'x') if info.isfile() else None)
    return r.h.sha(path)


class ReuseBuildTests(unittest.TestCase):

    def test_current_cache_without_hosted_state_imports_only_fixed_B_and_driver(self):
        for rust in ('1.99.1','1.89.0'):
            with self.subTest(rust=rust),tempfile.TemporaryDirectory() as temp:
                root=Path(temp);path=root/'current.tgz'
                prepared=dict(product=r.h.PG_CURRENT_BASELINE,rust_toolchain=rust,new_builds=True,
                    imported_measurement_windows=0,bindings={'B':{'files':{'ELF':{'sha256':r.CURRENT_PG_ELF}}}})
                files={'prepared-current.json':json.dumps(prepared).encode(),'B/build/bin/server':b'B',
                    'Driver/build/bin/client':b'Driver','BP/build/bin/server':b'not imported',
                    'normal-self-probe/B/sample/score/perf.data':b'not imported'}
                files['portable-file-map.json']=json.dumps(dict(path_root=r.TOKEN,raw_and_redacted_hashes_distinct=True,
                    files=[dict(path=k,bytes=len(v),original_sha256=hashlib.sha256(v).hexdigest(),
                        archived_sha256=hashlib.sha256(v).hexdigest()) for k,v in files.items()])).encode()
                with tarfile.open(path,'w:gz') as tar:
                    for name,raw in files.items():
                        info=tarfile.TarInfo(name);info.size=len(raw);tar.addfile(info,io.BytesIO(raw))
                digest=r.h.sha(path)
                with patch.object(r,'CURRENT_PG_ARCHIVE_SHA',digest):
                    if rust!='1.99.1':
                        with self.assertRaisesRegex(ValueError,'Rust1.99.1'):r.extract_builds(path,root/'out',digest,{'B','Driver'},current_pg_cache=True)
                        continue
                    r.extract_builds(path,root/'out',digest,{'B','Driver'},current_pg_cache=True)
                self.assertTrue((root/'out/B/build/bin/server').is_file())
                self.assertFalse((root/'out/BP').exists());self.assertFalse((root/'out/normal-self-probe').exists())
                self.assertFalse((root/'out/hosted-state.json').exists())
                provenance=r.h.load(root/'out/reuse-original-state.json')
                self.assertFalse(provenance['measurement_results_imported']);self.assertFalse(provenance['hosted_state_imported'])

    def test_current_guard_aliases_verified_B_as_A_and_requires_fresh_candidate(self):
        with tempfile.TemporaryDirectory() as temp,ExitStack() as patches:
            root=Path(temp)/'fresh';cache=root/'verified-current-cache'
            a=types.SimpleNamespace(root=root,source=Path(temp)/'source',baseline=r.h.PG_CURRENT_BASELINE,
                prepatch=r.h.PG_CURRENT_BASELINE,candidate='c'*40,screen_variants=[],pg_input_guard=True)
            binding=dict(source_commit=a.baseline,manifest_sha256='original',source_inventory_sha256='inventory',
                build_mode={'formatting_mode':'helpers'},files={k:{'sha256':v} for k,v in r.CURRENT_PG_FILES.items()})
            client=dict(sha256=r.CURRENT_PG_FILES['driver ELF'],source_sha256=hashlib.sha256(b'go').hexdigest())
            normal=dict(client=client,protocol='/verified/protocol',stacks={'ku':{'build':{'native':{'server':{'generated_c':'verified-C'}}}}})
            def extract(*args,**kwargs):
                r.h.save(cache/'prepared-current.json',{'bindings':{'B':binding}})
                return {'B/manifest.json':{'original_sha256':'original'}}
            patches.enter_context(patch.object(r.os,'geteuid',return_value=1001))
            imported=patches.enter_context(patch.object(r,'extract_builds',side_effect=extract))
            patches.enter_context(patch.object(r,'restore_json_inputs'));patches.enter_context(patch.object(r,'validate_protocol_inputs'))
            driver=patches.enter_context(patch.object(r,'verify_driver_cache',return_value={'client':client}))
            verify=patches.enter_context(patch.object(r,'verify_manifest',return_value=normal))
            patches.enter_context(patch.object(r.h.fc,'version_identity',return_value=binding))
            patches.enter_context(patch.object(r,'checked'))
            patches.enter_context(patch.object(r.subprocess,'run',return_value=types.SimpleNamespace(returncode=0)))
            patches.enter_context(patch.object(r.subprocess,'check_output',return_value=b'go'))
            state=r.prepare_pg_input_guard_cache(Path(temp)/'archive',a,source_run='41630973206')
            self.assertEqual(set(state['steps']),{'build-A','build-Driver'})
            self.assertEqual(state['identity'],r.h.hosted_identity(a,'diagnostic'))
            self.assertEqual(state['imported_measurement_windows'],0)
            self.assertFalse((root/'B').exists());self.assertTrue((root/'A/manifest.json').is_file())
            self.assertEqual(state['steps']['build-A']['original_build_name'],'B')
            self.assertTrue(r.h.load(root/'verified-baseline-cache.json')['new_candidate_build_still_required'])
            imported.assert_called_once_with(Path(temp)/'archive',cache,r.CURRENT_PG_ARCHIVE_SHA,{'B','Driver'},current_pg_cache=True)
            driver.assert_called_once_with(cache,expected_commit=a.baseline)
            verify.assert_called_once_with(cache,'B',a.baseline,{'B/manifest.json':{'original_sha256':'original'}},
                client_source_name='Driver',require_current_rust=True)
            with self.assertRaisesRegex(ValueError,'fresh guard root'):r.prepare_current_pg_guard_cache(Path(temp)/'archive',a)

    def test_current_manifest_binds_redacted_cli_receipt_and_rejects_historical_rust(self):
        for rust in ('1.99.1','1.89.0'):
            with self.subTest(rust=rust),tempfile.TemporaryDirectory() as temp:
                root=Path(temp);b=root/'B';b.mkdir();mapping={}
                header=bytearray(20);header[:6]=b'\x7fELF\x02\x01';header[18:20]=(183).to_bytes(2,'little')
                binary=b/'server';binary.write_bytes(header)
                source=b/'program';source.write_text('frozen program')
                pq=b/'build/libpq/libpq.so';pq.parent.mkdir(parents=True);pq.write_bytes(b'pq')
                go=root/'Driver/src/tests/benchmarks/cross_stack/client/main.go';go.parent.mkdir(parents=True);go.write_bytes(b'go')
                build_receipt=b/'build.json';r.h.save(build_receipt,dict(candidate_base=r.h.PG_CURRENT_BASELINE,failure=None,commands=[{'exit_code':0}]))
                native_receipt=b/'native.json';r.h.save(native_receipt,{'cwd':r.TOKEN+'/B'})
                cli_receipt=b/'cli.json';versions={'rustc':'rustc '+rust,'cargo':'cargo '+rust}
                r.h.save(cli_receipt,dict(rust_toolchain=rust,failure=None,exact_versions=versions,
                    commands=[dict(label='ku-cli',command=['cargo','+'+rust,'build','--locked','--release'],exit_code=0)],cwd=r.TOKEN+'/B'))
                for path,original in ((native_receipt,'a'*64),(cli_receipt,'b'*64)):
                    mapping[path.relative_to(root).as_posix()]=dict(original_sha256=original,archived_sha256=r.h.sha(path))
                native=dict(binary=str(binary),binary_sha256=r.h.sha(binary),source=str(source),source_sha256=r.h.sha(source),
                    generated_c=str(source),generated_c_sha256=r.h.sha(source),compiler_receipt=str(native_receipt),
                    compiler_receipt_sha256='a'*64,compiler_invocation={'flags':['-std=c11','-O2','-pthread'],'library_inputs':[]})
                build=dict(formatting_mode='helpers',json_response_mode='text',pg_execution_mode='async',pg_read_mode='int',
                    cli=str(binary),cli_sha256=r.h.sha(binary),libpq_sha256=r.h.sha(pq),native={'server':native},
                    rust_toolchain=rust,cli_build_receipt=str(cli_receipt),cli_build_receipt_sha256='b'*64)
                r.h.save(b/'manifest.json',dict(candidate_base=r.h.PG_CURRENT_BASELINE,build_receipt=str(build_receipt),
                    rust_toolchain=rust,versions=versions,stacks={'ku':{'command':[str(binary)],'build':build}},
                    client={'command':[str(binary)],'sha256':r.h.sha(binary),'source_sha256':r.h.sha(go)}))
                if rust!='1.99.1':
                    with self.assertRaisesRegex(ValueError,'historical Rust'):
                        r.verify_manifest(root,'B',r.h.PG_CURRENT_BASELINE,mapping,False,client_source_name='Driver',require_current_rust=True)
                else:
                    m=r.verify_manifest(root,'B',r.h.PG_CURRENT_BASELINE,mapping,False,client_source_name='Driver',require_current_rust=True)
                    receipt=m['stacks']['ku']['build']
                    self.assertEqual(receipt['original_cli_build_receipt_sha256'],'b'*64)
                    self.assertEqual(receipt['cli_build_receipt_sha256'],r.h.sha(cli_receipt))
                    r.h.bl.require_rust_manifest(m)

    def test_fixed_large_cache_extracts_three_builds_and_four_logs_without_score_state(self):
        with tempfile.TemporaryDirectory() as temp:
            root=Path(temp);path=root/'large.tgz'
            files={name+'/build/bin/server':name.encode() for name in ('A','B','Driver')}
            files.update({name:b'contract bytes' for name in r.PG_CONTRACT_LOGS})
            files['pg-input-guard-observation/score.json']=b'not imported'
            files['hosted-state.json']=json.dumps(dict(identity=dict(baseline=r.TYPED_GUARD_PRODUCT,
                prepatch=r.TYPED_GUARD_PRODUCT,candidate=r.LARGE_PG_PRODUCT,pg_input_guard=True),
                steps={**{k:{'state':'completed'} for k in ('build','build-A','build-B','build-Driver')},
                    'diagnostic':{'state':'failed'}})).encode()
            index=dict(path_root=r.TOKEN,raw_and_redacted_hashes_distinct=True,files=[dict(path=k,bytes=len(v),
                original_sha256=hashlib.sha256(v).hexdigest(),archived_sha256=hashlib.sha256(v).hexdigest()) for k,v in files.items()])
            files['portable-file-map.json']=json.dumps(index).encode()
            with tarfile.open(path,'w:gz') as tar:
                for name,data in files.items():
                    info=tarfile.TarInfo(name);info.size=len(data);tar.addfile(info,io.BytesIO(data))
            digest=r.h.sha(path)
            with patch.object(r,'LARGE_PG_ARCHIVE_SHA',digest):
                with self.assertRaisesRegex(ValueError,'fixed large-input'):
                    r.extract_builds(path,root/'wrong',digest,{'A','Driver'},pg_large_input_cache=True)
                r.extract_builds(path,root/'good',digest,{'A','B','Driver'},pg_large_input_cache=True)
            self.assertTrue((root/'good/B/build/bin/server').is_file())
            self.assertTrue(all((root/'good'/name).is_file() for name in r.PG_CONTRACT_LOGS))
            self.assertFalse((root/'good/pg-input-guard-observation').exists())
            self.assertFalse((root/'good/hosted-state.json').exists())
            self.assertNotIn('diagnostic',r.h.load(root/'good/reuse-original-state.json')['steps'])

    def test_large_contracts_require_four_exact_passes_and_bound_runtime_source(self):
        with tempfile.TemporaryDirectory() as temp:
            root=Path(temp);raws={name:('fixed '+name).encode() for name in r.LARGE_PG_SOURCES}
            hashes={name:hashlib.sha256(raw).hexdigest() for name,raw in raws.items()};mapping={}
            for name in r.PG_CONTRACT_LOGS:
                test=Path(name).name.rsplit('-',1)[0];path=root/name;path.parent.mkdir(exist_ok=True)
                path.write_text('test '+test+' ... ok\ntest result: ok. 1 passed; 0 failed; 0 ignored;\n')
                mapping[name]={'archived_sha256':r.h.sha(path),'original_sha256':r.h.sha(path)}
            receipt=root/'build.json';r.h.save(receipt,{'source_files':hashes})
            generated=root/'server.c';generated.write_bytes(raws['src/backend/c_pg_async_runtime.c'])
            manifest={'build_receipt':str(receipt),'stacks':{'ku':{'build':{'native':{'server':{'generated_c':str(generated)}}}}}}
            with patch.object(r,'LARGE_PG_SOURCES',hashes),patch.object(r.subprocess,'check_output',side_effect=lambda args,**kw:raws[args[-1].split(':',1)[1]]):
                self.assertTrue(r.verify_large_pg_contracts(root,root,manifest,mapping)['all_four_contracts_passed'])
                bad=root/r.PG_CONTRACT_LOGS[0];original=bad.read_text();bad.write_text(original.replace('0 ignored','1 ignored'))
                mapping[r.PG_CONTRACT_LOGS[0]]['archived_sha256']=r.h.sha(bad)
                with self.assertRaisesRegex(ValueError,'exactly one'):r.verify_large_pg_contracts(root,root,manifest,mapping)
                bad.write_text(original);mapping[r.PG_CONTRACT_LOGS[0]]['archived_sha256']=r.h.sha(bad)
                generated.write_bytes(b'wrong runtime')
                with self.assertRaisesRegex(ValueError,'generated C'):r.verify_large_pg_contracts(root,root,manifest,mapping)

    def test_large_guard_cache_seeds_B_without_importing_overall_build_or_diagnostic(self):
        with tempfile.TemporaryDirectory() as temp,ExitStack() as patches:
            root=Path(temp)/'fresh';a=types.SimpleNamespace(root=root,source=Path(temp)/'source',baseline=r.TYPED_GUARD_PRODUCT,
                prepatch=r.TYPED_GUARD_PRODUCT,candidate=r.LARGE_PG_PRODUCT,screen_variants=[],pg_input_guard=True)
            client={'sha256':'driver','source_sha256':hashlib.sha256(b'go').hexdigest()}
            manifests={n:{'client':client,'stacks':{'ku':{'command':[n+'-ELF']}}} for n in ('A','B')}
            def extract(*args,**kwargs):
                root.mkdir();r.h.save(root/'reuse-original-state.json',{'steps':{k:{'state':'completed'} for k in
                    ('build-A','build-B','build-Driver','diagnostic')}});return {}
            patches.enter_context(patch.object(r.os,'geteuid',return_value=1001))
            patches.enter_context(patch.object(r,'extract_builds',side_effect=extract))
            patches.enter_context(patch.object(r,'restore_json_inputs'));patches.enter_context(patch.object(r,'validate_protocol_inputs'))
            patches.enter_context(patch.object(r,'verify_driver_cache',return_value={'client':client}))
            patches.enter_context(patch.object(r,'verify_manifest',side_effect=lambda root,name,*args,**kw:manifests[name]))
            check=patches.enter_context(patch.object(r,'checked'))
            contracts=patches.enter_context(patch.object(r,'verify_large_pg_contracts'))
            patches.enter_context(patch.object(r.subprocess,'run',return_value=types.SimpleNamespace(returncode=0)))
            patches.enter_context(patch.object(r.subprocess,'check_output',return_value=b'go'))
            state=r.prepare_pg_input_guard_cache(Path(temp)/'archive',a,source_run='41599087071')
            self.assertEqual(set(state['steps']),{'build-A','build-B','build-Driver'})
            self.assertEqual(state['identity'],r.h.hosted_identity(a,'diagnostic'));self.assertEqual(state['imported_measurement_windows'],0)
            self.assertFalse(r.h.load(root/'verified-baseline-cache.json')['new_candidate_build_still_required'])
            contracts.assert_called_once_with(root,a.source,manifests['B'],{})
            check.assert_any_call('B-ELF',r.LARGE_PG_ELF)

    def test_guard_cache_seeds_only_verified_A_driver_not_candidate_or_measurements(self):
        for same_source in (True,False):
            with self.subTest(same_source=same_source),tempfile.TemporaryDirectory() as temp,ExitStack() as patches:
                root=Path(temp)/'fresh';candidate='c'*40
                a=types.SimpleNamespace(root=root,source=Path(temp)/'repo',baseline=r.TYPED_GUARD_PRODUCT,
                    prepatch=r.TYPED_GUARD_PRODUCT,candidate=candidate,screen_variants=[],pg_input_guard=True)
                client={'sha256':'driver','source_sha256':hashlib.sha256(b'go').hexdigest()}
                normal={'client':client,'stacks':{'ku':{'command':['immutable-A']}}}
                def extract(*args,**kwargs):
                    root.mkdir();r.h.save(root/'reuse-original-state.json',{'steps':{k:{'state':'completed'}
                        for k in ('build-A','build-Driver')}});return {}
                patches.enter_context(patch.object(r.os,'geteuid',return_value=1001))
                patches.enter_context(patch.object(r,'extract_builds',side_effect=extract))
                patches.enter_context(patch.object(r,'restore_json_inputs'))
                patches.enter_context(patch.object(r,'validate_protocol_inputs'))
                patches.enter_context(patch.object(r,'verify_driver_cache',return_value={'client':client}))
                patches.enter_context(patch.object(r,'verify_manifest',return_value=normal))
                patches.enter_context(patch.object(r,'checked'))
                patches.enter_context(patch.object(r.subprocess,'run',return_value=types.SimpleNamespace(returncode=0)))
                patches.enter_context(patch.object(r.subprocess,'check_output',return_value=b'go' if same_source else b'changed'))
                if not same_source:
                    with self.assertRaisesRegex(ValueError,'cached driver source differs'):r.prepare_pg_input_guard_cache(Path(temp)/'archive',a)
                    self.assertFalse((root/'hosted-state.json').exists());continue
                state=r.prepare_pg_input_guard_cache(Path(temp)/'archive',a)
                self.assertEqual(state['identity'],r.h.hosted_identity(a,'diagnostic'))
                self.assertEqual(set(state['steps']),{'build-A','build-Driver'})
                self.assertEqual(state['imported_measurement_windows'],0)
                self.assertNotIn('build-B',state['steps']);self.assertNotIn('diagnostic',state['steps'])
                self.assertTrue(r.h.load(root/'verified-baseline-cache.json')['new_candidate_build_still_required'])

    def test_fixed_guard_cache_only_imports_default_A_and_Driver_builds(self):
        with tempfile.TemporaryDirectory() as temp:
            root=Path(temp);path=root/'guard.tgz'
            identity=dict(baseline=r.TYPED_GUARD_PRODUCT,prepatch=r.TYPED_GUARD_PRODUCT,
                candidate=r.TYPED_GUARD_REJECTED,pg_input_guard=True)
            steps={k:{'state':'completed'} for k in ('build','build-A','build-B','build-Driver')}
            steps['diagnostic']={'state':'failed','result':{'memory_guard':False}}
            files={n+'/build/bin/artifact':n.encode() for n in ('A','B','Driver')}
            files['pg-input-guard-observation/old.json']=b'old score never imported'
            files['hosted-state.json']=json.dumps(dict(identity=identity,steps=steps)).encode()
            index=dict(path_root=r.TOKEN,raw_and_redacted_hashes_distinct=True,files=[dict(path=k,
                bytes=len(v),archived_sha256=hashlib.sha256(v).hexdigest()) for k,v in files.items()])
            files['portable-file-map.json']=json.dumps(index).encode()
            with tarfile.open(path,'w:gz') as tar:
                for name,data in files.items():
                    info=tarfile.TarInfo(name);info.size=len(data);tar.addfile(info,io.BytesIO(data))
            digest=r.h.sha(path)
            with self.assertRaisesRegex(ValueError,'build-only'):
                r.extract_builds(path,root/'ordinary',digest,{'A','Driver'})
            with self.assertRaisesRegex(ValueError,'fixed default'):
                r.extract_builds(path,root/'wrong-sha',digest,{'A','Driver'},pg_input_cache=True)
            with patch.object(r,'TYPED_GUARD_ARCHIVE_SHA',digest):
                with self.assertRaisesRegex(ValueError,'fixed default'):
                    r.extract_builds(path,root/'rejected-B',digest,{'A','B','Driver'},pg_input_cache=True)
                r.extract_builds(path,root/'good',digest,{'A','Driver'},pg_input_cache=True)
            self.assertFalse((root/'good/B').exists());self.assertFalse((root/'good/pg-input-guard-observation').exists())
            provenance=r.h.load(root/'good/reuse-original-state.json')
            self.assertFalse(provenance['measurement_results_imported'])
            self.assertEqual(set(provenance['steps']),{'build','build-A','build-B','build-Driver'})

    def test_guard_cache_typed_preparation_uses_only_cached_cli_and_checks_exact_ELF(self):
        for mismatch in (False,True):
            with self.subTest(mismatch=mismatch),tempfile.TemporaryDirectory() as temp,ExitStack() as patches:
                root=Path(temp)/'fresh';root.mkdir();tools=Path(temp)/'tools';tools.mkdir()
                protocol=root/'Driver/src/tests/benchmarks/cross_stack/protocol.json';protocol.parent.mkdir(parents=True);protocol.write_text('{}')
                client=dict(command=['/cached-driver'],sha256='a'*64)
                normal=dict(candidate_base=r.TYPED_GUARD_PRODUCT,protocol=str(protocol),client=client,
                    stacks={'ku':dict(command=['/normal-ELF'],build=dict(cli='/cached-cli',cli_sha256='b'*64,
                        native={'server':dict(source='/normal.ku')}))})
                typed=dict(candidate_base=r.TYPED_GUARD_PRODUCT,stacks={'ku':dict(command=['/typed-ELF'],
                    build=dict(native={'server':dict(binary='/typed-ELF',source='/typed.ku')}))})
                def snapshot(source,commit,destination):
                    self.assertEqual(commit,r.TYPED_GUARD_PRODUCT);destination.mkdir(parents=True)
                def finish(**kwargs):
                    r.h.save(root/'BP/manifest.json',typed)
                builder=types.SimpleNamespace(commands=[{'label':'ku-native-server'}],finish=finish)
                builder.ku_native=patches.enter_context(patch.object(types.SimpleNamespace(call=None),'call'))
                patches.enter_context(patch.object(r.os,'geteuid',return_value=1))
                patches.enter_context(patch.object(r.platform,'system',return_value='Linux'))
                patches.enter_context(patch.object(r.platform,'machine',return_value='aarch64'))
                patches.enter_context(patch.object(r,'extract_builds',return_value={}))
                patches.enter_context(patch.object(r,'restore_json_inputs'))
                patches.enter_context(patch.object(r,'validate_protocol_inputs'))
                patches.enter_context(patch.object(r,'verify_driver_cache',return_value={'client':client}))
                verify=patches.enter_context(patch.object(r,'verify_manifest',return_value=normal))
                patches.enter_context(patch.object(r.h,'snapshot',side_effect=snapshot))
                factory=patches.enter_context(patch.object(r.h.bl,'Builder',return_value=builder))
                patches.enter_context(patch.object(r.h.fc,'version_identity',return_value={'identity':'fixture'}))
                def checked(path,expected):
                    if mismatch and path=='/typed-ELF':raise ValueError('reused file identity differs')
                check=patches.enter_context(patch.object(r,'checked',side_effect=checked))
                if mismatch:
                    with self.assertRaisesRegex(ValueError,'identity differs'):
                        r.prepare_typed_pg_from_input_guard(Path(temp)/'fixed.tgz',root,Path(temp)/'source',tools)
                    self.assertFalse((root/'typed-pg-cache.json').exists())
                else:
                    result=r.prepare_typed_pg_from_input_guard(Path(temp)/'fixed.tgz',root,Path(temp)/'source',tools)
                    self.assertEqual(result,{'B':str(root/'A/manifest.json'),'BP':str(root/'BP/manifest.json')})
                    self.assertEqual(factory.call_args.kwargs['ku_pg_read'],'typed')
                    verify.assert_called_once_with(root,'A',r.TYPED_GUARD_PRODUCT,{},client_source_name='Driver')
                    receipt=r.h.load(root/'typed-pg-cache.json')
                    self.assertFalse(receipt['Rust_rebuilt']);self.assertFalse(receipt['Go_rebuilt'])
                    self.assertEqual(receipt['imported_measurement_windows'],0)
                builder.ku_native.assert_called_once_with(Path('/cached-cli'))
                check.assert_any_call('/typed-ELF',r.PG_CACHE_ELFS['typed'])

    def test_guard_driver_verifies_existing_client_and_control_without_rebuilding(self):
        with tempfile.TemporaryDirectory() as temp:
            root=Path(temp);directory=root/'Driver';directory.mkdir();header=bytearray(20)
            header[:6]=b'\x7fELF\x02\x01';header[18:20]=(183).to_bytes(2,'little')
            client=directory/'client';control=directory/'control';client.write_bytes(header);control.write_bytes(header+b'control')
            source=directory/'src/tests/benchmarks/cross_stack/client/main.go';source.parent.mkdir(parents=True);source.write_text('package main')
            receipt=directory/'build.json';r.h.save(receipt,dict(candidate_base=r.TYPED_GUARD_REJECTED,
                failure=None,commands=[{'label':'go-client','exit_code':0}]))
            r.h.save(directory/'manifest.json',dict(candidate_base=r.TYPED_GUARD_REJECTED,build_receipt=str(receipt),
                client=dict(command=[str(client)],sha256=r.h.sha(client),source_sha256=r.h.sha(source)),
                stacks={'control':dict(command=[str(control)],build={'binary_sha256':r.h.sha(control)})}))
            self.assertEqual(r.verify_driver_cache(root)['client']['sha256'],r.h.sha(client))
            client.write_bytes(header+b'tampered')
            with self.assertRaisesRegex(ValueError,'identity differs'):r.verify_driver_cache(root)

    def test_fixed_typed_pg_cache_reuses_only_build_trees_never_old_measurements(self):
        with tempfile.TemporaryDirectory() as temp:
            root=Path(temp);path=root/'typed.tgz'
            files={name+'/build/bin/ku-server':name.encode() for name in ('B','BP','R')}
            files['final/rows.json']=b'old measurement never imported'
            files['hosted-state.json']=json.dumps({'identity':{'candidate':r.TYPED_PG_PRODUCT},
                'steps':{k:{'state':'completed'} for k in ('build','run','final')}}).encode()
            index={'path_root':r.TOKEN,'raw_and_redacted_hashes_distinct':True,'files':[
                {'path':k,'bytes':len(v),'archived_sha256':hashlib.sha256(v).hexdigest()} for k,v in files.items()]}
            files['portable-file-map.json']=json.dumps(index).encode()
            with tarfile.open(path,'w:gz') as tar:
                for name,data in files.items():
                    info=tarfile.TarInfo(name);info.size=len(data);tar.addfile(info,io.BytesIO(data))
            digest=r.h.sha(path)
            with self.assertRaisesRegex(ValueError,'build-only'):
                r.extract_builds(path,root/'default',digest,{'B','BP','R'})
            with self.assertRaisesRegex(ValueError,'fixed typed'):
                r.extract_builds(path,root/'wrong',digest,{'B','BP','R'},typed_pg_observation=True)
            with patch.object(r,'TYPED_PG_ARCHIVE_SHA',digest):
                with self.assertRaisesRegex(ValueError,'fixed typed'):
                    r.extract_builds(path,root/'wrong-selection',digest,{'B','R'},typed_pg_observation=True)
                r.extract_builds(path,root/'good',digest,{'B','BP','R'},typed_pg_observation=True)
            self.assertFalse((root/'good/final').exists())
            old=r.h.load(root/'good/reuse-original-state.json')
            self.assertFalse(old['measurement_results_imported']);self.assertEqual(set(old['steps']),{'build'})
            self.assertFalse((root/'good/hosted-state.json').exists())
            self.assertEqual((root/'good/BP/build/bin/ku-server').read_bytes(),b'BP')

    def test_owner_reuse_requires_the_fixed_triple_before_touching_archive(self):
        args=types.SimpleNamespace(memory_owner_probe=True,source_run='bad',archive_sha256=r.OWNER_ARCHIVE_SHA,candidate=r.OWNER_PRODUCT)
        with patch.object(r,'extract_builds') as extract,self.assertRaisesRegex(ValueError,'fixed verified'):
            r.prepare(args)
        extract.assert_not_called()

    def test_measured_owner_archive_exception_never_imports_observations(self):
        with tempfile.TemporaryDirectory() as temp:
            root=Path(temp);path=root/'owner.tgz'
            files={'B/build/bin/ku-server':b'B','R/build/bin/client':b'client',
                'normal-self-probe/summary.json':b'old observation',
                'hosted-state.json':json.dumps({'steps':{k:{'state':'completed'} for k in ('build','build-B','build-R','diagnostic')}}).encode()}
            index={'path_root':r.TOKEN,'raw_and_redacted_hashes_distinct':True,'files':[
                {'path':k,'bytes':len(v),'archived_sha256':hashlib.sha256(v).hexdigest()} for k,v in files.items()]}
            files['portable-file-map.json']=json.dumps(index).encode()
            with tarfile.open(path,'w:gz') as tar:
                for name,raw in files.items():
                    info=tarfile.TarInfo(name);info.size=len(raw);tar.addfile(info,io.BytesIO(raw))
            digest=r.h.sha(path)
            with self.assertRaisesRegex(ValueError,'build-only'):r.extract_builds(path,root/'old',digest,{'B','R'})
            with self.assertRaisesRegex(ValueError,'build-only'):r.extract_builds(path,root/'wrong',digest,{'B','R'},owner_observation=True)
            with patch.object(r,'OWNER_ARCHIVE_SHA',digest):r.extract_builds(path,root/'good',digest,{'B','R'},owner_observation=True)
            self.assertFalse((root/'good/normal-self-probe').exists())
            self.assertNotIn('diagnostic',r.h.load(root/'good/reuse-original-state.json')['steps'])
            self.assertEqual((root/'good/B/build/bin/ku-server').read_bytes(),b'B')

    def test_reuse_selection_and_state_identity_match_the_explicit_hosted_flag(self):
        for selected,common in ((None,None),(False,False),(True,False),(True,True)):
            with self.subTest(selected=selected,common=common),tempfile.TemporaryDirectory() as d,ExitStack() as patches:
                root=Path(d);fresh=root/'fresh';tools=root/'tools';tools.mkdir()
                args=types.SimpleNamespace(root=fresh,source=root/'source',tools=tools,archive=root/'inert.tgz',
                    archive_sha256='1'*64,source_run='123',baseline=r.h.BASELINE,
                    candidate='b'*40,prepatch='f'*40,screen_variants=[])
                if selected is not None:args.task_json_screen=selected
                if common is not None:args.common_http_screen=common
                def extract(archive,root,digest,expected):
                    for name in expected:(root/name).mkdir(parents=True)
                    # Even capable normal cached builds must not implicitly
                    # change the requested experiment to a Task JSON screen.
                    ir=root/'B/src/src/ir/task.rs';ir.parent.mkdir(parents=True)
                    ir.write_text('enum TaskOp {\n    JsonDecode {\n}\n')
                    return {}
                patches.enter_context(patch.object(r.os,'geteuid',return_value=1))
                patches.enter_context(patch.object(r.platform,'system',return_value='Linux'))
                patches.enter_context(patch.object(r.platform,'machine',return_value='aarch64'))
                extract_mock=patches.enter_context(patch.object(r,'extract_builds',side_effect=extract))
                patches.enter_context(patch.object(r,'restore_json_inputs'))
                patches.enter_context(patch.object(r,'validate_protocol_inputs',return_value={}))
                verify=patches.enter_context(patch.object(r,'verify_manifest'))
                patches.enter_context(patch.object(r,'new_driver'))
                patches.enter_context(patch.object(r.h,'complete_build',return_value={'execution_complete':True}))
                self.assertTrue(r.prepare(args)['execution_complete'])
                expected={'A','B','R',*(['BT'] if selected else [])}
                self.assertEqual(set(extract_mock.call_args.args[3]),expected)
                self.assertEqual({call.args[1] for call in verify.call_args_list},expected)
                state=r.h.load(fresh/'hosted-state.json')
                self.assertEqual(state['identity'],r.h.hosted_identity(args,'followup-screen'))
                self.assertIs(state['identity']['task_json_screen'],selected is True)
                self.assertIs(state['identity']['common_http_screen'],common is True)

    def test_reuse_rejects_common_http_without_explicit_task_before_reading_archive(self):
        args=types.SimpleNamespace(common_http_screen=True,task_json_screen=False)
        with patch.object(r,'extract_builds') as extract:
            with self.assertRaisesRegex(ValueError,'--task-json-screen'):r.prepare(args)
            extract.assert_not_called()

    def test_final_task_json_cannot_reuse_a_followup_archive(self):
        args=types.SimpleNamespace(final_task_json=True,task_json_screen=False,common_http_screen=False)
        with patch.object(r,'extract_builds') as extract:
            with self.assertRaisesRegex(ValueError,'--final-task-json'):r.prepare(args)
            extract.assert_not_called()

    def test_reformatted_source_json_is_recovered_from_exact_commit_without_redefining_corpus(self):
        with tempfile.TemporaryDirectory() as d:
            root=Path(d);source=root/'repository';source.mkdir()
            subprocess.run(['git','init','-q',str(source)],check=True)
            prefix=Path('tests/benchmarks/cross_stack');directory=source/prefix
            (directory/'corpora').mkdir(parents=True)
            original={}
            spec={}
            for name in ('object','array','escape'):
                raw='{"counter":7,"data":{"text":"中文🙂"}}\n'.encode('utf-8')
                relative='corpora/'+name+'.json';original[relative]=raw
                spec[name]={'path':relative,'bytes':len(raw),'sha256':hashlib.sha256(raw).hexdigest(),'counter':7}
            original['protocol.json']=(json.dumps({'schema':1,'corpora':spec},indent=2)+'\n').encode()
            for relative,raw in original.items():(directory/relative).write_bytes(raw)
            subprocess.run(['git','-C',str(source),'add','.'],check=True)
            subprocess.run(['git','-C',str(source),'-c','user.name=Fixture','-c','user.email=fixture@example.invalid','commit','-qm','immutable inputs'],check=True)
            commit=subprocess.check_output(['git','-C',str(source),'rev-parse','HEAD'],text=True).strip()
            destination=root/'fresh';mapping={}
            for name in ('A','B'):
                for relative,raw in original.items():
                    path=name+'/src/'+prefix.as_posix()+'/'+relative
                    exported=(json.dumps(json.loads(raw),sort_keys=True,separators=(',',':'))+'\n').encode()
                    self.assertNotEqual(exported,raw)
                    target=destination/path;target.parent.mkdir(parents=True,exist_ok=True);target.write_bytes(exported)
                    mapping[path]={'original_sha256':hashlib.sha256(raw).hexdigest(),'archived_sha256':hashlib.sha256(exported).hexdigest(),'bytes':len(exported)}
                artifact=destination/name/'build/server.c';artifact.parent.mkdir(parents=True);artifact.write_bytes(b'untouched C')
            r.restore_json_inputs(source,destination,{'A':commit,'B':commit},mapping)
            for name in ('A','B'):
                result=r.validate_protocol_inputs(destination,name,mapping)
                self.assertEqual(result['corpora']['object']['sha256'],spec['object']['sha256'])
                for relative,raw in original.items():self.assertEqual((destination/name/'src'/prefix/relative).read_bytes(),raw)
                self.assertEqual((destination/name/'build/server.c').read_bytes(),b'untouched C')
            corpus=destination/'A/src'/prefix/'corpora/object.json'
            corpus.write_bytes(corpus.read_bytes()+b' ')
            with self.assertRaises(ValueError):r.validate_protocol_inputs(destination,'A',mapping)

    def test_checksum_or_unsafe_member_cannot_write_destination(self):
        for kind in ('checksum','traversal','absolute','link','duplicate'):
            with self.subTest(kind=kind),tempfile.TemporaryDirectory() as d:
                root=Path(d);tar=root/'cache.tgz';destination=root/'fresh'
                member=None
                if kind!='checksum':
                    member=tarfile.TarInfo({'traversal':'../outside','absolute':'/absolute',
                        'link':'A/link','duplicate':'A/build/bin/ku-server'}[kind])
                    if kind=='link':member.type=tarfile.SYMTYPE;member.linkname='../outside'
                    else:member.size=1
                digest=archive(tar,member)
                with self.assertRaises(ValueError):
                    r.extract_builds(tar,destination,'0'*64 if kind=='checksum' else digest,{'A'})
                self.assertFalse(destination.exists());self.assertFalse((root/'outside').exists())

    def test_only_fresh_tools_directory_is_allowed_and_old_results_never_resume(self):
        with tempfile.TemporaryDirectory() as d:
            root=Path(d);tar=root/'cache.tgz';digest=archive(tar)
            existing=root/'existing';existing.mkdir();(existing/'keep').write_text('user data')
            with self.assertRaises(ValueError):r.extract_builds(tar,existing,digest,{'A'})
            self.assertEqual((existing/'keep').read_text(),'user data')
            fresh=root/'fresh';(fresh/'tools').mkdir(parents=True)
            mapping=r.extract_builds(tar,fresh,digest,{'A'})
            r.checked(fresh/'A/build/bin/ku-server',mapping['A/build/bin/ku-server']['archived_sha256'])
            self.assertTrue((fresh/'reuse-original-state.json').exists())
            self.assertFalse((fresh/'hosted-state.json').exists())
            self.assertTrue((fresh/'A/build/bin/ku-server').stat().st_mode&0o100)
            old=root/'measured.tgz';digest=archive(old,old_run=True)
            with self.assertRaisesRegex(ValueError,'build-only'):
                r.extract_builds(old,root/'old-measurement',digest,{'A'})
            self.assertFalse((root/'old-measurement').exists())
            with self.assertRaisesRegex(ValueError,'build was not completed'):
                r.extract_builds(tar,root/'missing-task-build',r.h.sha(tar),{'A','BT'})
            self.assertFalse((root/'missing-task-build').exists())

    def test_corrupt_or_wrong_architecture_normal_artifact_is_rejected(self):
        with tempfile.TemporaryDirectory() as d:
            path=Path(d)/'binary';header=bytearray(20);header[:6]=b'\x7fELF\x02\x01';header[18:20]=(183).to_bytes(2,'little')
            path.write_bytes(header);r.require_elf(path)
            good=r.h.sha(path);path.write_bytes(header+b'tampered')
            with self.assertRaises(ValueError):r.checked(path,good)
            header[18:20]=(62).to_bytes(2,'little');path.write_bytes(header)
            with self.assertRaises(ValueError):r.require_elf(path)


if __name__=='__main__':unittest.main()
