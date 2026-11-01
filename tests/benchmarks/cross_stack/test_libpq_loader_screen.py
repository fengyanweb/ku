"""Pure loader-plan/mapping guards; never starts a service or compiler."""
import copy
from contextlib import redirect_stderr, redirect_stdout
import io
import json
from pathlib import Path
import shlex
import tempfile
import types
import unittest
from unittest.mock import patch
import hosted_cycle as h
import libpq_loader_screen as l


class LoaderScreenTests(unittest.TestCase):
    def test_actual_workflow_argv_pass_main_and_wrong_global_baseline_is_rejected(self):
        workflow=Path(__file__).resolve().parents[3]/'.github/workflows/native-three-os.yml'
        lines=workflow.read_text().split('\n  libpq-loader-memory:',1)[1].splitlines()
        commands=[];script='tests/benchmarks/cross_stack/hosted_cycle.py'
        for i,line in enumerate(lines):
            if line.strip().startswith('sudo -n -E python3 -B '+script):
                parts=[]
                while True:
                    part=lines[i].strip();parts.append(part.removesuffix('\\'))
                    if not part.endswith('\\'):break
                    i+=1
                tokens=shlex.split(' '.join(parts));commands.append(tokens[tokens.index(script)+1:])
        self.assertEqual(len(commands),2)
        self.assertEqual([cmd[cmd.index('--phase')+1] for cmd in commands],['diagnostic','archive'])
        with tempfile.TemporaryDirectory() as d:
            temporary=Path(d);source=temporary/'checkout';source.mkdir()
            root=temporary/'runtime/libpq-loader-memory/memory9'
            for arm in ('A','B'):
                (root/arm).mkdir(parents=True);(root/arm/'manifest.json').write_text(json.dumps(dict(uid=1001,gid=1001)))
            for raw in commands:
                argv=[arg.replace('$RUNNER_TEMP',str(temporary/'runtime')).replace('$GITHUB_WORKSPACE',str(source)) for arg in raw]
                phase=argv[argv.index('--phase')+1]
                self.assertEqual(argv[argv.index('--baseline')+1],h.BASELINE)
                with self.subTest(phase=phase),patch.object(h.platform,'system',return_value='Linux'),\
                        patch.object(h.platform,'machine',return_value='aarch64'),patch.object(h.signal,'signal'),\
                        patch.object(h.signal,'alarm'),patch.object(h,phase,return_value=dict(execution_complete=True,goal_status='unknown')) as operation:
                    with redirect_stdout(io.StringIO()):self.assertEqual(h.main(argv),0)
                    operation.assert_called_once()
                    actual=operation.call_args.args[0]
                    self.assertEqual((actual.baseline,actual.candidate,actual.prepatch),(h.BASELINE,l.SOURCE,l.SOURCE))
                    operation.reset_mock();bad=list(argv);bad[bad.index('--baseline')+1]=l.SOURCE
                    errors=io.StringIO()
                    with redirect_stderr(errors),self.assertRaises(SystemExit) as raised:h.main(bad)
                    self.assertEqual(raised.exception.code,2);self.assertIn('frozen full commit identities required',errors.getvalue())
                    operation.assert_not_called()

    def test_all_exact_system_dependencies_are_checked_and_mismatch_stops(self):
        with tempfile.TemporaryDirectory() as d:
            root=Path(d);plan={'libpq_loader':{}}
            modules={'/usr/lib/lib%d.so'%i:'a'*64 for i in range(20)}
            modules['/home/runner/work/_temp/libpq-loader-private-build/common-install/lib/libpq.so.5.18']='b'*64
            for arm in ('A','B'):
                p=root/(arm+'.json');p.write_text(json.dumps(dict(runtime_module_sha256=modules)))
                plan['libpq_loader'][arm]={'build_record':str(p)}
            with patch.object(Path,'is_file',return_value=True),patch.object(l,'digest',return_value='a'*64) as digest:
                self.assertEqual(len(l.verify_system_modules(plan)['files']),20);self.assertEqual(digest.call_count,20)
            with patch.object(Path,'is_file',return_value=True),patch.object(l,'digest',return_value='0'*64),self.assertRaises(ValueError):
                l.verify_system_modules(plan)
            modules['/usr/lib/lib0.so']='0'*64
            (root/'B.json').write_text(json.dumps(dict(runtime_module_sha256=modules)))
            with self.assertRaises(ValueError):l.verify_system_modules(plan)

    def test_reused_service_owner_cannot_be_root_or_differ_between_arms(self):
        with tempfile.TemporaryDirectory() as d:
            root=Path(d)
            for arm in ('A','B'):
                (root/arm).mkdir();(root/arm/'manifest.json').write_text(json.dumps(dict(uid=1001,gid=1001)))
            self.assertEqual(l.reused_owner(root),dict(uid=1001,gid=1001))
            for uid in (0,True,1002):
                (root/'B/manifest.json').write_text(json.dumps(dict(uid=uid,gid=1001)))
                with self.subTest(uid=uid),self.assertRaises(ValueError):l.reused_owner(root)

    def test_explicit_same_source_only_and_existing_modes_remain_distinct(self):
        good=dict(http_short_screen=True,http_short_goal='memory-probe',prepatch=l.SOURCE,
            candidate=l.SOURCE,phase='diagnostic',libpq_loader_screen=True)
        h.validate_screen_selection(types.SimpleNamespace(**good),'diagnostic')
        for key,value in [('http_short_screen',False),('http_short_goal','cpu'),('candidate','a'*40),('phase','build')]:
            bad=dict(good,**{key:value})
            with self.subTest(key=key),self.assertRaises(ValueError):l.validate_selection(types.SimpleNamespace(**bad),'diagnostic')
        with self.assertRaises(ValueError):h.validate_screen_selection(types.SimpleNamespace(**dict(good,libpq_loader_screen=False)),'diagnostic')

    def test_only_plan_identity_changes_not_original_windows_or_gates(self):
        libraries={arm:dict(path='/'+arm+'/libpq.so.5.18',sha256=arm.lower()*64,
            build_record='/'+arm+'/record.json',build_record_sha256='1'*64,
            functional_gate='/gate.json',functional_gate_sha256='2'*64,
            source_build_identity='/identities.json',source_build_identity_sha256='3'*64) for arm in ('A','B')}
        plan=l.frozen_plan({},libraries);original=h.http_short_plan({},'memory-probe')
        for key in ('windows','adoption_thresholds','resources','quality_policy','maximum_windows','maximum_replacements'):
            self.assertEqual(plan[key],original[key])
        self.assertEqual(plan['prepatch'],l.SOURCE)
        for mutation in ('missing','sha'):
            bad=copy.deepcopy(libraries)
            if mutation=='missing':del bad['B']
            else:bad['B']['sha256']='pending'
            with self.assertRaises(ValueError):l.frozen_plan({},bad)

    def test_actual_maps_require_selected_library_retained_SSL_GSS_and_exact_optional_closure(self):
        with tempfile.TemporaryDirectory() as d:
            pq=Path(d).resolve()/'libpq.so.5.18';pq.write_bytes(b'lib')
            def maps(arm):
                paths=[str(pq),'/usr/lib/libssl.so.3','/usr/lib/libcrypto.so.3','/usr/lib/libgssapi_krb5.so.2.2']
                if arm=='A':paths+=['/usr/lib/'+x for x in l.OPTIONAL]
                return ''.join(f'1000-2000 r--p 00000000 08:01 1 {x}\n' for x in paths).encode()
            for arm in ('A','B'):
                self.assertEqual(l.check_maps(maps(arm),arm,pq)['optional_closure_count'],12 if arm=='A' else 0)
            for raw,arm in [(maps('A'),'B'),(maps('B'),'A'),(maps('B').replace(b'libssl.so.3',b'wrong.so'),'B'),
                    (maps('B').replace(str(pq).encode(),b'/wrong/libpq.so.5.18'),'B'),(b'x'*(1<<20|1),'B')]:
                with self.subTest(arm=arm),self.assertRaises(ValueError):l.check_maps(raw,arm,pq)

    def test_runtime_override_keeps_original_D_link_receipt_and_rejects_other_input_changes(self):
        with tempfile.TemporaryDirectory() as d:
            root=Path(d).resolve();gate=root/'gate.json';gate.write_text('{}')
            common=root/'identities.json';common.write_text('{}');versions={};libraries={};bindings={}
            for arm in ('A','B'):
                directory=root/arm;directory.mkdir();pq=directory/'libpq.so.5.18';pq.write_text(arm)
                (directory/'libpq.so.5').symlink_to(pq.name)
                record=directory/'record.json';record.write_text(json.dumps(dict(shared_sha256=l.digest(pq),
                    DT_NEEDED=['libssl.so.3','libcrypto.so.3','libgssapi_krb5.so.2']+(['libldap.so.2'] if arm=='A' else []),
                    configure_flags='same',exports=['PQlibVersion'],pkg_config_static_tokens=['-lpq','-lldap'])))
                libraries[arm]=dict(path=str(pq),sha256=l.digest(pq),build_record=str(record),build_record_sha256=l.digest(record),
                    functional_gate=str(gate),functional_gate_sha256=l.digest(gate),source_build_identity=str(common),source_build_identity_sha256=l.digest(common))
                manifest=dict(libpq_loader_arm=arm,stacks={'ku':{'env':{'LD_LIBRARY_PATH':str(directory)}}})
                (directory/'manifest.json').write_text(json.dumps(manifest));versions[arm]={'root':str(directory)}
                files={key:{'sha256':'d'*64} for key in ('expanded source','Ku CLI','driver ELF')}
                files.update(ELF={'sha256':l.ELF_SHA},**{'generated C':{'sha256':l.C_SHA},
                    'libpq':{'path':'/original/D/libpq.so','sha256':'d'*64},
                    'declared:runtime libpq':{'path':str(pq),'sha256':l.digest(pq)}})
                bindings[arm]=dict(source_commit=l.SOURCE,files=files,build_mode={})
            plan=l.frozen_plan(versions,libraries);l.validate_bindings(plan,bindings)
            for change in ('ELF','Ku CLI','runtime libpq'):
                bad=copy.deepcopy(bindings)
                key='declared:runtime libpq' if change=='runtime libpq' else change
                bad['B']['files'][key]['sha256']='0'*64
                with self.subTest(change=change),self.assertRaises(ValueError):l.validate_bindings(plan,bad)


if __name__=='__main__':unittest.main()
