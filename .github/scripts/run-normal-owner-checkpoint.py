#!/usr/bin/env python3
"""Bounded idle or five-stage PG lifecycle observation of the unchanged ELF.

Restore the fixed build, never a rejected library candidate. Lifecycle mode
uses only the existing serial driver and never STOP/freeze or a formal score.
"""
import argparse
import copy
import hashlib
import http.client
import importlib.util
import json
import os
from pathlib import Path
import platform
import shutil
import signal
import sys
import subprocess
import multiprocessing
import time

REPO = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(REPO / 'tests/benchmarks/cross_stack'))
import normal_owner_checkpoint as checkpoint
import database
import linux_harness as lh
import hosted_cycle as hosted

spec = importlib.util.spec_from_file_location('fixed_old_restore', Path(__file__).with_name('prepare-libpq-loader-memory.py'))
restorer = importlib.util.module_from_spec(spec); spec.loader.exec_module(restorer)
SOURCE = '5b6b41138cce4086c19cc7daa24e820c9b6513d8'
ELF_SHA = '234300bba9b6fce603a178701ac289a82108398d5f35de3c0a7ad585f082367b'
C_SHA = '830536089f72ce56118e4e6a1556bbe007f816cc16a3c2f96c0dcff010d14e77'
D_SHA = 'b59de287c314a78cb70e7e0bb31fb2be34b961ccc90beff405d8c48412ace2df'
C_REL = 'B/build/ku-native/.ku/build/release/c/CPkLjMo5cOr7gaP-eyr5YRGcrGvMEuNt83h2b7ezDEs/ku-server.c'
CC_REL = 'B/build/logs/1793514266618607938/native-compiler-server/compiler.json'


def save(path, value):
    Path(path).write_text(json.dumps(value, ensure_ascii=False, indent=2) + '\n')


def alarm(signum, frame):
    raise TimeoutError('independent normal-owner checkpoint finite execution budget')


def check_default_maps(raw):
    if len(raw) > 1 << 20:
        raise ValueError('default maps exceeded1MiB')
    maps = checkpoint.rs.maps_parse(raw.decode())
    paths = sorted({m['path'] for m in maps if m['path'].startswith('/') and Path(m['path']).name.startswith('libpq.so')})
    if len(paths) != 1 or checkpoint.digest(paths[0]) != D_SHA:
        raise ValueError('actual process did not map the unchanged default D library')
    return dict(actual_default_D_library=paths[0], sha256=D_SHA,
                scope='Actual mapped file hash, not merely a link flag or environment value')


def distinct_object(pairs):
    result = {}
    for key, value in pairs:
        if key in result:
            raise ValueError('duplicate key in JSON response')
        result[key] = value
    return result


def single_json(service, corpus, destination=None):
    body = Path(corpus).read_bytes()
    if checkpoint.digest(corpus) != '548a8397f835c17ee8b524054e1da90d00d9293965986c5e48be5692663f5e3a':
        raise ValueError('frozen ordinary JSON input changed')
    host, port = service.address.rsplit(':', 1)
    connection = http.client.HTTPConnection(host, int(port), timeout=3)
    try:
        connection.request('POST', '/json?rid=owner-checkpoint-1', body=body,
                           headers={'Content-Type': 'application/json', 'Connection': 'keep-alive'})
        response = connection.getresponse(); data = response.read((1 << 20) + 1)
        if destination is not None:
            destination=Path(destination)
            (destination / 'ordinary-JSON-response.log').write_bytes(data)
            save(destination / 'ordinary-JSON-response-readback.json', dict(status=response.status,
                bytes_read=len(data),body_SHA256=hashlib.sha256(data).hexdigest(),
                truncated_at_bound=len(data)>1<<20,will_close=response.will_close,requests=1))
        original = json.loads(body, object_pairs_hook=distinct_object)
        wanted = {'id': 'owner-checkpoint-1', 'counter': original['counter'] + 1, 'data': original['data']}
        received = json.loads(data, object_pairs_hook=distinct_object)
        canonical = lambda value: json.dumps(value, sort_keys=True, ensure_ascii=False, separators=(',', ':'))
        if response.status != 200 or len(data) > 1 << 20 or canonical(received) != canonical(wanted):
            raise ValueError('one ordinary JSON response failed correctness')
        if response.will_close or connection.sock is None:
            raise ValueError('the single ordinary response did not preserve its idle keepalive')
        return connection, dict(status=200, input_bytes=len(body), response_bytes=len(data),
            response_SHA256=hashlib.sha256(data).hexdigest(), raw_response_utf8=data.decode('utf-8'), correct=True, requests=1,
            keepalive_open=True, elapsed_not_a_performance_score=True)
    except BaseException:
        connection.close(); raise


