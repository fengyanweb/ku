"""One private derived-C PG owner-residency observation, never a score."""
import argparse, copy, json, os, platform, sys, time, types
from pathlib import Path
import database as db
import hosted_cycle as h
import http_cost_diagnostic as cost
import linux_harness as lh
import reuse_build as reuse

REPO=Path(__file__).resolve().parents[3]
def plan(versions):
    return dict(schema='ku-owner-PG-lifecycle-v1',versions=versions,diagnostic_only=True,
        normal_score_eligible=False,formal_qualified=False,workload='db',rate=200,
        warm_seconds=10,steady_seconds=15,stages=['ready','warm','steady'],maximum_replacements=0,
        source_run=reuse.OK_PMU_RUN,archive_sha256=reuse.OK_PMU_ARCHIVE_SHA,
        normal_C_sha256=h.NORMAL_PG_C_SHA,normal_ELF_sha256=h.NORMAL_PG_ELF_SHA,
        scope='M has startup labels and a different ELF/layout; not byte-equivalent to normal B. No CPU qualification.')

def prepare(a):
    if a.archive is None or a.tools is None:raise ValueError('archive and prepared tools are required')
    if os.geteuid()==0 or platform.system()!='Linux' or platform.machine() not in ('aarch64','arm64'):
        raise ValueError('prepare requires ordinary Linux ARM64 benchmark user')
    a.candidate=h.NORMAL_OK_PRODUCT
    mapping=reuse.extract_builds(a.archive,a.root,reuse.OK_PMU_ARCHIVE_SHA,{'B','Driver'},owner_lifecycle_observation=True)
    commits=dict.fromkeys(('B','Driver'),a.candidate);reuse.restore_json_inputs(REPO,a.root,commits,mapping)
    inputs={n:reuse.validate_protocol_inputs(a.root,n,mapping) for n in commits}
    if inputs['B']['protocol_sha256']!=inputs['Driver']['protocol_sha256']:raise ValueError('protocol differs')
    source=a.root/'Driver/src/tests/benchmarks/cross_stack/client/main.go'
    normal=reuse.verify_manifest(a.root,'B',a.candidate,mapping,client_source=source)
    build=normal['stacks']['ku']['build'];native=build['native']['server']
    for path,wanted in [(native['binary'],h.NORMAL_PG_ELF_SHA),(native['generated_c'],h.NORMAL_PG_C_SHA),
            (native['source'],h.NORMAL_OK_INPUT_SHA),(build['libpq_source'],h.NORMAL_PG_D_SHA)]:reuse.checked(path,wanted)
    if normal['stacks']['ku'].get('env'):raise ValueError('normal library override')
    driver=reuse.translated(h.load(a.root/'Driver/manifest.json'),a.root);receipt=h.load(driver['build_receipt'])
    if driver.get('candidate_base')!=a.candidate or receipt.get('candidate_base')!=a.candidate or receipt.get('failure') is not None or not receipt.get('commands') or any(r.get('exit_code')!=0 for r in receipt['commands']):raise ValueError('driver provenance differs')
    reuse.require_elf(driver['client']['command'][0]);reuse.checked(driver['client']['command'][0],reuse.OK_PMU_DRIVER_SHA)
    reuse.checked(source,driver['client']['source_sha256'])
    for key in ('sha256','source_sha256'):
        if driver['client'][key]!=normal['client'][key]:raise ValueError('B/Driver differs')
    reuse.checked(driver['protocol'],inputs['Driver']['protocol_sha256'])
    driver.update(uid=os.getuid(),gid=os.getgid());h.save(a.root/'Driver/manifest.json',driver)
    state=dict(uid=os.getuid(),gid=os.getgid(),reused_build=dict(source_run=reuse.OK_PMU_RUN,
        archive_sha256=reuse.OK_PMU_ARCHIVE_SHA,imported_measurement_windows=0))
    result=h.prepare_memory_owner(a,state,plan_builder=plan)
    h.save(a.root/'owner-lifecycle-preparation.json',dict(state=state,result=result,diagnostic_only=True,normal_score_eligible=False))
    return result

def capture(pid,ticks,expected,log,destination,parent):
    destination=Path(destination);started=time.monotonic_ns();proc=Path('/proc')/str(pid)
    def identity():
        current=cost.read_text(proc/'stat').rsplit(')',1)[1].split()[19]
        ppid=int(next(l.split()[1] for l in cost.read_text(proc/'status').splitlines() if l.startswith('PPid:')))
        if current!=str(ticks) or ppid!=parent or h.sha(proc/'exe')!=expected:raise ValueError('same owned PID identity changed')
    identity();memory=h.diagnostic_memory(types.SimpleNamespace(process=types.SimpleNamespace(pid=pid)),destination.with_suffix('.smaps'))
    owners=cost.allocation_owner_ranges(log,pid,memory);pages=cost.allocation_owner_pages(pid,owners,ticks)
    identity();record=dict(pid=pid,start_ticks=str(ticks),derived_ELF_sha256=expected,memory=memory,owners=owners,pages=pages,
        started_monotonic_ns=started,ended_monotonic_ns=time.monotonic_ns(),diagnostic_only=True,normal_score_eligible=False)
    h.save(destination,record);return record

