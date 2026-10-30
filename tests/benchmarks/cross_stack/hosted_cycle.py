#!/usr/bin/env python3
"""Serial ARM64 hosted A0/B/R evidence. Builds and observations are separate from goals.
No product patch, remote dispatch, SDK matrix, installation or release operation.
"""
from __future__ import annotations
import argparse, copy, gzip, hashlib, io, json, os
from pathlib import Path
import platform, pwd, re, shutil, signal, subprocess, sys, tarfile, time
import build_linux as bl
import focus_cycle as fc
import database as db
from linux_harness import ExternalDomain, GIB

ARCHIVE_LIMIT = 384<<20
BASELINE = '988f32ef496b18b935a3c276ef7933d0563691da'
POLICY = {'arrival_limits_ms': {'p99_ms': 2, 'p999_ms': 10, 'max_ms': 50}}
RESOURCES = {'service_cores': 2, 'service_memory_bytes': GIB,
             'driver_cores': 1, 'driver_cpuset': '2', 'driver_memory_bytes': GIB}
SERVICE_ENV = {'KU_EVENT_LOOPS': '{cores}', 'KU_BLOCKING_THREADS': '17',
               'KU_THREAD_BUDGET': '64', 'KU_TASK_MAX_BYTES': '67108864',
               'KU_HTTP_REQUEST_TIMEOUT_MS': '30000'}


def load(p): return json.loads(Path(p).read_text())
def sha(p): return bl.digest(p)
def save(p, v): bl.write_json(p, v)
def encoded(v): return (json.dumps(v, sort_keys=True, separators=(',', ':'))+'\n').encode()


def frozen_plan(versions):
    """Seven named rows, PG Ku shared only inside the same registered group."""
    triples = [['A0','B','R'], ['B','R','A0'], ['R','A0','B'], ['A0','R','B'], ['B','A0','R']]
    pairs = [['B','R'], ['R','B'], ['B','R'], ['R','B'], ['B','R']]
    schedule, targets = [], []
    entries = [('json_object',500,'dom',triples,'B','R',.90),
        ('ok',2000,'public',pairs,'B','R',1), ('cpu',80,'public',pairs,'B','R',1),
        ('json_object',500,'typed',[[n.replace('B','BJ').replace('R','RJ') for n in g] for g in pairs],'BJ','RJ',1),
        ('db',200,'pg-shared',[[n for old in g for n in ([old,'RS'] if old=='R' else [old])] for g in triples],'B','R',1),
        ('db',200,'typed-pg',[[n for old in g for n in (['BP'] if old=='B' else ['RP','RPS'])] for g in pairs],'BP','RP',1)]
    for kind,rate,contract,orders,n,d,limit in entries:
        schedule.append(dict(phase='final',workload=kind,rate=rate,contract=contract,
                             orders=orders,warmup_seconds=10,score_seconds=30))
        for metric in ['full_cpu_us','memory.steady_median_bytes']:
            targets.append(dict(phase='final',workload=kind,contract=contract,numerator=n,
                denominator=d,maximum_ratio=limit if metric=='full_cpu_us' else 1,metric=metric,minimum_pairs=5))
        if contract in ('pg-shared','typed-pg'):
            targets.append(dict(phase='final',workload=kind,contract=contract,numerator=n,
                denominator='RS' if contract=='pg-shared' else 'RPS',maximum_ratio=.90 if contract=='pg-shared' else 1,
                metric='full_cpu_us',minimum_pairs=5,display_contract='pg-strict' if contract=='pg-shared' else 'typed-pg-strict'))
            targets.append(dict(targets[-1],metric='memory.steady_median_bytes',maximum_ratio=1))
    return dict(versions=versions,resources=RESOURCES,quality_policy=POLICY,rates={'json_object':500,'db':200,'ok':2000,'cpu':80},
        schedule=schedule,targets=targets,resume={'maximum_replacements':0},bootstrap_resamples=20000,
        stop_after_consecutive_unusable_windows=2,control_cpu_max_min=1.10,
        pg_shared_scope='one Ku window serves default/strict only within the SAME preregistered entry/group; R normal/reset separate',
        historical_ablation_scope='AJ/AR/AP/AM/B0 local history excluded; no old-machine score imported',
        core_advantage_goal={'maximum_cpu_ratio':.80,'maximum_memory_ratio':1,'scope':'predeclared JSON and strict PG only; no best-row postselection'},
        capacity=dict(final_frozen=True,names=['B','R'],workloads=['json_object','db'],maximum_points=8,refinements=2,
            factor=2,maximum_rate=64000,maximum_coarse_points=6,maximum_rates={'json_object':16000,'db':6400},start_rates={'json_object':500,'db':200},warmup_seconds=10,score_seconds=30,
            recovery_rate=100,recovery_seconds=2,recovery_maximum_seconds=5,
            contracts={'json_object':'dom','db':'pg-default'}),
        stability=dict(name='B',frozen_final=True,seconds=1800,segment_seconds=300,mixed_rate=500,short_rate=800,cpu_rate=10,
            short_slo_ms=20,maximum_start_skew_seconds=2,maximum_segment_gap_seconds=10,
            growth_limits={'service_memory_bytes':64<<20,'coordinator_rss_bytes':32<<20,'service_fd':32,'service_threads':8}))