def natural(cleanup, service=False):
    return (cleanup.get('forced') is False and cleanup.get('cgroup_removed') is True
            and cleanup.get('after', {}).get('process_count') == 0
            and (not service or cleanup.get('exit_code') == 0))



def lifecycle_stage(service, destination, phase, expected=None, cleanup=None, supervisor_pid=None):
    """One live endpoint, never STOP/freeze; sequential reads are not atomic."""
    destination=Path(destination);destination.mkdir(parents=True,exist_ok=True)
    record=dict(phase=phase,diagnostic_only=True,score_eligible=False,status='unknown',
        monotonic_ns=time.monotonic_ns(),wall_time_ns=time.time_ns(),identity=expected,
        STOP_or_freeze_sent=False,smaps={'available':False},cgroups={},threads=[])
    deadline=time.monotonic()+6
    def read(path,limit):
        remaining=deadline-time.monotonic()
        if remaining<=0:raise TimeoutError('lifecycle endpoint6s read budget')
        command=['/usr/bin/head','-c',str(limit+1),str(path)]
        raw=subprocess.run(command,stdout=subprocess.PIPE,stderr=subprocess.PIPE,check=True,timeout=min(2,remaining)).stdout
        if len(raw)>limit:raise RuntimeError('lifecycle file exceeds bounded read: '+str(path))
        return raw.decode()
    try:
        if cleanup is not None:
            exited=service.process.poll() is not None
            record.update(identity=dict(expected or {},exit_code=cleanup.get('exit_code'),exited=exited),
                status='observed-termination' if exited and natural(cleanup,service=True) and cleanup.get('drain',{}).get('status')==200 else 'unknown',drain=cleanup.get('drain'),
                cgroups={'service':cleanup.get('after',{})},smaps={'available':False,'reason':'same service exited; smaps unavailable' if exited else 'drain failed or exit unconfirmed; not labelled as a post-exit smaps'},
                scope='After actual drain/termination; cgroup values are the retained termination snapshot, not a live removed-cgroup read.')
            return record
        if supervisor_pid is not None and os.getppid()!=supervisor_pid:raise ValueError('snapshot helper supervisor changed')
        if supervisor_pid is None and service.process.poll() is not None:raise RuntimeError('owned service exited before stage')
        proc=Path('/proc')/str(service.process.pid);exe=(proc/'exe').stat()
        identity=dict(pid=service.process.pid,start_ticks=checkpoint.read_start(proc),exe_inode=exe.st_ino,exe_device=exe.st_dev,ELF_SHA256=checkpoint.digest(proc/'exe'))
        if expected is not None and identity!=expected:raise ValueError('lifecycle process identity changed')
        if identity['ELF_SHA256']!=ELF_SHA:raise ValueError('lifecycle normal executable differs')
        def verify_owner():
            if supervisor_pid is None:return checkpoint.check_identity(proc,None,identity['start_ticks'],(exe.st_ino,exe.st_dev))
            status=read(proc/'status',65536);parent=int(next(line.split()[1] for line in status.splitlines() if line.startswith('PPid:')))
            current=(proc/'exe').stat()
            if parent!=supervisor_pid or os.getppid()!=supervisor_pid or checkpoint.read_start(proc)!=identity['start_ticks'] or (current.st_ino,current.st_dev)!=(exe.st_ino,exe.st_dev):raise ValueError('owned service/helper identity changed')
        verify_owner()
        record['identity']=identity
        for name,limit in (('smaps',2<<20),('maps',256<<10),('status',65536),('stat',65536)):
            try:
                raw=read(proc/name,limit)
                if name=='smaps':
                    path=destination/(phase+'-smaps.log');path.write_text(raw)
                    record['smaps']=dict(available=True,path=str(path),bytes=len(raw.encode()),sha256=checkpoint.digest(path),limit_bytes=limit)
                else:record['proc_'+name]=raw
            except TimeoutError:raise
            except (OSError,RuntimeError) as error:record['smaps' if name=='smaps' else 'proc_'+name]={'available':False,'reason':repr(error)[:256]}
        tasks=sorted(proc.joinpath('task').iterdir(),key=lambda task:int(task.name))
        if len(tasks)>128:raise ValueError('lifecycle thread identity bound exceeded128')
        for task in tasks:record['threads'].append(dict(tid=int(task.name),stat=read(task/'stat',4096)))
        for label,domain in [('service',service.domain),*([('database',service.harness.db_domain)] if service.harness.db_domain else [])]:
            values={}
            for name in ('memory.current','memory.peak','memory.stat','memory.events','cpu.stat','pids.current','cgroup.procs'):
                values[name]=read(domain.path/name,16384)
            record['cgroups'][label]=dict(path=str(domain.path),values=values)
        verify_owner()
        record['status']='observed' if record['smaps'].get('available') and all(isinstance(record.get('proc_'+name),str) for name in ('maps','status','stat')) else 'unknown'
        return record
    except (OSError,ValueError,RuntimeError) as error:
        record['error']=repr(error)[:512];raise
    finally:
        record['ended_monotonic_ns']=time.monotonic_ns()
        save(destination/(phase+'.json'),record)