def observe(service,directory,stage,ticks,expected):
    directory=Path(directory);target=directory/(stage+'.json');started=time.monotonic_ns()
    active=service.metrics()
    command=[sys.executable,'-B',str(Path(__file__).resolve()),'capture','--root',str(directory),
        '--pid',str(service.process.pid),'--ticks',str(ticks),'--elf',expected,'--log',str(Path(service.cwd).parent/'service.log'),
        '--parent',str(os.getpid()),'--stage',stage]
    receipt=h.diagnostic_command(command,directory/(stage+'-observer.stdout'),timeout=3,limit=65536)
    if receipt.get('complete') is not True:raise RuntimeError('bounded owner observer failed: '+stage)
    record=h.load(target);record.update(stage=stage,active_metrics_before=active,observer_command=receipt,
        callback_started_monotonic_ns=started,callback_ended_monotonic_ns=time.monotonic_ns())
    h.save(target,record);return record

def sampler_class(original,callback,delay=7.5):
    class Once(original):
        instances=[]
        def __init__(self,*args,**kwargs):
            super().__init__(*args,**kwargs);self.owner_due=time.monotonic()+delay;self.owner_attempted=False;self.owner_error=None;self.instances.append(self)
        def sample(self):
            super().sample()
            if not self.owner_attempted and time.monotonic()>=self.owner_due:
                self.owner_attempted=True
                try:callback()
                except Exception as error:self.owner_error=repr(error)
    return Once

def check_report(report,wanted):
    ledger=report['ledger']
    if (report.get('valid_driver') is not True or ledger.get('results')!={'correct':wanted}
            or any(ledger.get(k)!=wanted for k in ('planned','attempted','sent','terminal','correct_total'))
            or ledger.get('planned_reconciled') is not True
            or report.get('service_cost',{}).get('driver_cgroup_removed') is not True):
        raise ValueError('PG diagnostic correctness/accounting/driver cleanup failed')

def archive(root):
    root=Path(root);original=h.evidence_file
    def selected(path,base):
        relative=path.relative_to(base);parts=relative.parts
        if path.is_symlink() or path.stat().st_size>32<<20:return False
        if len(parts)==1:return path.name in ('frozen-plan.json','owner-lifecycle-preparation.json','run.log')
        if parts[0]=='owner-lifecycle':
            return path.suffix in ('.json','.smaps','.stdout','.log') and not any(x in parts for x in ('data','socket')) and path.name!='config.json'
        if parts[0]=='M':return path.suffix in ('.json','.c','.log') or relative.as_posix()=='M/bin/ku-owner'
        if parts[0]=='B':
            return relative.as_posix() in ('B/manifest.json','B/build/bin/ku-server') or (parts[1:3]==('build','ku-native') and path.suffix in ('.c','.ku'))
        return False
    try:
        h.evidence_file=selected
        state=h.load(root/'owner-lifecycle-preparation.json')['state'] if (root/'owner-lifecycle-preparation.json').is_file() else dict(uid=root.stat().st_uid,gid=root.stat().st_gid)
        return h.full_archive(types.SimpleNamespace(root=root),state)
    finally:h.evidence_file=original