def step(state, root, name, call, resume=False):
    old=state.setdefault('steps',{}).get(name)
    if old and old.get('state')=='completed': return old['result']
    if old and not resume: raise ValueError('retained incomplete step; new root required: '+name)
    row={'state':'started','started_ns':time.time_ns(),'goal_status':'unknown'};state['steps'][name]=row;save(root/'hosted-state.json',state)
    print(json.dumps({'step':name,'state':'started'}),flush=True)
    try:
        row.update(result=call(),state='completed');row['goal_status']=row['result'].get('goal_status','unknown');return row['result']
    except BaseException as e: row.update(state='failed',error=repr(e),goal_status='blocked');raise
    finally:
        row['ended_ns']=time.time_ns();save(root/'hosted-state.json',state)
        print(json.dumps({'step':name,'state':row['state'],'goal_status':row['goal_status']}),flush=True)


def snapshot(checkout, commit, destination):
    destination.mkdir(parents=True,exist_ok=False)
    process=subprocess.Popen(['git','-C',str(checkout),'archive','--format=tar',commit],stdout=subprocess.PIPE)
    try:
        # Trusted git objects, extracted into a new isolated directory; never a checkout/worktree mutation.
        subprocess.run(['tar','-xf','-','-C',str(destination)],stdin=process.stdout,check=True,timeout=120)
        process.stdout.close();assert process.wait(timeout=10)==0
    finally:
        if process.poll() is None:process.kill();process.wait(timeout=5)


def normalize_manifest(m, protocol, client):
    m.update(driver_cores=1,driver_cpuset='2',database_cpu_set='3',quality_policy=POLICY,
             service_env=SERVICE_ENV,protocol=str(protocol),db_cgroup=None,
             cgroup_root='/sys/fs/cgroup/ku-hosted-'+hashlib.sha256(str(protocol).encode()).hexdigest()[:12])
    m['client']=copy.deepcopy(client);m['client']['env']={'GOMAXPROCS':'1'}
    return m


