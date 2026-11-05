#!/usr/bin/env python3
"""One preregistered same-binary memory.current A/A; no build or stock probes."""
import argparse
import copy
import hashlib
import json
import math
import os
from pathlib import Path
import platform
import signal
import sys
import time

BASELINE='d05ca1551d4585011499d81d0ae828c4173306ff'
KERNEL='7.0.0-1017-azure'
HELPERS=('hosted_cycle.py','focus_cycle.py','linux_harness.py','database.py','reuse_build.py')


def helpers(source):
    sys.path.insert(0,str(source/'tests/benchmarks/cross_stack'))
    global h,fc,reuse,lh
    import hosted_cycle as h
    import focus_cycle as fc
    import reuse_build as reuse
    import linux_harness as lh
    if h.bl.RUST_TOOLCHAIN!='1.99.1':raise ValueError('fixed Rust1.99.1 cache required')


def plan_for(spec):
    return dict(schema='ku-memory-current-aa-v1',source_commit=BASELINE,
        versions={'A':copy.deepcopy(spec),'B':copy.deepcopy(spec)},
        resources=copy.deepcopy(h.RESOURCES),quality_policy=copy.deepcopy(h.POLICY),
        windows=[dict(name=name,group=group,order='AB'if group%2 else'BA',workload='db',rate=200,
            warmup_seconds=10,score_seconds=30,contract='pg-strict')
            for group in range(1,7)for name in(('A','B')if group%2 else('B','A'))],
        cooldown_seconds=60,observer_cpu=3,required_online_cpus=[0,1,2,3],required_kernel=KERNEL,
        maximum_windows=12,maximum_replacements=0,operation_alarm_seconds=720,
        bootstrap_seed=20261023,bootstrap_resamples=20000,
        minimum_memory_samples=5,latency_p99_SLO_ms=250,
        thresholds={'each_pair':[.97,1.03],'overall_95':[.985,1.015],'AB_over_BA_geomeans':[.985,1.015]},
        memory_metric='memory.steady_median_bytes',
        memory_scope='Original whole service cgroup memory.current; original arrival-window middle60% median.',
        diagnostic_only=True,normal_score_eligible=False,candidate_accepted=False,formal_qualified=False,
        original_3pct_adoption_guard_changed=False,
        boundary='One fixed ubuntu-24.04-arm job/boot, not the same physical VM as historical jobs. '
            'Observer CPU3 controls only owned coordinator/sampler placement. Fresh service and per-phase driver '
            'cgroups, databaseCPU3, original SQL/capacities remain. External contexts, seven-slot stock eviction, '
            'interrupts and kernel work are not controlled. No stock subtraction, RSS substitution, reclaim, '
            'memory-conditioned waiting, replacement windows or automatic adoption.')


def prepare(a):
    if os.geteuid()==0:raise ValueError('prepare as the original ordinary user')
    if a.archive is None:raise ValueError('prepare requires the already cached archive')
    if a.root.exists()and any(a.root.iterdir()):raise ValueError('fresh A/A output required; never overwrite evidence')
    a.root.mkdir(parents=True,exist_ok=True)
    manifests,original,actual=reuse.restore_current_pg_builds(a.archive,a.root/'cache',a.source,typed=False)
    if set(manifests)!={'B','Driver'}or set(actual)!={'B'}:raise ValueError('only normal B/Driver may be restored')
    manifest=manifests['B'];spec=dict(root=str(a.root/'cache/B'),stack='ku',source_commit=BASELINE,
        binary_sha256=reuse.CURRENT_PG_FILES['ELF'],
        artifacts={'protocol':dict(path=manifest['protocol'],sha256=reuse.CURRENT_PG_FILES['declared:protocol'])})
    plan=plan_for(spec);bindings={n:fc.version_identity(plan,n)for n in('A','B')}
    if bindings['A']!=bindings['B']or bindings['A']!=actual['B']:
        raise ValueError('both labels must use the identical verified normal binding')
    fixture=a.root/'fixture';fixture.mkdir();fm=copy.deepcopy(manifest);fm['stacks']={};h.save(fixture/'manifest.json',fm)
    receipt=dict(entry_sha256=h.sha(__file__),source_commit=BASELINE,uid=os.getuid(),gid=os.getgid(),
        original_binding=original['B'],same_binary_binding=bindings['A'],
        helper_sha256={n:h.sha(a.source/'tests/benchmarks/cross_stack'/n)for n in HELPERS},
        archive_sha256=reuse.CURRENT_PG_ARCHIVE_SHA,new_builds=0,imported_measurement_windows=0)
    h.save(a.root/'frozen-plan.json',plan);h.save(a.root/'prepare.json',receipt)
    return dict(prepared=True,maximum_windows=12,new_builds=0,root=str(a.root))