def run(root):
    root=Path(root);frozen=h.load(root/'frozen-plan.json')
    if frozen!=plan(frozen.get('versions',{})) or os.geteuid()!=0:raise ValueError('explicit prepared root diagnostic required')
    evidence=root/'owner-lifecycle';evidence.mkdir();state=h.load(root/'owner-lifecycle-preparation.json')['state']
    reuse.checked('/usr/lib/aarch64-linux-gnu/libpq.so.5',h.NORMAL_PG_D_SHA)
    fixture=db.Database(root/'fixture',state['uid'],state['gid']);service=None;harness=None;original=lh.Sampler;collectors=[];result=dict(diagnostic_only=True,normal_score_eligible=False,stages=[],status='unknown')
    try:
        fixture.start();private=h.load(fixture.manifest_path)
        class OwnerHarness(lh.Harness):
            def __init__(self,*args,**kwargs):
                super().__init__(*args,**kwargs);self.manifest=copy.deepcopy(self.manifest)
                self.manifest['db_url']=private['db_url'];self.db_domain=h.ExternalDomain(private['db_cgroup'])
        harness=OwnerHarness(root/'M',evidence/'harness');service=harness.start('ku',cores=2,memory=lh.GIB,output=evidence/'service')
        pid=service.process.pid;ticks=cost.read_text(Path('/proc')/str(pid)/'stat').rsplit(')',1)[1].split()[19]
        expected=frozen['versions']['M']['binary_sha256'];result.update(pid=pid,start_ticks=ticks,derived_ELF_sha256=expected)
        status,body=lh.request(service.address,'/db?key=1&rid=owner-ready',timeout=3)
        if status!=200 or json.loads(body)!=dict(id=1,label='item-1-中文🙂',score=17,rid='owner-ready'):raise ValueError('PG readiness failed')
        result['stages'].append(observe(service,evidence,'ready',ticks,expected))
        for stage,seconds in [('warm',10),('steady',15)]:
            samples=[]
            if stage=='steady':
                collector=sampler_class(original,lambda:samples.append(observe(service,evidence,'steady',ticks,expected)));collectors.append(collector);lh.Sampler=collector
            try:report=harness.client(service,'db',200,seconds,evidence/stage,events=False,runtime_metrics=False)
            finally:lh.Sampler=original
            report.update(diagnostic_only=True,normal_score_eligible=False);h.save(evidence/stage/'client.json',report)
            wanted=200*seconds;check_report(report,wanted)
            if stage=='warm':samples=[observe(service,evidence,'warm',ticks,expected)]
            if len(samples)!=1:raise ValueError('exactly one phase capture required')
            snapshot=samples[0]
            if stage=='steady':
                clock=lh.measurement_clock(report.get('configuration',{}));start=clock.get('origin_ns',0)
                if not clock.get('available') or not start<=snapshot['started_monotonic_ns']<=snapshot['ended_monotonic_ns']<=start+15_000_000_000:raise ValueError('steady capture outside actual arrival window')
            result['stages'].append(snapshot)
        spans=lambda s:[(r['pid'],r['label'],r['pointer'],r['requested_bytes'],r['context']) for r in s['owners']['records']]
        if any(spans(s)!=spans(result['stages'][0]) for s in result['stages']):raise ValueError('same-PID backing spans changed')
        result['status']='observed' if all(s['pages'].get('available') is True for s in result['stages']) else 'unknown'
    except Exception as error:result['error']=repr(error)
    finally:
        lh.Sampler=original
        for collector in collectors:
            for sampler in collector.instances:
                if sampler.thread is not None and sampler.thread.is_alive():sampler.thread.join(timeout=15)
                if sampler.thread is not None and sampler.thread.is_alive():
                    result.update(status='unknown',natural_cleanup=False,cleanup_deferred=True,reason='existing sampler/observer not reaped; runner teardown required',owned_service_pid=service.process.pid if service else None)
                    h.save(evidence/'summary.json',result);return result
                if sampler.owner_error:result['observer_error']=sampler.owner_error
        for name,cleanup in [('service_cleanup',service.stop if service else None),('harness_cleanup',harness.close if harness else None),('database_cleanup',fixture.stop)]:
            if cleanup:
                try:result[name]=cleanup()
                except Exception as error:result[name]={'error':repr(error)};result['status']='unknown'
        result['natural_cleanup']=all(result.get(k,{}).get('forced') is False and result[k].get('cgroup_removed') is True and result[k].get('after',{}).get('process_count')==0 for k in ['service_cleanup','database_cleanup'])
        if not result['natural_cleanup']:result['status']='unknown'
        h.save(evidence/'summary.json',result)
    return result

def main():
    parser=argparse.ArgumentParser(description=__doc__);parser.add_argument('phase',choices=['prepare','run','capture','archive'])
    parser.add_argument('--root',required=True,type=Path);parser.add_argument('--archive',type=Path);parser.add_argument('--tools',type=Path)
    parser.add_argument('--pid',type=int);parser.add_argument('--ticks');parser.add_argument('--elf');parser.add_argument('--log',type=Path);parser.add_argument('--parent',type=int);parser.add_argument('--stage')
    a=parser.parse_args();a.root=a.root.resolve()
    if any(os.environ.get(k) for k in ('LD_PRELOAD','LD_AUDIT','LD_LIBRARY_PATH','GNUTLS_SKIP_GLOBAL_INIT')):raise ValueError('ordinary library environment required')
    if a.phase=='prepare':result=prepare(a)
    elif a.phase=='run':result=run(a.root)
    elif a.phase=='archive':result=archive(a.root)
    else:result=capture(a.pid,a.ticks,a.elf,a.log,a.root/(a.stage+'.json'),a.parent)
    print(json.dumps(dict(status=result.get('status','prepared'),diagnostic_only=True,normal_score_eligible=False)))
    if a.phase=='run' and result.get('status')!='observed':raise SystemExit(1)
if __name__=='__main__':main()