def build(a,state):
    if os.geteuid()==0: raise ValueError('build must run as the ordinary checkout user')
    uid,gid=os.getuid(),os.getgid();state.update(uid=uid,gid=gid)
    class Builder(bl.Builder):
        def command(self,label,command,**kwargs):
            command=list(command)
            if label=='ku-cli':command[command.index('--target-dir')+1]=a.root/'cli-cache'
            result=super().command(label,command,**kwargs)
            if label=='ku-cli':
                dest=self.build/'ku-cargo/release/ku';dest.parent.mkdir(parents=True,exist_ok=True)
                shutil.copy2(a.root/'cli-cache/release/ku',dest) # distinct snapshot BEFORE next CLI overwrites cache
            return result
    def one(name,commit,mode=None):
        d=a.root/name;snapshot(a.source,commit,d/'src');(d/'tools').symlink_to(a.tools,target_is_directory=True)
        b=Builder(d,d/'src',uid=uid,gid=gid,candidate=commit,**(mode or {}))
        try:
            if name in ('A0','B'):b.ku()
            elif name=='R':b.rust();b.go_build(client=True)
            else:b.ku_native(Path(load(a.root/'B/manifest.json')['stacks']['ku']['build']['cli']))
            return b.finish()
        except BaseException as e:b.finish(failed=repr(e));raise
    for name,commit,mode in [('A0',a.baseline,None),('B',a.candidate,None),('R',a.candidate,None),
        ('BJ',a.candidate,{'ku_json':'typed'}),('BP',a.candidate,{'ku_pg_read':'typed'}),('BD',a.candidate,{'ku_format':'direct'})]:
        step(state,a.root,'build-'+name,lambda n=name,c=commit,m=mode:one(n,c,m))
    rm=load(a.root/'R/manifest.json');client=rm['client'];protocol=Path(rm['protocol']);versions={}
    for name in ['A0','B','R','BJ','BP','BD','RS','RJ','RP','RPS']:
        d=a.root/name;is_r=name.startswith('R');m=load(a.root/('R' if is_r else name)/'manifest.json')
        m=normalize_manifest(m,protocol,client);stack='rust' if is_r else 'ku';m['stacks']={stack:m['stacks'][stack]}
        if is_r:m['stacks'][stack]['env'].update(BENCH_REQUEST_TIMEOUT_MS='30000',BENCH_LISTEN_BACKLOG='4096',
            BENCH_PG_MODE='reset-equivalent' if name in ('RS','RPS') else 'normal',
            BENCH_PG_API='typed-text-key' if name in ('RP','RPS') else 'normal',BENCH_JSON_MODE='typed' if name=='RJ' else 'generic')
        if name=='BD':m.update(diagnostic_only=True,normal_score_eligible=False)
        save(d/'manifest.json',m);binary=m['stacks'][stack]['command'][0]
        versions[name]=dict(root=str(d),stack=stack,source_commit=m['candidate_base'],binary_sha256=sha(binary))
    fixture=normalize_manifest(copy.deepcopy(load(a.root/'B/manifest.json')),protocol,client)
    fixture['stacks']={};save(a.root/'fixture/manifest.json',fixture)
    plan=frozen_plan(versions);save(a.root/'frozen-plan.json',plan)
    identities={n:fc.version_identity(plan,n) for n in versions};save(a.root/'build-identities.json',identities)
    dependencies={};abi={}
    for binary in sorted({f['path'] for v in identities.values() for k,f in v['files'].items() if k in ('ELF','Ku CLI','driver ELF')}):
        linked=subprocess.run(['ldd',binary],capture_output=True,text=True,check=True,timeout=30).stdout
        abi[binary]={'sha256':sha(binary),'ELF_header':subprocess.run(['readelf','-h',binary],capture_output=True,text=True,check=True,timeout=30).stdout,
            'dynamic':subprocess.run(['readelf','-d',binary],capture_output=True,text=True,check=True,timeout=30).stdout,'linked_libraries':linked}
        for library in re.findall(r'(/[^\s()]+)',linked):
            path=Path(library).resolve(strict=True);key=sha(path);dest=a.root/'dependencies'/(key+'-'+path.name)
            if key not in dependencies:dest.parent.mkdir(exist_ok=True);shutil.copyfile(path,dest)
            dependencies[key]={'system_path':str(path),'copy':str(dest),'bytes':path.stat().st_size}
    save(a.root/'dependency-identities.json',dependencies);save(a.root/'target-abi.json',abi)
    return {'execution_complete':True,'goal_status':'unknown','plan_sha256':sha(a.root/'frozen-plan.json'),
            'direct_scope':'BD separate app/ELF identity only; not mixed with helpers主表'}


def environment_record(summary, destination):
    rows=summary.get('rows',[]);costs=[r.get('cpu_us') for r in rows];missing=[];faults=[];sources=[]
    for row in rows:
        path=Path(row['score_report']) if row.get('score_report') else None
        if path is None or not path.is_file():missing.append('score report absent');continue
        result=load(path);cost=result.get('service_cost',{});sources.append({'path':str(path),'sha256':sha(path)})
        for key in ['before','after']:
            if not cost.get(key):missing.append('resource endpoint '+key)
        resources=Path(cost.get('samples_path',''))
        samples=load(resources).get('samples',[]) if resources.is_file() else []
        domains={d['label'] for sample in samples for d in sample.get('domains',[])}
        if len(domains)<3:missing.append('service/driver/database resource samples')
        for label in sorted(domains):
            values=[d for sample in samples for d in sample['domains'] if d['label']==label]
            for field,group in [('oom','memory_events'),('oom_kill','memory_events'),('throttled_usec','cpu_stat'),('pgmajfault','memory_stat')]:
                points=[v.get(group,{}).get(field) for v in values]
                if len(points)<2 or any(type(x)is not int for x in points):missing.append(label+':'+group+'.'+field)
                elif max(points)!=min(points):faults.append(label+':'+group+'.'+field)
    spread=max(costs)/min(costs) if len(costs)==3 and all(type(x)in(int,float) and x>0 for x in costs) else None
    observed=len(rows)==3 and all(r.get('status')=='observed' and r.get('functional_valid') is True and
        r.get('accounting_valid') is True and r.get('arrival_schedule_quality')=='pass' and r.get('natural_reclamation') is True for r in rows)
    quality='fail' if faults or spread is not None and spread>1.10 or not observed else 'unknown' if missing or spread is None else 'pass'
    record=dict(quality=quality,evidence=str(destination),source_reports=sources,CPU_max_min=spread,
        control_count=len(rows),pacing_policy=POLICY,missing=missing,faults=faults,measurement_fault_confirmed=False,
        scope='three independent current A0 controls; actual score resource endpoints; page faults are not attributed to swap')
    save(destination,record);return record