def platform_identity():
    online=(Path('/sys/devices/system/cpu/online').read_text()).strip()
    if platform.system()!='Linux'or platform.machine()not in('aarch64','arm64')or platform.release()!=KERNEL or online!='0-3':
        raise ValueError('required fixed ARM64 kernel7.0.0-1017-azure/online0-3 absent; zero windows, no runner retry')
    return dict(kernel=platform.release(),uname=list(os.uname()),online=online,
        boot_sha256=h.sha('/proc/sys/kernel/random/boot_id'),cpuinfo=Path('/proc/cpuinfo').read_text()[:65536],
        proc_stat=Path('/proc/stat').read_text()[:65536],
        boundary='Current job boot identity only; historical hosted VM was destroyed, no cross-job physical identity claim.')


class ObserverPlacement:
    """Move only this process; future sampler threads inherit its CPU3 mask."""
    def __init__(self,harness,output):
        self.harness,self.output=harness,output;self.domain=None;self.affinities={};self.origin=None
        self.record={'restored':False,'removed':False,'only_owned_process':os.getpid()}

    def validate(self):
        tids=[int(p.name)for p in Path('/proc/self/task').iterdir()]
        if len(tids)>256:raise ValueError('owned observer thread bound exceeded')
        masks={str(t):sorted(os.sched_getaffinity(t))for t in tids}
        if not masks or any(mask!=[3]for mask in masks.values()):raise ValueError('owned observer/sampler affinity differs fromCPU3')
        expected='0::/'+str(self.domain.path.relative_to('/sys/fs/cgroup'))
        if expected not in Path('/proc/self/cgroup').read_text().splitlines():raise ValueError('observer moved out of owned cgroup')
        return dict(tid_affinities=masks,cgroup=str(self.domain.path))

    def __enter__(self):
        base=Path('/sys/fs/cgroup');lines=Path('/proc/self/cgroup').read_text().splitlines()
        paths=[l[3:]for l in lines if l.startswith('0::')]
        if len(paths)!=1:raise ValueError('one original unified observer cgroup required')
        self.origin=(base/paths[0].lstrip('/')).resolve(strict=True)
        if self.origin!=base and base not in self.origin.parents:raise ValueError('original cgroup outside visible mount')
        self.affinities={int(p.name):set(os.sched_getaffinity(int(p.name)))for p in Path('/proc/self/task').iterdir()}
        if len(self.affinities)>256:raise ValueError('owned observer thread bound exceeded')
        try:
            self.domain=lh.Domain(self.harness,'aa-observer',1,lh.GIB,'3')
            # This infrastructure remains outside all measured service domains.
            # Do not introduce an observer quota or memory pressure intervention.
            (self.domain.path/'cpu.max').write_text('max 100000');(self.domain.path/'memory.max').write_text('max')
            (self.domain.path/'cgroup.procs').write_text(str(os.getpid()))
            for tid in self.affinities:os.sched_setaffinity(tid,{3})
            self.record.update(origin=str(self.origin),original_affinities={str(t):sorted(v)for t,v in self.affinities.items()},
                placement=self.validate(),limits={k:(self.domain.path/k).read_text().strip()for k in('cpu.max','memory.max','cpuset.cpus.effective')})
            h.save(self.output,self.record);return self
        except BaseException:
            self.__exit__(*sys.exc_info());raise

    def __exit__(self,*exc):
        errors=[]
        if self.origin is not None:
            try:(self.origin/'cgroup.procs').write_text(str(os.getpid()))
            except BaseException as error:errors.append('restore cgroup: '+repr(error))
        restore=dict(self.affinities)
        try:
            tids=[int(p.name)for p in Path('/proc/self/task').iterdir()]
            if len(tids)>256:errors.append('cleanup owned thread bound exceeded')
            parent_mask=self.affinities.get(os.getpid(),{0,1,2,3})
            for tid in tids[:256]:restore.setdefault(tid,parent_mask)
        except BaseException as error:errors.append('enumerate owned cleanup threads: '+repr(error))
        for tid,mask in restore.items():
            try:os.sched_setaffinity(tid,mask)
            except ProcessLookupError:pass
            except BaseException as error:errors.append('restore affinity: '+repr(error))
        self.record['restored']=not errors
        if self.domain is not None and self.record['restored']:
            try:self.record['removed']=self.domain.remove()
            except BaseException as error:errors.append('remove observer: '+repr(error))
        self.record['cleanup_errors']=errors
        self.record.setdefault('cleanup_attempts',[]).append(dict(errors=errors,restored=self.record['restored'],removed=self.record['removed']))
        h.save(self.output,self.record)
        if(errors or(self.domain is not None and not self.record['removed']))and not any(exc):
            raise RuntimeError('owned observer cleanup incomplete')


