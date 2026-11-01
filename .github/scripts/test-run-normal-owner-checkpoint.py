#!/usr/bin/env python3
"""Pure lifecycle/default-library checks; no compiler, service or database."""
from contextlib import ExitStack
import importlib.util
import json
import re
from pathlib import Path
import tempfile
import unittest
from unittest.mock import Mock, patch

spec=importlib.util.spec_from_file_location('owner_run',Path(__file__).with_name('run-normal-owner-checkpoint.py'))
r=importlib.util.module_from_spec(spec);spec.loader.exec_module(r)


class CheckpointRunnerTests(unittest.TestCase):
    def test_all_action_refs_are40_hex_and_new_upload_matches_existing_pin(self):
        text=(r.REPO/'.github/workflows/native-three-os.yml').read_text()
        uses=re.findall(r'^\s*(?:-\s*)?uses:\s+(\S+)',text,re.M)
        self.assertTrue(uses)
        for action in uses:
            self.assertRegex(action,r'^[^@]+@[0-9a-f]{40}$')
        before,added=text.split('\n  normal-owner-checkpoint:',1)
        existing=set(re.findall(r'uses:\s+(actions/upload-artifact@\S+)',before))
        new=re.findall(r'uses:\s+(actions/upload-artifact@\S+)',added)
        self.assertEqual(existing,{'actions/upload-artifact@043fb46d1a93c77aae656e7c1c64a875d1fc6a0a'})
        self.assertEqual(new,list(existing))
    def test_ECHILD_keeps_observation_unknown_and_never_hides_process_residue(self):
        for mode in ('already-reaped','already-reaped-with-residue'):
            with self.subTest(mode=mode):self.lifecycle(mode)
    def test_export_manifest_includes_maps_smaps_layout_and_exact_hashes(self):
        with tempfile.TemporaryDirectory() as temp:
            p=Path(temp);(p/'checkpoint').mkdir()
            for name in ('checkpoint/maps.log','checkpoint/smaps.log','layout.json','normal-global-symbol.readback'):
                (p/name).write_text(name)
            r.evidence_manifest(p)
            v=json.loads((p/'manifest.json').read_text())
            self.assertEqual(len(v['files']),4)
            self.assertIn('checkpoint/smaps.log',[x['path'] for x in v['files']])
            for row in v['files']:self.assertEqual(row['sha256'],r.checkpoint.digest(p/row['path']))
    def test_pre_service_failure_is_unknown_and_removes_only_temporary_inputs(self):
        with tempfile.TemporaryDirectory() as temp,ExitStack() as stack:
            root=Path(temp)/'run';old=Path(temp)/'old.zip';old.write_bytes(b'owned artifact')
            stack.enter_context(patch.object(r.platform,'system',return_value='Linux'))
            stack.enter_context(patch.object(r.platform,'machine',return_value='aarch64'))
            stack.enter_context(patch.object(r.os,'geteuid',return_value=0))
            stack.enter_context(patch.dict(r.os.environ,{},clear=True))
            stack.enter_context(patch.object(r.signal,'signal'));stack.enter_context(patch.object(r.signal,'alarm'))
            stack.enter_context(patch.object(r.restorer,'restore_old',return_value={'run':41280471018}))
            original_digest=r.checkpoint.digest
            stack.enter_context(patch.object(r.checkpoint,'digest',side_effect=lambda p:r.D_SHA if str(p)=='/usr/lib/aarch64-linux-gnu/libpq.so.5' else original_digest(p)))
            run=stack.enter_context(patch.object(r,'run_restored',side_effect=ValueError('layout compiler mismatch')))
            self.assertEqual(r.main(['--old-zip',str(old),'--root',str(root)]),1)
            self.assertFalse(old.exists());self.assertEqual(json.loads((root/'evidence/failure.json').read_text())['status'],'unknown')
            self.assertIn('layout compiler mismatch',(root/'evidence/failure.json').read_text());run.assert_called_once()
            self.assertTrue((root/'evidence/manifest.json').exists())
    def test_actual_default_mapping_requires_one_exact_D_hash(self):
        raw=b'1000-2000 r--p 00000000 00:01 9 /lib/libpq.so.5.18\n'
        with patch.object(r.checkpoint,'digest',return_value=r.D_SHA):
            self.assertEqual(r.check_default_maps(raw)['sha256'],r.D_SHA)
        with patch.object(r.checkpoint,'digest',return_value='b'*64):
            with self.assertRaisesRegex(ValueError,'unchanged default'):r.check_default_maps(raw)
        with self.assertRaisesRegex(ValueError,'unchanged default'):
            r.check_default_maps(raw+b'3000-4000 r--p 00000000 00:01 10 /private/libpq.so.5.18\n')
    def test_one_JSON_request_retains_idle_socket_and_wrong_types_fail(self):
        for bad in (False,True):
            with self.subTest(bad=bad),tempfile.TemporaryDirectory() as temp:
                corpus=Path(temp)/'object.json';corpus.write_text('{"counter":7,"data":{"nested":[1,true,"中文"]}}')
                response=Mock(status=200,will_close=False)
                response.read.return_value=json.dumps({'id':'owner-checkpoint-1','counter':8.0 if bad else 8,'data':{'nested':[1,True,'中文']}}).encode()
                connection=Mock();connection.getresponse.return_value=response;connection.sock=object()
                with patch.object(r.http.client,'HTTPConnection',return_value=connection),patch.object(r.checkpoint,'digest',return_value='548a8397f835c17ee8b524054e1da90d00d9293965986c5e48be5692663f5e3a'):
                    if bad:
                        with self.assertRaisesRegex(ValueError,'correctness'):r.single_json(Mock(address='127.0.0.1:1234'),corpus)
                        connection.close.assert_called_once()
                    else:
                        kept,receipt=r.single_json(Mock(address='127.0.0.1:1234'),corpus)
                        self.assertIs(kept,connection);self.assertEqual(receipt['requests'],1);connection.close.assert_not_called()
                    connection.request.assert_called_once()
    def test_natural_service_exit_requires_exit0(self):
        cleanup=dict(forced=False,cgroup_removed=True,after={'process_count':0},exit_code=1)
        self.assertFalse(r.natural(cleanup,service=True));self.assertTrue(r.natural(cleanup))

    def lifecycle(self,mode):
        with tempfile.TemporaryDirectory() as temp,ExitStack() as stack:
            root=Path(temp).resolve();(root/'original/B').mkdir(parents=True);(root/'evidence').mkdir();(root/'layout').mkdir()
            cgroup=root/'own-cgroup'
            manifest=dict(candidate_base=r.SOURCE,uid=1001,gid=1001,stacks={'ku':{'env':{},'build':{'libpq_source':'original'}}})
            (root/'original/B/manifest.json').write_text(json.dumps(manifest))
            # Cgroup name is normally fixed under/sys; substitute only its
            # path in this pure fixture so remove() is really exercised.
            real_save=r.save
            def save(path,value):
                if str(path).endswith('/fixture/manifest.json'):value['cgroup_root']=str(cgroup)
                return real_save(path,value)
            proof={'byte_equal':True,'current':{'sha256':r.C_SHA},'old_source_commit':r.SOURCE}
            original_load=r.json.loads
            def loads(data,*args,**kwargs):
                parsed=original_load(data,*args,**kwargs)
                if isinstance(parsed,dict) and parsed.get('current',{}).get('sha256')==r.C_SHA:return proof
                return parsed
            fake_db=Mock()
            def start_db():
                cgroup.mkdir()
                fp=root/'fixture/manifest.json';v=json.loads(fp.read_text());v.update(db_url='postgresql://own-fixture',db_cgroup=str(cgroup/'db'));fp.write_text(json.dumps(v));fake_db.manifest_path=fp
                return {'status':'running','pid':3210}
            fake_db.start.side_effect=start_db;fake_db.stop.return_value=dict(forced=False,cgroup_removed=True,after={'process_count':0})
            service=Mock();service.process.pid=1234;service.metrics.return_value={'queued':0};service.stop.return_value=dict(forced=False,cgroup_removed=True,after={'process_count':0},exit_code=0)
            harness=Mock();harness.start.return_value=service
            connection=Mock()
            stack.enter_context(patch.object(r,'save',side_effect=save))
            stack.enter_context(patch.object(r.json,'loads',side_effect=loads))
            original_exists=Path.exists
            stack.enter_context(patch.object(r.Path,'exists',autospec=True,side_effect=lambda p:original_exists(cgroup) if str(p).startswith('/sys/fs/cgroup/ku-idle-owner-') else original_exists(p)))
            stack.enter_context(patch.object(r.checkpoint,'digest',side_effect=lambda p:r.C_SHA if str(p).endswith('ku-server.c') else r.ELF_SHA))
            stack.enter_context(patch.object(r.checkpoint,'prepare',return_value={}))
            stack.enter_context(patch.object(r.database,'Database',return_value=fake_db))
            stack.enter_context(patch.object(r.lh,'Harness',return_value=harness))
            stack.enter_context(patch.object(r,'check_default_maps',return_value={'sha256':r.D_SHA}))
            stack.enter_context(patch.object(r.checkpoint.rs,'read_text',return_value=''))
            stack.enter_context(patch.object(r,'single_json',return_value=(connection,{'requests':1,'correct':True,'raw_response_utf8':'{}'})))
            stack.enter_context(patch.object(r.time,'sleep'))
            stack.enter_context(patch.object(r.lh,'quiet_metrics',return_value=mode!='post-close-not-quiet'))
            stack.enter_context(patch.object(r.signal,'alarm'))
            wait=stack.enter_context(patch.object(r.os,'waitpid',return_value=(3210,0)))
            if mode.startswith('already-reaped'):wait.side_effect=ChildProcessError()
            if mode=='already-reaped-with-residue':fake_db.stop.return_value=dict(forced=True,cgroup_removed=False,after={'process_count':1})
            capture=stack.enter_context(patch.object(r.checkpoint,'capture_owned'))
            capture.return_value={'owners':{'records':[{}]},'page_metadata':{'available':False,'unavailable_reason':'PFNs unavailable'}}
            if mode=='capture-denied':capture.side_effect=PermissionError('owned proc mem denied')
            if mode=='JSON-failed':r.single_json.side_effect=ValueError('JSON result mismatch')
            # The base cgroup path is independent of fixture manifest; record
            # removal using a narrow fake only for that expected path.
            original_rmdir=Path.rmdir
            def remove(path):
                if str(path).startswith('/sys/fs/cgroup/ku-idle-owner-'):return cgroup.rmdir()
                return original_rmdir(path)
            stack.enter_context(patch.object(r.Path,'rmdir',autospec=True,side_effect=remove))
            result=r.run_restored(root)
            service.stop.assert_called_once();fake_db.stop.assert_called_once()
            self.assertEqual(connection.close.call_count,0 if mode=='JSON-failed' else 1)
            self.assertEqual(capture.call_count,0 if mode=='JSON-failed' else 1)
            self.assertEqual(service.metrics.call_count,1 if mode=='JSON-failed' else 2)
            self.assertEqual(result['status'],'captured' if mode in ('ok','already-reaped') else 'unknown')
            self.assertEqual(result['natural_cleanup'],mode!='already-reaped-with-residue');self.assertFalse(cgroup.exists())
            if mode.startswith('already-reaped'):
                self.assertIsNone(result['owned_database_child_reaped'])
                self.assertIsNone(result['database_wait_status'])
                self.assertEqual(result['database_waitpid_state'],'already_reaped_or_not_child')
                self.assertIn('ECHILD alone cannot distinguish',result['database_reap_observation_boundary'])
                self.assertEqual(result['database_child_cleanup_verified'],mode!='already-reaped-with-residue')
            self.assertEqual(result['performance_windows'],0)
    def test_single_capture_and_all_failure_paths_cleanup_without_retry(self):
        for mode in ('ok','capture-denied','JSON-failed','post-close-not-quiet'):
            with self.subTest(mode=mode):self.lifecycle(mode)


if __name__=='__main__':unittest.main()