def run(a,state):
    if os.geteuid()!=0:raise ValueError('run requires root cgroup control')
    boot=hashlib.sha256(Path('/proc/sys/kernel/random/boot_id').read_bytes()).hexdigest()
    if state.setdefault('run_boot_sha256',boot)!=boot:raise ValueError('resume across hosts/boots forbidden; new root required')
    plan=load(a.root/'frozen-plan.json');fixture=db.Database(a.root/'fixture',state['uid'],state['gid'])
    original=fc.Harness;environment=None;private=None
    state['execution_environment']={'machine':platform.machine(),'kernel':platform.release(),'cpu_count':os.cpu_count(),
        'coordinator_affinity':sorted(os.sched_getaffinity(0)),'MemTotal':next(x for x in Path('/proc/meminfo').read_text().splitlines() if x.startswith('MemTotal:')),
        'parent_cgroup_limits':{n:(Path('/sys/fs/cgroup')/n).read_text().strip() for n in ['cpu.max','memory.max','memory.swap.max'] if (Path('/sys/fs/cgroup')/n).exists()}}
    class HostedHarness(original):
        def __init__(self,*args,**kwargs):
            super().__init__(*args,**kwargs);self.manifest=copy.deepcopy(self.manifest)
            self.manifest['db_url']=private['db_url'];self.db_domain=ExternalDomain(private['db_cgroup'])
            if environment:self.manifest['environment_qualification']=environment
    def execute(label,config,phase):
        path=a.root/(label+'-plan.json')
        if path.exists() and load(path)!=config:raise ValueError('frozen plan changed; new root required')
        if not path.exists():save(path,config)
        output=a.root/'results'/label
        return step(state,a.root,label,lambda:fc.execute(path,output,phase,resume=(output/'identity.json').exists()),resume=True)
    try:
        fixture.start();private=load(a.root/'fixture/manifest.json')
        private['database']['cpu_set']=load(fixture.state_path)['limits']['cpuset.cpus'];save(fixture.manifest_path,private)
        fc.Harness=HostedHarness
        fixed=copy.deepcopy(plan);fixed.pop('capacity',None);fixed.pop('stability',None)
        control=copy.deepcopy(fixed);control.update(targets=[],schedule=[dict(phase='control',workload='json_object',rate=500,
            orders=[['A0'],['A0'],['A0']],warmup_seconds=5,score_seconds=10)])
        control.pop('stop_after_consecutive_unusable_windows',None)
        summary=execute('control',control,'all');environment=environment_record(summary,a.root/'environment-qualification.json')
        final=execute('final',fixed,'all')
        function_ok=bool(final.get('rows')) and all(r.get('functional_valid') is True and r.get('accounting_valid') is True for r in final['rows'])
        if function_ok:
            def capacities():
                results=[]
                for kind in plan['capacity']['workloads']:
                    config=copy.deepcopy(plan);config['capacity']['workloads']=[kind]
                    config['capacity']['maximum_rate']=plan['capacity']['maximum_rates'][kind]
                    results.append(execute('capacity-'+kind,config,'capacity'))
                return {'capacity':[r for result in results for r in result['capacity']],
                    'execution_complete':all(r.get('execution_complete') is True for r in results),'goal_status':'unknown'}
            step(state,a.root,'capacity',capacities,resume=True)
            if not state['steps'].get('stability'):
                execute('stability',plan,'stability') # once-only; no resume/replacement of continuous evidence
        else:state['deferred']={'capacity':'confirmed function/accounting failure','stability':'same candidate; not retried'}
        core_targets=[dict(t,maximum_ratio=.80 if t['metric']=='full_cpu_us' else 1) for t in plan['targets']
            if t['numerator']=='B' and ((t['contract']=='dom' and t['denominator']=='R') or (t['contract']=='pg-shared' and t['denominator']=='RS'))]
        core_plan=dict(plan,targets=core_targets);core=fc.compare_goals(core_plan,final.get('rows',[]))
        scenes=[[g for g in core if g['target']['contract']==contract] for contract in ['dom','pg-shared']]
        core_status='met' if any(len(g)==2 and all(x['goal_status']=='met' for x in g) for g in scenes) else 'not_met' if all(any(x['goal_status']=='not_met' for x in g) for g in scenes) else 'unknown'
        save(a.root/'results/core-advantage.json',{'goals':core,'goal_status':core_status,'selection':'predeclared dom/strictPG only; no score postselection'})
        goal='not_met' if 'not_met' in (core_status,final.get('goal_status')) else 'met' if core_status==final.get('goal_status')=='met' else 'unknown'
        acceptance=acceptance_result(final,state['steps'].get('capacity',{}).get('result',{}),
            state['steps'].get('stability',{}).get('result',{}),goal)
        return dict(acceptance,core_advantage_goal_status=core_status,
            formal_qualified=final.get('formal_qualified',False),environment_quality=environment['quality'],candidate_accepted=False,
            stability_status=state['steps'].get('stability',{}).get('state','not_executed'))
    finally:
        fc.Harness=original
        state['database_cleanup']=fixture.stop();save(a.root/'hosted-state.json',state)