def decision(plan,rows,complete):
    result=dict(conditions_adjudicable=False,product_benefit=False,candidate_accepted=False,
        original_3pct_guard_passed=False,formal_qualified=False,reasons=[],window_ratios=[],order_geomeans={})
    if not complete or len(rows)!=12:
        result['reasons'].append('incomplete fixed12 windows; no replacement');return result
    pairs=[]
    for group in range(1,7):
        selected=[r for r in rows if r['group']==group];by={r['name']:r for r in selected}
        if len(selected)!=2 or set(by)!={'A','B'}:raise ValueError('A/A fixed pair missing/duplicate')
        pairs.append(by)
    nums=[fc.metric(p['B'],plan['memory_metric'])for p in pairs];dens=[fc.metric(p['A'],plan['memory_metric'])for p in pairs]
    ci=fc.paired_interval(nums,dens,minimum=6,seed=plan['bootstrap_seed'],resamples=plan['bootstrap_resamples']);result['interval']=ci
    if ci.get('status')!='estimated':result['reasons'].append('paired current metric unavailable');return result
    ratios=ci['ratios'];result['window_ratios']=ratios
    if not all(.97<=v<=1.03 for v in ratios):result['reasons'].append('individual A/A ratio outside original3%band')
    if not(.985<=ci['lower95']and ci['upper95']<=1.015):result['reasons'].append('A/A95%interval outside fixed1.5%noise band')
    geo=lambda xs:math.exp(sum(math.log(x)for x in xs)/len(xs))
    order={name:geo([ratios[i]for i in range(6)if('AB'if i%2==0 else'BA')==name])for name in('AB','BA')}
    result['order_geomeans']=order;result['AB_over_BA_geomeans']=order['AB']/order['BA']
    if not .985<=result['AB_over_BA_geomeans']<=1.015:result['reasons'].append('AB/BA order contrast outside fixed1.5%band')
    result['first_last_half_current_geomeans']={label:geo([fc.metric(r,plan['memory_metric'])for r in part])for label,part in(('first',rows[:6]),('last',rows[6:]))}
    result['first_last_half_drift_ratio']=result['first_last_half_current_geomeans']['last']/result['first_last_half_current_geomeans']['first']
    result['conditions_adjudicable']=not result['reasons']
    result['boundary']='A/A repeatability check for this fixed job/boot/control policy only, not universal3%resolution, a product memory improvement or candidate guard. Preserve all trajectories/order drift; never subtract them or inherit old candidate scores.'
    return result