def steady_pg_window(harness, service, evidence, identity, result):
    """One owned Linux helper; reap it and its group before service cleanup."""
    (evidence/'steady').mkdir(parents=True,exist_ok=True)
    context=multiprocessing.get_context('fork');cancel=context.Event();session=context.Event();supervisor=os.getpid()
    receipt=evidence/'steady-helper.json'
    def collect():
        try:
            if os.getppid()!=supervisor:raise ValueError('snapshot helper is not an owned child')
            os.setsid();session.set()
            if cancel.wait(7.5):save(receipt,{'cancelled_before_capture':True});return
            lifecycle_stage(service,evidence,'steady',identity,supervisor_pid=supervisor)
            save(receipt,{'captured':True})
        except BaseException as error:save(receipt,{'error':repr(error)[:1024]})
    helper=context.Process(target=collect,name='ku-one-steady-snapshot');helper.start();forced=False
    try:
        report=harness.client(service,'db',200,15,evidence/'steady',events=False,runtime_metrics=False)
        report.update(diagnostic_only=True,normal_score_eligible=False,score_eligible=False);save(evidence/'steady/client.json',report)
    finally:
        cancel.set();join_error=None
        try:helper.join(timeout=10)
        except BaseException as error:join_error=error
        if helper.is_alive():
            forced=True
            try:
                if session.is_set():os.killpg(helper.pid,signal.SIGTERM)
                else:helper.terminate()
            except ProcessLookupError:pass
            helper.join(timeout=2)
            try:
                if session.is_set():os.killpg(helper.pid,signal.SIGKILL)
                elif helper.is_alive():helper.kill()
            except ProcessLookupError:pass
            helper.join(timeout=2)
        reaped=not helper.is_alive()
        result['steady_helper_cleanup']=dict(pid=helper.pid,process_group=helper.pid if session.is_set() else None,process_reaped=reaped,forced=forced,exit_code=helper.exitcode,
            supervisor_pid=supervisor,nominal_delay_seconds=7.5,read_budget_seconds=6,join_seconds=10)
        if not reaped:raise RuntimeError('owned snapshot helper could not be reaped; no service cleanup permitted')
        helper.close()
        if join_error is not None:raise join_error
    if forced or result['steady_helper_cleanup']['exit_code']!=0:raise RuntimeError('steady helper required forced cleanup or failed')
    observation=json.loads(checkpoint.rs.read_text(receipt,65536))
    if observation.get('error'):raise RuntimeError('steady lifecycle observation failed: '+observation['error'])
    if observation.get('captured') is not True:raise RuntimeError('steady lifecycle observation did not run exactly once')
    snapshot=json.loads(checkpoint.rs.read_text(evidence/'steady.json',2<<20));clock=lh.measurement_clock(report.get('configuration',{}))
    if not clock['available']:raise ValueError('steady driver monotonic arrival origin unavailable')
    start=clock['origin_ns'];end=start+15_000_000_000
    snapshot.update(nominal_delay_seconds=7.5,timer_anchor='owned helper starts before steady harness.client; driver startup is included',
        capture_offset_seconds=(snapshot['monotonic_ns']-start)/1e9,
        fully_within_arrival_window=start<=snapshot['monotonic_ns']<=snapshot['ended_monotonic_ns']<=end,
        scope='One non-STOP mid-load diagnostic snapshot; observer overhead is included and no performance score is claimed.')
    save(evidence/'steady.json',snapshot)
    if not snapshot['fully_within_arrival_window']:raise ValueError('steady capture outside actual driver arrival interval')
    return report,snapshot