def compiler_plan(versions):
    names=['B','KO3','KThin','KPGO','R','RThin','RPGO']
    orders=[names,names[2:]+names[:2],names[4:]+names[:4]]
    schedule=[dict(phase='explore',workload=w,rate=r,contract='compiler',orders=orders,warmup_seconds=5,score_seconds=15)
        for w,r in [('cpu',80),('json_object',500)]]
    schedule.append(dict(phase='explore',workload='cpu',rate=80,contract='direct-diagnostic',
        orders=[['BD','B'],['B','BD'],['BD','B']],warmup_seconds=5,score_seconds=15))
    return dict(versions=versions,resources=RESOURCES,quality_policy=POLICY,rates={'cpu':80,'json_object':500},
        schedule=schedule,targets=[],resume={'maximum_replacements':0},stop_after_consecutive_unusable_windows=2,
        scope='48 diagnostic comparison windows plus3 independent controls; no formal/capacity/stability; default remainsO2/helpers')


def compiler(a,state):
    """Existing compiler CLI under workload uid; HTTP train/heldout, then one short plan."""
    import compile_variants as cv
    import profile_training as pt
    from linux_harness import Domain
    if os.geteuid()!=0:raise ValueError('compiler requires root coordinator; compilation drops to build uid/gid')
    boot=hashlib.sha256(Path('/proc/sys/kernel/random/boot_id').read_bytes()).hexdigest()
    if state.setdefault('compiler_boot_sha256',boot)!=boot:raise ValueError('compiler resume across boots forbidden')
    directory=a.root/'compiler';directory.mkdir(exist_ok=True);os.chown(directory,state['uid'],state['gid'])
    fixture=db.Database(a.root/'fixture',state['uid'],state['gid']);old_fc,old_pt=fc.Harness,pt.Harness
    private=None;environment=None
    class HostedHarness(old_fc):
        def __init__(self,*args,**kwargs):
            super().__init__(*args,**kwargs);self.manifest=copy.deepcopy(self.manifest)
            self.manifest['db_url']=private['db_url'];self.db_domain=ExternalDomain(private['db_cgroup'])
            if environment:self.manifest['environment_qualification']=environment
    def execute(label,plan):
        path=directory/(label+'-plan.json');output=directory/label
        if path.exists() and load(path)!=plan:raise ValueError('compiler plan changed; fresh root required')
        if not path.exists():save(path,plan)
        return step(state,a.root,'compiler-'+label,lambda:fc.execute(path,output,'all',resume=(output/'identity.json').exists()),resume=True)
    def tuned(name,stack,variant,training=None):
        output=directory/name;argv=[sys.executable,str(Path(cv.__file__).resolve()),'--manifest',str(a.root/('B' if stack=='ku' else 'R')/'manifest.json'),
            '--output',str(output),'--tools',str(a.tools),'--stack',stack,'--variant',variant]
        if training:argv+=['--training-receipt',str(training)]
        def call():
            h=old_fc(a.root/'B',directory/('build-coordinator-'+name));domain=Domain(h,'compiler-build',2,6*GIB,'0-1');process=None
            env=cv.clean_environment(a.tools);env['HOME']=pwd.getpwuid(state['uid']).pw_dir
            try:
                process=h.launch(argv,a.source,env,domain,directory/(name+'-command.log'))
                if process.wait(timeout=1500)!=0:raise ValueError('compiler CLI failed; retained log: '+name)
                receipt=load(output/'build.json')
                if receipt.get('passed') is not True:raise ValueError('compiler receipt not passed: '+name)
                return {'passed':True,'goal_status':'unknown','build':str(output/'build.json'),'build_sha256':sha(output/'build.json'),
                    'command':argv,'uid':h.uid,'gid':h.gid,'HOME':env['HOME'],'build_limits':domain.settings}
            finally:
                domain.kill()
                if process is not None and process.poll() is None:process.wait(timeout=5)
                if not domain.remove():raise RuntimeError('owned compiler domain not removed')
                h.close()
        step(state,a.root,'compiler-build-'+name,call)
        return output/'manifest.json'
    try:
        fixture.start();private=load(fixture.manifest_path);fc.Harness=pt.Harness=HostedHarness
        versions=copy.deepcopy(load(a.root/'frozen-plan.json')['versions'])
        control=compiler_plan(versions);control.update(schedule=[dict(phase='control',workload='json_object',rate=500,
            orders=[['A0'],['A0'],['A0']],warmup_seconds=5,score_seconds=10)])
        control.pop('stop_after_consecutive_unusable_windows')
        observed=execute('control',control);environment=environment_record(observed,directory/'environment-qualification.json')
        manifests={'KO3':tuned('KO3','ku','o3'),'KThin':tuned('KThin','ku','thin'),'RThin':tuned('RThin','rust','thin')}
        validation={}
        for stack,prefix in [('ku','K'),('rust','R')]:
            generated=tuned(prefix+'Generate',stack,'generate');training=directory/('http-train-'+stack)
            step(state,a.root,'compiler-http-train-'+stack,lambda m=generated,o=training,s=stack:pt.run_http(m,o,s,'train',a.tools))
            name=prefix+'PGO';manifests[name]=tuned(name,stack,'use',training/'receipt.json');heldout=directory/('http-heldout-'+stack)
            validation[stack]=step(state,a.root,'compiler-http-heldout-'+stack,
                lambda m=manifests[name],o=heldout,s=stack:pt.run_http(m,o,s,'validate',a.tools))
        for name,path in manifests.items():
            m=load(path);stack='ku' if name.startswith('K') else 'rust';binary=m['stacks'][stack]['command'][0]
            versions[name]=dict(root=str(path.parent),stack=stack,source_commit=m['candidate_base'],binary_sha256=sha(binary))
        plan=compiler_plan(versions);save(directory/'identities.json',{n:fc.version_identity(plan,n) for n in ['B','R','BD',*manifests]})
        result=execute('comparison',plan);comparisons=[]
        pairs=[('KO3','B'),('KThin','B'),('KPGO','B'),('RThin','R'),('RPGO','R'),('KO3','R'),('KThin','RThin'),('KPGO','RPGO'),('BD','B')]
        for workload in ['cpu','json_object']:
            for left,right in pairs:
                contract='direct-diagnostic' if left=='BD' else 'compiler'
                if left=='BD' and workload!='cpu':continue
                rows=[r for r in result['rows'] if r['workload']==workload and r['contract']==contract]
                groups=[{r['name']:r for r in rows if r['group']==g} for g in [1,2,3]]
                complete=[g for g in groups if left in g and right in g]
                comparisons.append({'workload':workload,'numerator':left,'denominator':right,'contract':contract,
                    'metric':'full_cpu_us','interval':fc.paired_interval([fc.metric(g[left],'full_cpu_us') for g in complete],[fc.metric(g[right],'full_cpu_us') for g in complete],minimum=3),
                    'observational_quality_passed':len(complete)==3 and all(r.get('status')=='observed' and r.get('arrival_schedule_quality')==r.get('environment_quality')=='pass' for g in complete for r in [g[left],g[right]]),
                    'goal_status':'unknown','formal_qualified':False,'default_changed':False})
        save(directory/'comparisons.json',{'comparisons':comparisons,'HTTP_train_and_heldout':validation,'goal_status':'unknown'})
        return dict(execution_complete=result['execution_complete'],goal_status='unknown',comparisons=str(directory/'comparisons.json'),
            environment_quality=environment['quality'],candidate_accepted=False,default_changed=False,
            functional_acceptance=bool(result['rows']) and all(r.get('functional_valid') is True and r.get('accounting_valid') is True for r in result['rows']))
    finally:
        fc.Harness,pt.Harness=old_fc,old_pt
        state['compiler_database_cleanup']=fixture.stop();save(a.root/'hosted-state.json',state)