def run(a):
    if os.geteuid()!=0:raise ValueError('run requires owned cgroup root control')
    plan=h.load(a.root/'frozen-plan.json');proof=h.load(a.root/'prepare.json')
    if plan!=plan_for(plan['versions']['A'])or plan['versions']['A']!=plan['versions']['B']:
        raise ValueError('fixed identical-binary A/A plan changed')
    if proof['entry_sha256']!=h.sha(__file__)or set(proof['helper_sha256'])!=set(HELPERS)or any(h.sha(a.source/'tests/benchmarks/cross_stack'/n)!=v for n,v in proof['helper_sha256'].items()):
        raise ValueError('prepared entry/helpers changed')
    expected=proof['same_binary_binding']
    if any(fc.version_identity(plan,n)!=expected for n in('A','B')):raise ValueError('A/A byte identities differ')
    result=dict(execution_complete=False,rows=[],stopped=[],diagnostic_only=True,candidate_accepted=False,normal_score_eligible=False)
    original_harness=fc.Harness;fixture=None;placement=None;began=time.monotonic();private=None
    class PrivateHarness(original_harness):
        def __init__(self,*args,**kw):
            super().__init__(*args,**kw);self.manifest=copy.deepcopy(self.manifest)
            self.manifest['db_url']=private['db_url'];self.db_domain=h.ExternalDomain(private['db_cgroup'])
    def stop(*unused):raise TimeoutError('fixed720s A/A bound or interruption; no retry')
    previous={sig:signal.getsignal(sig)for sig in(signal.SIGALRM,signal.SIGTERM)}
    try:
        for sig in previous:signal.signal(sig,stop)
        signal.alarm(plan['operation_alarm_seconds'])
        identity=platform_identity();h.save(a.root/'platform.json',identity)
        fixture=h.db.Database(a.root/'fixture',proof['uid'],proof['gid'])
        placement=ObserverPlacement(fixture.harness,a.root/'observer-placement.json');placement.__enter__()
        fixture.start();private=h.load(fixture.manifest_path);fc.Harness=PrivateHarness
        h.save(a.root/'cooldown.json',{'begin_ns':time.monotonic_ns(),'fixed_seconds':plan['cooldown_seconds'],'memory_conditioned':False})
        print(json.dumps({'phase':'fixed-cooldown','seconds':plan['cooldown_seconds']}),flush=True);time.sleep(plan['cooldown_seconds'])
        h.save(a.root/'cooldown-end.json',{'end_ns':time.monotonic_ns()})
        for config in plan['windows']:
            if platform_identity()['boot_sha256']!=identity['boot_sha256']:raise ValueError('job boot identity changed')
            placement.validate()
            if any(fc.version_identity(plan,n)!=expected for n in('A','B')):raise ValueError('A/A input identity changed')
            print(json.dumps({'phase':'window','group':config['group'],'name':config['name']}),flush=True)
            row=fc.window(plan,config['name'],config['workload'],config['rate'],config['warmup_seconds'],config['score_seconds'],
                a.root/'windows'/('g'+str(config['group'])+'-'+config['name']),binding=expected,
                job={'phase':'diagnostic','contract':config['contract']})
            row.update(group=config['group'],order=config['order'],observer_placement=placement.validate())
            valid,_=h.normal_diagnostic_quality(row);value=fc.metric(row,plan['memory_metric'])
            samples=row.get('memory',{}).get('sample_count');p99=row.get('latency',{}).get('p99_ms')
            row['aa_window_valid']=(valid and type(value)in(int,float)and math.isfinite(value)and value>0
                and type(samples)is int and samples>=plan['minimum_memory_samples']
                and type(p99)in(int,float)and math.isfinite(p99)and 0<=p99<=plan['latency_p99_SLO_ms'])
            result['rows'].append(row);h.save(a.root/'results.json',result)
            if not row['aa_window_valid']:
                result['stopped'].append('original diagnostic correctness/accounting/arrival/resource/cleanup failure');break
    except BaseException as error:
        result['error']=repr(error)
        if not isinstance(error,Exception):raise
    finally:
        fc.Harness=original_harness
        try:result['database_cleanup']=fixture.stop()if fixture else{'not_started':True}
        except BaseException as error:result['database_cleanup']={'error':repr(error)}
        try:
            if placement and placement.domain is not None:placement.__exit__(*sys.exc_info())
            result['observer_cleanup']=placement.record if placement else{'not_started':True}
        except BaseException as error:result['observer_cleanup']={**(placement.record if placement else{}),'error':repr(error)}
        signal.alarm(0)
        for sig,handler in previous.items():signal.signal(sig,handler)
        cleanup=result['database_cleanup'];dbok=cleanup.get('forced')is False and cleanup.get('cgroup_removed')is True and cleanup.get('after',{}).get('process_count')==0
        obs=result['observer_cleanup'];obsok=obs.get('restored')is True and obs.get('removed')is True
        result.update(seconds=time.monotonic()-began,execution_complete=len(result['rows'])==12 and not result['stopped']and'error'not in result and dbok and obsok)
        result['decision']=decision(plan,result['rows'],result['execution_complete']);h.save(a.root/'summary.json',result)
    return result


def main():
    p=argparse.ArgumentParser(description=__doc__);p.add_argument('--phase',choices=('prepare','run'),required=True)
    p.add_argument('--root',required=True,type=Path);p.add_argument('--source',required=True,type=Path);p.add_argument('--archive',type=Path)
    a=p.parse_args();a.root=a.root.resolve();a.source=a.source.resolve();helpers(a.source)
    if any(os.environ.get(k)for k in('LD_PRELOAD','LD_AUDIT','LD_LIBRARY_PATH')):raise ValueError('library overrides change frozen normal inputs')
    result=prepare(a)if a.phase=='prepare'else run(a);print(json.dumps({k:v for k,v in result.items()if k!='rows'}),flush=True)
    return 0 if a.phase=='prepare'or result['execution_complete']else 2


if __name__=='__main__':raise SystemExit(main())