def pg_lifecycle(harness, service, evidence, result):
    phases=result['lifecycle_phases']=[dict(phase='init',status='unknown',monotonic_ns=None,diagnostic_only=True,score_eligible=False,
        reason='Unmodified ELF has no external init-complete hook; Harness.start returns only after HTTP readiness, not initialization.')]
    save(evidence/'init.json',phases[0])
    status,body=lh.request(service.address,'/db?key=1&rid=lifecycle-ready',timeout=3)
    actual=json.loads(body,object_pairs_hook=distinct_object)
    wanted=dict(id=1,label='item-1-中文🙂',score=17,rid='lifecycle-ready')
    canonical=lambda value:json.dumps(value,ensure_ascii=False,sort_keys=True,separators=(',',':'))
    if status!=200 or canonical(actual)!=canonical(wanted):raise ValueError('real PG/HTTP readiness failed')
    result['PG_HTTP_readiness']=dict(status=status,correct=True,requests=1)
    identity=None
    for phase,seconds in (('ready',0),('warm',3),('steady',15)):
        if seconds:
            (evidence/phase).mkdir(parents=True,exist_ok=True)
            if phase=='steady':report,snapshot=steady_pg_window(harness,service,evidence,identity,result)
            else:report=harness.client(service,'db',200,seconds,evidence/phase,events=False,runtime_metrics=False)
            report.update(diagnostic_only=True,normal_score_eligible=False,score_eligible=False)
            save(evidence/phase/'client.json',report)
            ledger=report['ledger'];planned=200*seconds
            if (report.get('valid_driver') is not True or any(ledger.get(key)!=planned for key in ('planned','attempted','sent','terminal','correct_total'))
                    or ledger.get('planned_reconciled') is not True or ledger.get('results')!={'correct':planned}
                    or report.get('service_cost',{}).get('driver_cgroup_removed') is not True):
                raise ValueError('diagnostic PG window correctness/accounting/cleanup failed: '+phase)
            result.setdefault('diagnostic_windows',[]).append(dict(phase=phase,rate=200,seconds=seconds,correct=planned,score_eligible=False))
        if phase!='steady':snapshot=lifecycle_stage(service,evidence,phase,identity)
        identity=snapshot['identity'];phases.append(snapshot)
        result['capture_attempts']+=1
    result['status']='observed' if all(p['status']=='observed' for p in phases[1:]) else 'unknown'
    result['lifecycle_boundary']='Ready and post-warm endpoints plus one live mid-load steady snapshot; no STOP, no formal score or performance conclusion.'