def acceptance_result(final, capacity, stability, comparison_goal):
    """Capacity/recovery and continuous evidence are independent required axes."""
    rows=capacity.get('capacity',[]);scenes=[]
    for workload in ['json_object','db']:
        pair={r['name']:r for r in rows if r['workload']==workload}
        b,r=pair.get('B',{}),pair.get('R',{})
        low_b,low_r=b.get('qualified_lower_rate'),r.get('qualified_lower_rate')
        high_b=b.get('upper_rate') if b.get('boundary_observed') else None
        high_r=r.get('upper_rate') if r.get('boundary_observed') else None
        status='met' if low_b and high_r and low_b>=high_r else 'not_met' if high_b and low_r and high_b<low_r else 'unknown'
        scenes.append({'workload':workload,'goal_status':status,'B':b,'R':r})
    cap_function=len(rows)==4 and all(x.get('functional_valid') is True and x.get('accounting_valid') is True and x.get('all_recovery_passed') is True for x in rows)
    stable_rows=stability.get('rows',[])
    stable_function=bool(stable_rows) and all(r.get('natural_reclamation') is True and bool(r.get('segments')) and all(
        stream['assessment'].get('functional_valid') is True and stream['assessment'].get('accounting_valid') is True
        for segment in r['segments'] for stream in segment['streams'].values()) for r in stable_rows)
    functional=bool(final.get('rows')) and all(r.get('functional_valid') is True and r.get('accounting_valid') is True for r in final['rows']) and cap_function and stable_function
    cap_status='not_met' if any(x['goal_status']=='not_met' for x in scenes) else 'met' if all(x['goal_status']=='met' for x in scenes) else 'unknown'
    statuses=[comparison_goal,cap_status,'met' if stability.get('formal_qualified') is True else 'unknown']
    known_failure=any(r.get('functional_valid') is False or r.get('accounting_valid') is False for r in final.get('rows',[])+rows)
    goal='not_met' if known_failure or 'not_met' in statuses else 'met' if all(x=='met' for x in statuses) and functional else 'unknown'
    return {'execution_complete':final.get('execution_complete') is True and capacity.get('execution_complete') is True and stability.get('execution_complete') is True,
        'goal_status':goal,'functional_acceptance':functional,'functional_axes':{'capacity_recovery':cap_function,'continuous':stable_function},
        'capacity_goals':scenes,'stability_qualified':stability.get('formal_qualified') is True,
        'scope':'completed!=qualified!=goal met; censored capacity remains unknown; candidate adoption separate'}


