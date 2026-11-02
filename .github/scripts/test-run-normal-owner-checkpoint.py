#!/usr/bin/env python3
"""Pure lifecycle/default-library checks; no compiler, service or database."""
from contextlib import ExitStack
import importlib.util
import json
import re
from pathlib import Path
from types import SimpleNamespace
import tempfile
import unittest
from unittest.mock import Mock, patch

spec=importlib.util.spec_from_file_location('owner_run',Path(__file__).with_name('run-normal-owner-checkpoint.py'))
r=importlib.util.module_from_spec(spec);spec.loader.exec_module(r)


class CaptureEvent:
    def __init__(self):self.flag=False;self.wait_seconds=None
    def set(self):self.flag=True
    def is_set(self):return self.flag
    def wait(self,seconds):self.wait_seconds=seconds;return self.flag

class CaptureProcess:
    latest=None
    def __init__(self,target,name):
        self.target=target;self.name=name;self.pid=9876;self.alive=False;self.exitcode=None;self.joined=[];self.closed=False
        CaptureProcess.latest=self
    def start(self):self.alive=True
    def fire(self):
        with patch.object(r.os,'setsid'),patch.object(r.os,'getppid',return_value=r.os.getpid()):self.target()
        self.alive=False;self.exitcode=0
    def join(self,timeout):
        self.joined.append(timeout)
        if self.alive:self.fire()
    def is_alive(self):return self.alive
    def terminate(self):self.alive=False;self.exitcode=-15
    def kill(self):self.alive=False;self.exitcode=-9
    def close(self):self.closed=True