def run_restored(root, lifecycle=False):
    original = root / 'original'; evidence = root / 'evidence'
    base = json.loads((original / 'B/manifest.json').read_text().replace('${EVIDENCE_ROOT}', str(original)))
    if base['candidate_base'] != SOURCE or checkpoint.digest(original / C_REL) != C_SHA or checkpoint.digest(original / 'B/build/bin/ku-server') != ELF_SHA:
        raise ValueError('fixed ordinary ELF/C/product identity differs')
    proof = json.loads((REPO / '.github/fixtures/libpq-loader-memory/current-default-C-proof.json').read_text())
    if proof.get('byte_equal') is not True or proof['current']['sha256'] != C_SHA or proof['old_source_commit'] != SOURCE:
        raise ValueError('current complete-C equivalence proof absent')
    # Build only the isolated offsetof helper, using original C and compiler.
    if not lifecycle:
        checkpoint.prepare(original / 'B/build/bin/ku-server', original / C_REL,
            original / 'B/manifest.json', original / CC_REL, '/usr/bin/clang-18',
            original / 'B/build/libpq/libpq.so', SOURCE, root / 'layout')
    save(evidence / 'current-default-C-proof.json', proof)
    fixture_root, service_root = root / 'fixture', root / 'service-input'
    fixture_root.mkdir(); service_root.mkdir()
    base['cgroup_root'] = '/sys/fs/cgroup/ku-idle-owner-' + hashlib.sha256(str(root).encode()).hexdigest()[:12]
    if Path(base['cgroup_root']).exists():
        raise ValueError('fresh checkpoint cgroup path already exists; no domain reused')
    base['database_cpu_set'] = '3'
    base['stacks'] = {'ku': base['stacks']['ku']}
    if base['stacks']['ku'].get('env'):
        raise ValueError('original ordinary service unexpectedly has a runtime library override')
    base['stacks']['ku']['build']['libpq_source'] = str(original / 'B/build/libpq/libpq.so')
    fixture_manifest = copy.deepcopy(base); fixture_manifest['stacks'] = {}
    save(fixture_root / 'manifest.json', fixture_manifest)
    fixture, service, keepalive, harness = None, None, None, None
    result = dict(schema='ku-one-normal-owner-checkpoint-v1', diagnostic_only=True,
        score_eligible=False, performance_windows=0, automatic_retry=False,
        original_product=SOURCE, normal_ELF_SHA256=ELF_SHA, normal_C_SHA256=C_SHA,
        default_libpq_SHA256=D_SHA, status='unknown', capture_attempts=0)
    began = time.monotonic()
    try:
        fixture = database.Database(fixture_root, base['uid'], base['gid'])
        result['database_start'] = fixture.start()
        result['database_child_provenance'] = dict(source='database.Database.start -> Harness.launch direct Popen',
            pid=result['database_start'].get('pid'), cgroup=result['database_start'].get('cgroup'),
            validation='Database.start validates the owned marker, executable, data directory and cgroup before returning; no waitpid observation is implied.')
        private = json.loads(fixture.manifest_path.read_text())
        base.update(db_url=private['db_url'], db_cgroup=private['db_cgroup'])
        save(service_root / 'manifest.json', base)
        harness = lh.Harness(service_root, root / 'service')
        service = harness.start('ku', cores=2, memory=lh.GIB, output=root / 'service/one')
        maps = checkpoint.rs.read_text(Path('/proc') / str(service.process.pid) / 'maps', 1 << 20).encode()
        result['runtime_default'] = check_default_maps(maps)
        (evidence / 'runtime-default.maps').write_bytes(maps)
        if lifecycle:
            result.update(schema='ku-normal-PG-five-stage-lifecycle-v1',alarm_seconds=120,cleanup_alarm_seconds=30)
            pg_lifecycle(harness,service,evidence,result)
            return result
        keepalive, result['single_correct_response'] = single_json(service,
            original / 'Driver/src/tests/benchmarks/cross_stack/corpora/object.json', evidence)
        result['single_correct_response'].pop('raw_response_utf8',None)
        # Exactly one management read, then no further producer or management
        # request until after the independent stopped checkpoint has resumed.
        save(evidence / 'pre-capture-metrics.json', hosted.redact(service.metrics(), root))
        time.sleep(.1)
        result['capture_attempts'] = 1
        try:
            captured = checkpoint.capture_owned(service.process, root / 'layout/prepared.json',
                evidence / 'checkpoint', dict.fromkeys(checkpoint.REQUIRED_BOUNDARY, True))
            result['status'] = 'captured'
            result['owner_ranges'] = len(captured['owners']['records'])
            result['page_metadata_available'] = captured['page_metadata'].get('available') is True
            if not result['page_metadata_available']:
                result['page_classification'] = 'unknown: ' + str(captured['page_metadata'].get('unavailable_reason'))
        except (OSError, ValueError, RuntimeError, TimeoutError) as error:
            result['capture_error'] = repr(error)[:1024]
            result['status'] = 'unknown'
    except (OSError, ValueError, RuntimeError, TimeoutError) as error:
        result['operation_error'] = repr(error)[:1024]
        result['status'] = 'unknown'
    finally:
        observer=result.get('steady_helper_cleanup',{})
        if lifecycle and observer.get('process_reaped') is False:
            result.update(status='unknown',natural_cleanup=False,observer_reaped=False,cleanup_deferred=True,
                cleanup_reason='Snapshot helper could not be reaped after KILL; no PID/cgroup cleanup or evidence packaging may race it. Disposable runner teardown required.',
                owned_not_confirmed_reclaimed=dict(observer=observer,
                    service={'pid':service.process.pid,'cgroup':str(service.domain.path)} if service else None,
                    database={'pid':result.get('database_start',{}).get('pid'),'cgroup':base.get('db_cgroup')}))
            save(evidence/'summary.json',hosted.redact(result,root))
            return result
        signal.alarm(30)
        try:
            if keepalive is not None:
                keepalive.close()
            if service is not None:
                try:
                    time.sleep(.1)
                    final_metrics=service.metrics()
                    save(evidence / 'post-close-metrics.json', hosted.redact(final_metrics, root))
                    result['post_close_quiet'] = lh.quiet_metrics('ku', final_metrics)
                except Exception as error:
                    result['post_close_quiet'] = False
                    result['post_close_metrics_error'] = repr(error)[:512]
            if service is not None:
                try: result['service_cleanup'] = service.stop()
                except Exception as error: result['service_cleanup'] = {'error': repr(error)[:1024]}
            if lifecycle and service is not None and result.get('lifecycle_phases'):
                try:
                    stage=lifecycle_stage(service,evidence,'drain',result['lifecycle_phases'][-1].get('identity'),result.get('service_cleanup',{}));result['lifecycle_phases'].append(stage)
                    if stage['status']=='unknown':result['status']='unknown'
                except Exception as error: result.update(status='unknown',drain_observation_error=repr(error)[:512])
            if harness is not None:
                try: result['owned_harness_finalizer']=harness.close()
                except Exception as error: result['owned_harness_finalizer']=[{'error':repr(error)[:512]}]
            if fixture is not None:
                try: result['database_cleanup'] = fixture.stop()
                except Exception as error: result['database_cleanup'] = {'error': repr(error)[:1024]}
            db_pid=result.get('database_start',{}).get('pid')
            if type(db_pid) is int:
                try:
                    reaped,status=os.waitpid(db_pid,os.WNOHANG)
                    result['owned_database_child_reaped'] = reaped==db_pid
                    result['database_waitpid_state'] = 'reaped_by_checkpoint' if reaped==db_pid else 'not_yet_reaped'
                    result['database_wait_status'] = status if reaped==db_pid else None
                except ChildProcessError:
                    result['owned_database_child_reaped'] = None
                    result['database_waitpid_state'] = 'already_reaped_or_not_child'
                    result['database_wait_status'] = None
                    result['database_reap_observation_boundary'] = ('ECHILD alone cannot distinguish prior reaping from not-a-child. '
                        'Only the recorded self-created fixture provenance plus zero processes and removed owned cgroup can establish no remaining fixture process; exact reap/exit status remains unknown.')
                except OSError as error:
                    result['owned_database_child_reaped'] = False
                    result['database_waitpid_state'] = 'waitpid_error'
                    result['database_reap_error'] = repr(error)[:256]
            owned_parent = Path(base['cgroup_root'])
            if owned_parent.exists():
                try:
                    owned_parent.rmdir(); result['empty_parent_cgroup_removed'] = True
                except OSError as error:
                    result['empty_parent_cgroup_removed'] = False
                    result['cgroup_parent_error'] = repr(error)[:256]
        finally:
            signal.alarm(0)
        provenance=result.get('database_child_provenance',{})
        result['database_child_cleanup_verified'] = (type(db_pid) is int and provenance.get('pid')==db_pid
            and provenance.get('source')=='database.Database.start -> Harness.launch direct Popen'
            and natural(result.get('database_cleanup',{}))
            and result.get('database_waitpid_state') in ('reaped_by_checkpoint','already_reaped_or_not_child'))
        result['natural_cleanup'] = (natural(result.get('service_cleanup', {}), service=True)
            and natural(result.get('database_cleanup', {})) and result.get('empty_parent_cgroup_removed') is True
            and result.get('database_child_cleanup_verified') is True
            and not any(x.get('forced_owned_cleanup') or x.get('error') for x in result.get('owned_harness_finalizer',[])))
        if result.get('post_close_quiet') is not True:
            result['status'] = 'unknown'
        if not result['natural_cleanup']:
            result['status'] = 'unknown'
        result['elapsed_seconds'] = time.monotonic() - began
        save(evidence / 'summary.json', hosted.redact(result, root))
        # Keep identities/layout/raw checkpoint, not temporary executables,
        # restored CLI/driver or the disposable PostgreSQL data directory.
        for filename in ('prepared.json', 'layout.json', 'known-static-symbols.json', 'layout-build.stdout', 'layout-build.stderr', 'layout.stdout', 'layout.stderr', 'normal-global-symbol.readback'):
            path = root / 'layout' / filename
            if path.exists(): shutil.copy2(path, evidence / filename)
        for path in (root / 'layout/layout-only', root / 'layout/layout-only.c',
                     original / 'B/build/ku-cargo', original / 'Driver/build'):
            if path.is_dir(): shutil.rmtree(path)
            elif path.exists(): path.unlink()
        if natural(result.get('database_cleanup', {})):
            for path in (fixture_root / 'database/data', fixture_root / 'database/socket'):
                if path.exists(): shutil.rmtree(path)
    return result