def redact(value, root):
    if isinstance(value,dict):return {k:redact(v,root) for k,v in value.items() if not re.search(r'password|secret|(?:^|_)token$|db_url|conninfo|authorization',k,re.I)}
    if isinstance(value,list):return [redact(x,root) for x in value]
    if isinstance(value,str):return re.sub(r'postgres(?:ql)?://[^\s\"\x27]+','<private-db-uri-redacted>',value).replace(str(root),'${EVIDENCE_ROOT}')
    return value


def evidence_file(path, root):
    parts=path.relative_to(root).parts
    if len(parts)>2 and parts[1]=='src' and parts[2:5]!=('tests','benchmarks','cross_stack'):return False
    if 'build-time-profiles' in parts:return False
    if 'dependencies' in parts:return True
    if any(x in parts for x in ['tools','cli-cache','rust-cargo','ku-cargo','target','data','socket','.git','__pycache__']):
        return path.name=='ku' and parts[-3:]==('ku-cargo','release','ku') or path.name=='ku-cross-stack-rust' and parts[-3:] in (('rust-cargo','release','ku-cross-stack-rust'),('target','release','ku-cross-stack-rust'))
    return path.name not in ('ku-performance-v2-evidence.tar.gz','archive.json') and not path.name.endswith('.writing') and (
        path.suffix in ('.json','.ku','.c','.md','.log','.sql','.gz','.csv','.py','.lock','.toml','.rs','.go','.mod','.sum','.profdata') or (path.suffix=='.profraw' and 'http-profiles' in parts) or 'bin' in parts or path.name=='libpq.so')


def full_archive(a,state):
    destination=a.root/'ku-performance-v2-evidence.tar.gz'
    if destination.exists():raise ValueError('archive already exists; never overwrite retained evidence')
    entries=[];limit=ARCHIVE_LIMIT
    with tarfile.open(destination,'x:gz',compresslevel=6) as tar:
        for path in sorted(a.root.rglob('*')):
            if not path.is_file() or path.is_symlink() or not evidence_file(path,a.root):continue
            raw=path.read_bytes();original=hashlib.sha256(raw).hexdigest();data=raw
            if path.suffix=='.json':
                try:data=encoded(redact(json.loads(raw),a.root))
                except (ValueError,UnicodeError):data=encoded({'invalid_json_not_exported':True,'raw_sha256':original,'reason':'unparseable receipt may contain private values'})
            elif path.suffix in ('.log','.md','.py'):data=redact(raw.decode(errors='replace'),a.root).encode()
            name=path.relative_to(a.root).as_posix();info=tarfile.TarInfo(name);info.size=len(data);info.mode=path.stat().st_mode&0o777;info.mtime=0
            tar.addfile(info,io.BytesIO(data));entries.append(dict(path=name,original_sha256=original,archived_sha256=hashlib.sha256(data).hexdigest(),bytes=len(data),mode=info.mode))
            if destination.stat().st_size>limit:raise ValueError('archive exceeds frozen384MiB; no evidence silently discarded')
        raw=encoded({'files':entries,'path_root':'${EVIDENCE_ROOT}','raw_and_redacted_hashes_distinct':True})
        info=tarfile.TarInfo('portable-file-map.json');info.size=len(raw);tar.addfile(info,io.BytesIO(raw))
    if destination.stat().st_size>limit:raise ValueError('archive footer exceeded frozen limit')
    result=dict(complete=True,path=str(destination),sha256=sha(destination),bytes=destination.stat().st_size,file_count=len(entries),
        goal_status=state.get('steps',{}).get('run',{}).get('result',{}).get('goal_status','unknown'),
        caches_included=False,private_uris_redacted=True,source_scope='normal generated C/Ku inputs plus benchmark source; product Git commits in frozen plan')
    save(a.root/'archive.json',result)
    for path in [destination,a.root/'archive.json']:os.chown(path,state.get('uid',os.getuid()),state.get('gid',os.getgid()));path.chmod(0o644)
    return result