class CaptureContext:
    def __init__(self):self.events=[]
    def Event(self):
        event=CaptureEvent();self.events.append(event);return event
    Process=CaptureProcess

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
    def test_lifecycle_cli_unknown_observation_is_not_a_success_exit(self):
        for status in ('observed','unknown'):
            with self.subTest(status=status),tempfile.TemporaryDirectory() as temp,ExitStack() as stack:
                root=Path(temp)/'run';old=Path(temp)/'old.zip';old.write_bytes(b'fixture')
                for attr,value in (('system','Linux'),('machine','aarch64')):stack.enter_context(patch.object(r.platform,attr,return_value=value))
                stack.enter_context(patch.object(r.os,'geteuid',return_value=0));stack.enter_context(patch.dict(r.os.environ,{},clear=True))
                stack.enter_context(patch.object(r.signal,'signal'));stack.enter_context(patch.object(r.signal,'alarm'))
                stack.enter_context(patch.object(r.restorer,'restore_old',return_value={'run':41280471018}))
                real_digest=r.checkpoint.digest
                stack.enter_context(patch.object(r.checkpoint,'digest',side_effect=lambda p:r.D_SHA if str(p)=='/usr/lib/aarch64-linux-gnu/libpq.so.5' else real_digest(p)))
                run=stack.enter_context(patch.object(r,'run_restored',return_value=dict(status=status,natural_cleanup=True,capture_attempts=3)))
                self.assertEqual(r.main(['--old-zip',str(old),'--root',str(root),'--lifecycle']),0 if status=='observed' else 1)
                run.assert_called_once_with(root.resolve(),lifecycle=True)

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

    def test_PG_lifecycle_uses_one_service_serial_windows_and_no_STOP(self):
        with tempfile.TemporaryDirectory() as temp,ExitStack() as stack:
            service=Mock(address='127.0.0.1:1234');harness=Mock();result={'capture_attempts':0}
            response=json.dumps(dict(id=1,label='item-1-中文🙂',score=17,rid='lifecycle-ready')).encode()
            stack.enter_context(patch.object(r.lh,'request',return_value=(200,response)))
            identity={'pid':1234,'start_ticks':'77'};stages=[]
            def capture(s,d,phase,expected=None,supervisor_pid=None):
                self.assertIs(s,service);self.assertIn(expected,(None,identity));stages.append(phase)
                snapshot=dict(phase=phase,status='observed',identity=identity,monotonic_ns=8_000_000_000,ended_monotonic_ns=8_100_000_000)
                r.save(d/(phase+'.json'),snapshot);return snapshot
            snapshot=stack.enter_context(patch.object(r,'lifecycle_stage',side_effect=capture))
            forbidden=stack.enter_context(patch.object(r.checkpoint,'capture_owned'))
            context=CaptureContext();stack.enter_context(patch.object(r.multiprocessing,'get_context',return_value=context))
            def client(s,workload,rate,duration,output,**kw):
                self.assertIs(s,service);self.assertEqual((workload,rate),('db',200));self.assertEqual(stages,['ready'] if duration==3 else ['ready','warm'])
                self.assertFalse(kw['events']);self.assertFalse(kw['runtime_metrics'])
                if duration==15:CaptureProcess.latest.fire()
                n=rate*duration
                return dict(valid_driver=True,ledger=dict.fromkeys(('planned','attempted','sent','terminal','correct_total'),n),service_cost={'driver_cgroup_removed':True},configuration={'origin_monotonic_ns':500_000_000,'origin_monotonic_status':'available','origin_monotonic_uncertainty_ns':1})
            def complete(*args,**kw):
                v=client(*args,**kw);v['ledger'].update(planned_reconciled=True,results={'correct':200*args[3]});return v
            harness.client.side_effect=complete
            r.pg_lifecycle(harness,service,Path(temp),result)
            self.assertEqual([p['phase'] for p in result['lifecycle_phases']],['init','ready','warm','steady'])
            self.assertEqual(result['lifecycle_phases'][0]['status'],'unknown');self.assertIsNone(result['lifecycle_phases'][0]['monotonic_ns'])
            self.assertEqual([c.args[3] for c in harness.client.call_args_list],[3,15]);self.assertEqual(snapshot.call_count,3)
            self.assertEqual(context.events[0].wait_seconds,7.5);self.assertTrue(context.events[0].is_set());self.assertTrue(CaptureProcess.latest.closed)
            self.assertTrue(result['lifecycle_phases'][-1]['fully_within_arrival_window'])
            forbidden.assert_not_called();service.freeze.assert_not_called();service.stop.assert_not_called()
            for phase in ('warm','steady'):
                report=json.loads((Path(temp)/phase/'client.json').read_text())
                self.assertTrue(report['diagnostic_only']);self.assertFalse(report['normal_score_eligible'])
    def test_failed_PG_window_retains_failure_and_never_starts_steady(self):
        with tempfile.TemporaryDirectory() as temp,patch.object(r.lh,'request',return_value=(200,br'{"id":1,"label":"item-1-\u4e2d\u6587\ud83d\ude42","score":17,"rid":"lifecycle-ready"}')),patch.object(r,'lifecycle_stage',return_value={'phase':'ready','status':'observed','identity':{}}):
            h=Mock();h.client.return_value={'ledger':{}}
            with self.assertRaisesRegex(ValueError,'correctness/accounting/cleanup'):r.pg_lifecycle(h,Mock(),Path(temp),{'capture_attempts':0})
            h.client.assert_called_once()
    def test_steady_early_client_and_collector_failures_cancel_reap_before_return(self):
        for failure in ('client','collector','missing'):
            with self.subTest(failure=failure),tempfile.TemporaryDirectory() as temp,patch.object(r.multiprocessing,'get_context',return_value=CaptureContext()):
                h=Mock();result={}
                def client(*args,**kw):
                    if failure!='missing':CaptureProcess.latest.fire()
                    if failure=='client':raise RuntimeError('client failed early')
                    return {}
                h.client.side_effect=client
                with patch.object(r,'lifecycle_stage',side_effect=PermissionError('collector failed')):
                    with self.assertRaises(RuntimeError):r.steady_pg_window(h,Mock(),Path(temp),{'pid':1234},result)
                self.assertTrue(CaptureProcess.latest.closed);self.assertEqual(CaptureProcess.latest.joined,[10])
                self.assertTrue(result['steady_helper_cleanup']['process_reaped'])

    def test_steady_timeout_terminates_owned_helper_group_and_reaps_before_error(self):
        class DelayedProcess(CaptureProcess):
            def join(self,timeout):
                self.joined.append(timeout)
                if timeout!=10:self.alive=False;self.exitcode=-15
        context=CaptureContext();context.Process=DelayedProcess
        original_event=context.Event
        def event():
            value=original_event()
            if len(context.events)==2:value.set()
            return value
        context.Event=event;h=Mock();h.client.return_value={};result={}
        with tempfile.TemporaryDirectory() as temp,patch.object(r.multiprocessing,'get_context',return_value=context),patch.object(r.os,'killpg') as kill:
            with self.assertRaisesRegex(RuntimeError,'forced cleanup'):r.steady_pg_window(h,Mock(),Path(temp),{},result)
            self.assertEqual(kill.call_args_list,[unittest.mock.call(9876,r.signal.SIGTERM),unittest.mock.call(9876,r.signal.SIGKILL)])
            self.assertTrue(result['steady_helper_cleanup']['forced']);self.assertTrue(result['steady_helper_cleanup']['process_reaped'])
            self.assertTrue(CaptureProcess.latest.closed)
    def test_helper_rejects_a_different_supervisor_without_polling_nonchild_service(self):
        with tempfile.TemporaryDirectory() as temp,patch.object(r.os,'getppid',return_value=5):
            service=Mock()
            with self.assertRaisesRegex(ValueError,'helper supervisor'):r.lifecycle_stage(service,Path(temp),'steady',supervisor_pid=4)
            service.process.poll.assert_not_called()

    def test_actual_post_drain_endpoint_never_fabricates_live_smaps(self):
        with tempfile.TemporaryDirectory() as temp,patch.object(r.checkpoint,'check_identity') as read:
            service=Mock();service.process.poll.return_value=0
            cleanup=dict(forced=False,cgroup_removed=True,exit_code=0,drain={'status':200},after={'process_count':0,'threads':0})
            stage=r.lifecycle_stage(service,Path(temp),'drain',{'pid':1234},cleanup)
            self.assertEqual(stage['status'],'observed-termination');self.assertFalse(stage['smaps']['available'])
            self.assertEqual(stage['cgroups']['service'],cleanup['after']);read.assert_not_called()

    def test_live_stage_is_bounded_and_keeps_denied_smaps_unknown(self):
        for denied in (False,True,'alarm'):
            with self.subTest(denied=denied),tempfile.TemporaryDirectory() as temp,ExitStack() as stack:
                service=Mock();service.process.pid=1234;service.process.poll.return_value=None
                service.harness.db_domain=None;service.domain.path=Path(temp)/'owned-cgroup'
                real_stat=Path.stat;real_iter=Path.iterdir;real_digest=r.checkpoint.digest
                stack.enter_context(patch.object(r.Path,'stat',autospec=True,side_effect=lambda p,*a,**kw:SimpleNamespace(st_ino=7,st_dev=8) if str(p)=='/proc/1234/exe' else real_stat(p,*a,**kw)))
                stack.enter_context(patch.object(r.Path,'iterdir',autospec=True,side_effect=lambda p:iter([p/'1234']) if str(p)=='/proc/1234/task' else real_iter(p)))
                stack.enter_context(patch.object(r.checkpoint,'read_start',return_value='77'))
                identity=stack.enter_context(patch.object(r.checkpoint,'check_identity'))
                stack.enter_context(patch.object(r.checkpoint,'digest',side_effect=lambda p:r.ELF_SHA if str(p)=='/proc/1234/exe' else real_digest(p)))
                def read(path,limit):
                    if denied=='alarm' and path.name=='smaps':raise TimeoutError('global execution bound')
                    if denied and path.name=='smaps':raise PermissionError('denied fixture')
                    return 'bounded fixture'
                def command(argv,**kw):
                    self.assertLessEqual(kw['timeout'],2);return SimpleNamespace(stdout=read(Path(argv[-1]),int(argv[2])-1).encode())
                reads=stack.enter_context(patch.object(r.subprocess,'run',side_effect=command))
                if denied=='alarm':
                    with self.assertRaisesRegex(TimeoutError,'global execution'):r.lifecycle_stage(service,Path(temp),'ready')
                    continue
                stage=r.lifecycle_stage(service,Path(temp),'ready')
                self.assertEqual(stage['status'],'unknown' if denied else 'observed');self.assertEqual(len(stage['threads']),1)
                self.assertEqual(identity.call_count,2);self.assertFalse(stage['STOP_or_freeze_sent'])
                self.assertTrue(all(int(c.args[0][2])<=1+(2<<20) for c in reads.call_args_list))
                with self.assertRaisesRegex(ValueError,'identity changed'):r.lifecycle_stage(service,Path(temp),'warm',{'pid':0})
    def test_unreaped_helper_defers_all_owned_cleanup_and_records_unknown(self):
        self.lifecycle('five-unreaped')

    def test_lifecycle_mode_skips_idle_capture_and_failure_still_finalizes(self):
        for mode in ('five-ok','five-failed'):
            with self.subTest(mode=mode):self.lifecycle(mode)

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
            service=Mock();service.process.pid=1234;service.domain.path=cgroup/'service';service.metrics.return_value={'queued':0};service.stop.return_value=dict(forced=False,cgroup_removed=True,after={'process_count':0},exit_code=0)
            harness=Mock();harness.start.return_value=service;harness.close.return_value=[]
            connection=Mock()
            stack.enter_context(patch.object(r,'save',side_effect=save))
            stack.enter_context(patch.object(r.json,'loads',side_effect=loads))
            original_exists=Path.exists
            stack.enter_context(patch.object(r.Path,'exists',autospec=True,side_effect=lambda p:original_exists(cgroup) if str(p).startswith('/sys/fs/cgroup/ku-idle-owner-') else original_exists(p)))
            stack.enter_context(patch.object(r.checkpoint,'digest',side_effect=lambda p:r.C_SHA if str(p).endswith('ku-server.c') else r.ELF_SHA))
            prepare=stack.enter_context(patch.object(r.checkpoint,'prepare',return_value={}))
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
            five=mode.startswith('five-')
            if five:
                def lifecycle(h,s,e,result):
                    self.assertIs(h,harness);self.assertIs(s,service)
                    result.update(lifecycle_phases=[{'phase':'init','status':'unknown'}],status='observed')
                    if mode=='five-failed':raise ValueError('driver fixture failed')
                    if mode=='five-unreaped':
                        result['steady_helper_cleanup']={'pid':9876,'process_group':9876,'process_reaped':False,'forced':True}
                        raise RuntimeError('helper could not be reaped')
                stack.enter_context(patch.object(r,'pg_lifecycle',side_effect=lifecycle))
                stack.enter_context(patch.object(r,'lifecycle_stage',return_value={'phase':'drain','status':'observed-termination'}))
            result=r.run_restored(root,lifecycle=five)
            harness.start.assert_called_once()
            if mode=='five-unreaped':
                service.metrics.assert_not_called();service.stop.assert_not_called();harness.close.assert_not_called();fake_db.stop.assert_not_called();wait.assert_not_called()
                self.assertFalse(result['natural_cleanup']);self.assertEqual(result['status'],'unknown');self.assertFalse(result['observer_reaped'])
                self.assertEqual(result['owned_not_confirmed_reclaimed']['service'],{'pid':1234,'cgroup':str(cgroup/'service')})
                self.assertEqual(result['owned_not_confirmed_reclaimed']['database']['pid'],3210);self.assertTrue(cgroup.exists())
                self.assertIn('runner teardown',result['cleanup_reason']);return
            harness.close.assert_called_once()
            self.assertEqual(prepare.call_count,0 if five else 1)
            service.stop.assert_called_once();fake_db.stop.assert_called_once()
            self.assertEqual(connection.close.call_count,0 if five or mode=='JSON-failed' else 1)
            self.assertEqual(capture.call_count,0 if five or mode=='JSON-failed' else 1)
            self.assertEqual(service.metrics.call_count,1 if five or mode=='JSON-failed' else 2)
            self.assertEqual(result['status'],'observed' if mode=='five-ok' else 'captured' if mode in ('ok','already-reaped') else 'unknown')
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