def evidence_manifest(directory):
    files=[]; total=0
    for path in sorted(Path(directory).rglob('*')):
        if path.is_symlink():raise ValueError('unsupported evidence symlink')
        if path.name=='manifest.json' or path.is_dir():continue
        if not path.is_file():raise ValueError('unsupported evidence member')
        size=path.stat().st_size; total+=size
        if size>5<<20 or total>16<<20 or len(files)>=64:raise ValueError('bounded evidence exceeds allowlist limits')
        files.append(dict(path=str(path.relative_to(directory)),bytes=size,sha256=checkpoint.digest(path)))
    save(Path(directory)/'manifest.json',dict(schema='ku-normal-owner-export-v1',files=files,total_bytes=total,
        manifest_self_excluded=True,normal_ELF_or_C_rewritten=False,original_archive_included=False,
        scope='All selected raw checkpoint files are exported directly, not filtered by generic performance archive names.'))


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--old-zip', type=Path, required=True); parser.add_argument('--root', type=Path, required=True)
    parser.add_argument('--lifecycle',action='store_true',help='Five diagnostic PG lifecycle boundaries; no idle STOP checkpoint or layout build')
    args = parser.parse_args(argv)
    if platform.system() != 'Linux' or platform.machine() != 'aarch64' or os.geteuid() != 0:
        raise ValueError('existing isolated Linux ARM64 root harness required')
    if any(os.environ.get(k) for k in ('LD_PRELOAD', 'LD_AUDIT', 'LD_LIBRARY_PATH', 'GNUTLS_SKIP_GLOBAL_INIT')):
        raise ValueError('ordinary checkpoint cannot inherit a library override/interposer')
    root = args.root.resolve(); root.mkdir(exist_ok=False); (root / 'evidence').mkdir()
    old_alarm = signal.signal(signal.SIGALRM, alarm); old_term = signal.signal(signal.SIGTERM, alarm)
    signal.alarm(120 if args.lifecycle else 85)
    began = time.monotonic(); result = None
    try:
        restoration = restorer.restore_old(args.old_zip, root / 'original',
            REPO / '.github/fixtures/libpq-loader-memory/original-allowlist.json')
        save(root / 'evidence/restoration.json', restoration)
        if checkpoint.digest('/usr/lib/aarch64-linux-gnu/libpq.so.5') != D_SHA:
            raise ValueError('system default D libpq changed; no service started')
        result = run_restored(root,lifecycle=True) if args.lifecycle else run_restored(root)
        print(json.dumps(dict(status=result['status'], natural_cleanup=result['natural_cleanup'],
                              capture_attempts=result['capture_attempts']), sort_keys=True))
        return 0 if result['natural_cleanup'] and (not args.lifecycle or result['status']=='observed') else 1
    except (OSError, ValueError, RuntimeError, TimeoutError) as error:
        save(root / 'evidence/failure.json', dict(status='unknown', error=repr(error)[:1024],
             diagnostic_only=True, score_eligible=False, elapsed_seconds=time.monotonic()-began))
        return 1
    finally:
        signal.alarm(0); signal.signal(signal.SIGALRM, old_alarm); signal.signal(signal.SIGTERM, old_term)
        if not (isinstance(result,dict) and result.get('observer_reaped') is False):
            for name in ('layout-build.stdout', 'layout-build.stderr', 'layout.stdout', 'layout.stderr', 'normal-global-symbol.readback', 'known-static-symbols.json'):
                path=root/'layout'/name
                if path.exists(): shutil.copy2(path, root/'evidence'/name)
            for name in ('layout-only', 'layout-only.c'):
                (root/'layout'/name).unlink(missing_ok=True)
            args.old_zip.unlink(missing_ok=True)
            (root / 'original-evidence.tar.gz').unlink(missing_ok=True)
            evidence_manifest(root / 'evidence')


if __name__ == '__main__':
    raise SystemExit(main())