def archive(a,state):
    destination=a.root/'ku-performance-v2-evidence.tar.gz'
    if destination.exists():raise ValueError('archive exists; never overwrite')
    try:return full_archive(a,state)
    except (OSError,ValueError,tarfile.TarError) as error:
        # Raw evidence stays in ROOT; only our incomplete derived tar is replaced.
        if destination.exists():destination.unlink()
        inventory=[{'path':p.relative_to(a.root).as_posix(),'sha256':sha(p),'bytes':p.stat().st_size}
            for p in sorted(a.root.rglob('*')) if p.is_file() and not p.is_symlink() and evidence_file(p,a.root)]
        failure=redact({'complete':False,'error':repr(error),'goal_status':'blocked','raw_evidence_retained_in_ROOT':True,'inventory':inventory},a.root)
        with tarfile.open(destination,'x:gz') as tar:
            for name,value in [('archive-failure.json',failure),('hosted-state.json',redact(state,a.root))]:
                data=encoded(value);info=tarfile.TarInfo(name);info.size=len(data);tar.addfile(info,io.BytesIO(data))
        result={'complete':False,'path':str(destination),'sha256':sha(destination),'bytes':destination.stat().st_size,
            'goal_status':'blocked','error':repr(error),'raw_evidence_retained_in_ROOT':True}
        save(a.root/'archive.json',result)
        for path in [destination,a.root/'archive.json']:os.chown(path,state.get('uid',os.getuid()),state.get('gid',os.getgid()));path.chmod(0o644)
        return result


def main(argv=None):
    p=argparse.ArgumentParser(description=__doc__)
    p.add_argument('--phase',required=True,choices=['build','run','compiler','archive']);p.add_argument('--root',required=True,type=Path)
    p.add_argument('--source',required=True,type=Path);p.add_argument('--tools',required=True,type=Path)
    p.add_argument('--baseline',default=BASELINE);p.add_argument('--candidate',required=True)
    a=p.parse_args(argv);a.root=a.root.resolve();a.source=a.source.resolve(strict=a.phase!='archive');a.tools=a.tools.resolve(strict=a.phase!='archive')
    if a.phase!='archive' and (a.root==a.source or a.source in a.root.parents or a.root in a.source.parents):p.error('output must be independent of checkout')
    if any(not re.fullmatch('[0-9a-f]{40}',x) for x in [a.baseline,a.candidate]) or a.baseline!=BASELINE:p.error('frozen full commit identities required')
    if a.phase!='archive' and (platform.system()!='Linux' or platform.machine() not in ('aarch64','arm64')):p.error('actual Linux ARM64 required')
    a.root.mkdir(parents=True,exist_ok=True);identity=dict(baseline=a.baseline,candidate=a.candidate,entry_sha256=sha(__file__))
    state=load(a.root/'hosted-state.json') if (a.root/'hosted-state.json').exists() else {'identity':identity,'steps':{},'goal_status':'unknown'}
    if state['identity']!=identity and a.phase!='archive':p.error('identity changed; new root required')
    if a.phase=='archive':result=archive(a,state)
    else:
        def stop(*unused):raise KeyboardInterrupt('hosted coordinator interrupted')
        signal.signal(signal.SIGTERM,stop);signal.signal(signal.SIGALRM,stop)
        if a.phase in ('run','compiler'):signal.alarm(130*60-30)
        result=step(state,a.root,a.phase,lambda:(build if a.phase=='build' else compiler if a.phase=='compiler' else run)(a,state),resume=a.phase in ('run','compiler'))
    if a.phase!='archive':state['goal_status']=result.get('goal_status','unknown');save(a.root/'hosted-state.json',state)
    print(json.dumps(result));return 0

if __name__=='__main__':raise SystemExit(main())
