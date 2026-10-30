#!/usr/bin/env python3
"""Serial ARM64 hosted A0/B/R evidence. Builds and observations are separate from goals.
No product patch, remote dispatch, SDK matrix, installation or release operation.
"""
from __future__ import annotations
import argparse, copy, gzip, hashlib, io, json, os
from pathlib import Path
import platform, pwd, re, shutil, signal, subprocess, sys, tarfile, threading, time
import build_linux as bl
import focus_cycle as fc
import database as db
from linux_harness import ExternalDomain, GIB

ARCHIVE_LIMIT = 384<<20
BASELINE = '956727188a058119fdb3790959063f1496ccb4cc'
FOLLOWUP_PREPATCH = BASELINE
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


def diagnostic_command(argv, output, timeout=30, limit=32<<20):
    """Finite observer/tool invocation; failure is evidence, never a fake sample."""
    output=Path(output);output.parent.mkdir(parents=True,exist_ok=True)
    record=dict(command=list(map(str,argv)),stdout=str(output),stderr=str(output.with_suffix(output.suffix+'.log')))
    process=None;began=time.monotonic()
    try:
        with output.open('xb') as out,Path(record['stderr']).open('xb') as err:
            process=subprocess.Popen(record['command'],stdout=out,stderr=err,
                stdin=subprocess.DEVNULL,start_new_session=True)
            while process.poll() is None:
                if time.monotonic()-began>timeout or out.tell()+err.tell()>limit:
                    raise RuntimeError('diagnostic tool time/output bound exceeded')
                time.sleep(.05)
            record['exit_code']=process.returncode
            if out.tell()+err.tell()>limit:raise RuntimeError('diagnostic tool output bound exceeded before exit')
    except OSError as error:record['error']=repr(error)
    except RuntimeError as error:record['error']=str(error)
    finally:
        if process is not None and process.poll() is None:
            os.killpg(process.pid,signal.SIGKILL);process.wait(timeout=5)
        record.update(seconds=time.monotonic()-began,complete=process is not None and
            process.returncode==0 and 'error' not in record)
    save(output.with_suffix(output.suffix+'.json'),record);return record


def diagnostic_plan(versions):
    """A single short identification experiment, never the formal matrix."""
    return dict(schema='ku-hosted-followup-diagnostic-v1', versions=versions,
        resources=RESOURCES, quality_policy=POLICY,
        cases=[dict(workload=w, rate=r, reference=ref, contract=contract)
               for w,r,ref,contract in [('ok',2000,'R','text'),
                   ('json_object',500,'R','dom'),('db',200,'RS','pg-strict')]],
        order=['reference-before','normal-before','ledger-sample','normal-after','reference-after'],
        warmup_seconds=3, score_seconds=15, perf_frequency=499,
        stop_after_consecutive_environment_failures=2, maximum_additional_anomaly_windows=0,
        no_replacement=True, formal_qualified=False, candidate_accepted=False,
        scope='Current normal Release helpers/text/async/int versus Rust; ledger+roles only in D. '
              'No A0, typed/direct alternatives, compiler variants, capacity or 30-minute acceptance. '
              'D CPU is diagnostic only; inclusive samples overlap and cannot be added. '
              'Exit ledger covers readiness, warmup, score and drain, not just score requests.')


def followup_plan(versions, prepatch=FOLLOWUP_PREPATCH):
    plan=diagnostic_plan(versions)
    plan.update(schema='ku-hosted-followup-screen-v1',prepatch=prepatch,
        paired_orders=[['A','B','reference'],['B','reference','A'],['reference','A','B']],
        environment_controls=dict(name='A',workload='json_object',rate=500,warmup_seconds=5,score_seconds=10,count=3),
        maximum_windows=33,formal_baseline_executed=False,
        latency_slo_ms={'ok':100,'json_object':100,'db':250},
        scope='Three prepatch environment controls, then3 paired normal A/B/R turns per business and at most1 B-ledger D. '
              'A is the explicitly frozen prepatch, not historical formal A0=988f32e. Normal CLI C11O2/helpers/text/async/int only. '
              'No formal/capacity/stability score; candidate adoption requires valid measured normal A/B benefit, not counters.')
    variants=sorted(n for n in versions if re.fullmatch(r'C[1-7]',n))
    fixed=['BF'] if 'BF' in versions else []
    plan['candidate_versions']=['B',*variants,*fixed]
    if variants or fixed:
        names=['A','B',*variants,*fixed,'reference']
        plan['paired_orders']=[names,names[1:]+names[:1],names[2:]+names[:2]]
        plan['maximum_windows']=3+len(plan['cases'])*(3*len(names)+1)
        plan['scope']+=' Additional C1..C7 are preregistered normal commit ablations, not selected post hoc. BF is the identical B binary with explicit fixed workers.'
    if 'C1' in versions and 'C2' in versions:
        names=['A','C1','C2','B','reference']
        plan['cases'].append(dict(workload='json_escape',rate=500,reference='R',contract='escaped-unicode',
            paired_orders=[names,names[2:]+names[:2],names[4:]+names[:4]],ledger=False))
        plan['latency_slo_ms']['json_escape']=100
    plan['cases'].append(dict(workload='cpu',rate=80,reference='R',contract='compute-guard',
        paired_orders=[['A','B','reference'],['B','reference','A'],['reference','A','B']],ledger=False))
    plan['latency_slo_ms']['cpu']=1000
    if fixed:
        plan['worker_growth']=dict(names=['B','BF'],orders=[['B','BF'],['BF','B'],['B','BF']],
            cold=dict(rate=500,seconds=3),burst=dict(rate=8000,seconds=5),warmup_seconds=0,
            workload='json_object',maximum_p99_ms=100,diagnostic_only=True,normal_score_eligible=False)
    if 'HC' in versions:
        plan['transport_control']=dict(name='HC',workload='json_object',rate=500,warmup_seconds=3,score_seconds=15,
            diagnostic_only=True,normal_score_eligible=False,
            scope='Same1048bytebody/1071byteresponseandworkerpath;omitsTransform;notJSON-onlytimeorranking')
    plan['maximum_windows']=3+sum(len(diagnostic_turns(plan,c,True)) for c in plan['cases'])
    if fixed:plan['maximum_windows']+=12
    if 'HC' in versions:plan['maximum_windows']+=1
    return plan


def diagnostic_turns(plan,case,followup):
    if not followup:
        return [(turn,case['reference'] if turn.startswith('reference') else 'D' if turn=='ledger-sample' else 'B')
                for turn in plan['order']]
    turns=[(f'group-{i+1}-{name}',case['reference'] if name=='reference' else name)
        for i,order in enumerate(case.get('paired_orders',plan['paired_orders'])) for name in order]
    if case.get('ledger',True):turns.append(('ledger-sample','D'))
    return turns


def diagnostic_axes(plan,rows,expected,stopped,control,growth,followup):
    normal=len(rows)==expected and not stopped
    qualified=not followup or all(r.get('diagnostic_only') or
        (r.get('functional_valid') is True and r.get('accounting_valid') is True
         and r.get('all_correct') is True and r.get('natural_reclamation') is True
         and r.get('arrival_schedule_quality')=='pass' and r.get('environment_quality')=='pass'
         and not r.get('measurement_invalid') and not r.get('observed_environment_faults')) for r in rows)
    layers=not followup or plan.get('json_layer_diagnosis',{}).get('execution_complete') is True
    control_ok=not followup or (isinstance(control,dict) and control.get('functional_valid') is True
        and control.get('accounting_valid') is True and control.get('all_correct') is True
        and control.get('natural_reclamation') is True and control.get('arrival_schedule_quality')=='pass'
        and not control.get('measurement_invalid') and not control.get('observed_environment_faults'))
    growth_ok=not plan.get('worker_growth') or len(growth)==12 and all(r.get('valid') for r in growth)
    return dict(normal_windows_complete=normal,normal_windows_qualified=qualified,json_layers_complete=layers,
        transport_control_complete=control_ok,worker_growth_complete=growth_ok,
        execution_complete=normal and qualified and layers and control_ok and growth_ok)


def build_ledger(a):
    import http_cost_diagnostic as hcd
    m=copy.deepcopy(load(a.root/'B/manifest.json'));build=m['stacks']['ku']['build']
    normal=build['native']['server'];directory=a.root/'D/build';directory.mkdir(parents=True)
    ledger=directory/'ledger.c';source=directory/'roles.c';binary=directory/'bin/ku-server'
    binary.parent.mkdir()
    receipt=hcd.prepare_ledger_source(Path(normal['generated_c']),ledger,source_commit=a.candidate)
    save(directory/'ledger-identity.json',receipt)
    roles=hcd.prepare_source(ledger,source,source_commit=a.candidate)
    save(directory/'roles-identity.json',roles)
    invocation=normal['compiler_invocation'];compiler=Path(invocation['compiler']['path'])
    if sha(compiler)!=invocation['compiler']['sha256']:raise ValueError('normal compiler identity changed')
    libraries=[x['selected_input']['real_path'] for x in invocation['library_inputs']]
    for item in invocation['library_inputs']:
        if sha(item['selected_input']['real_path'])!=item['selected_input']['sha256']:
            raise ValueError('normal compiler library identity changed')
    argv=[str(compiler),str(source),*invocation['flags'],*libraries,'-g',
          '-Wl,--wrap=pthread_create','-o',str(binary)]
    completed=diagnostic_command(argv,directory/'compiler.log',timeout=120)
    record=dict(command=argv,tool_result=completed,compiler=invocation['compiler'],
        normal_c=bl.file_identity(normal['generated_c']),ledger_c=bl.file_identity(ledger),
        roles_c=bl.file_identity(source),source_commit=a.candidate,diagnostic_only=True,
        normal_score_eligible=False,normal_flags=invocation['flags'],additional_flags=['-g','-Wl,--wrap=pthread_create'])
    save(directory/'compiler.json',record)
    if not completed['complete']:raise RuntimeError('ledger compile failed; see D/build/compiler.log')
    for label,command in [('assembly',['llvm-objdump-18','--disassemble','--source','--line-numbers','--demangle',str(binary)]),
                          ('symbols',['nm','-n','-S','--defined-only',str(binary)]),
                          ('elf',['readelf','-h','-S','-n',str(binary)])]:
        diagnostic_command(command,directory/(label+'.log'),timeout=60,limit=64<<20)
    m['stacks']['ku']['command']=[str(binary)]
    build['native']['server']=dict(normal,generated_c=str(source),generated_c_sha256=sha(source),
        binary=str(binary),binary_sha256=sha(binary),diagnostic_receipt=str(directory/'compiler.json'))
    build['native_flags']['server']=[*invocation['flags'],'-g','-Wl,--wrap=pthread_create']
    build['diagnostic']=record;m.update(diagnostic_only=True,normal_score_eligible=False)
    save(a.root/'D/manifest.json',m)
    return dict(root=str(a.root/'D'),stack='ku',source_commit=a.candidate,binary_sha256=sha(binary),
        artifacts={name:dict(path=str(path),sha256=sha(path)) for name,path in
                   [('normal C',Path(normal['generated_c'])),('ledger C',ledger),('roles C',source)]})


def build_json_layers(a):
    """Actual generated Transform and same-volume transport control; never rankings."""
    import json_dense as jd
    records=[]
    try:
        for name in ('A','B'):
            m=load(a.root/name/'manifest.json');build=m['stacks']['ku']['build']
            normal=build['native']['server'];invocation=normal['compiler_invocation']
            compiler=invocation['compiler'];libraries=invocation['library_inputs']
            if sha(compiler['path'])!=compiler['sha256']:raise ValueError('JSON diagnostic compiler changed')
            for item in libraries:
                if sha(item['selected_input']['real_path'])!=item['selected_input']['sha256']:
                    raise ValueError('JSON diagnostic library changed')
            common=['--generated-c',normal['generated_c'],'--source-commit',m['candidate_base'],
                '--cli-sha256',sha(build['cli']),'--cc',compiler['path']]
            for item in libraries:common+=['--link',item['selected_input']['real_path']]
            for mode,extra in [('plain',[]),('clocks',['--phase-clocks']),('allocations',['--phase-ledger'])]:
                output=a.root/'json-layers'/name/mode
                jd.main([*common,'--output',str(output),*extra,'--family','http-object',
                    '--iterations','4096' if mode=='allocations' else '32768','--wall-seconds','30',
                    '--runner-prefix','taskset -c 0'])
                reports=sorted(output.glob('result-*.json'))
                if len(reports)!=1 or not load(reports[0]).get('passed'):raise ValueError('JSON diagnostic did not validate')
                records.append(dict(name=name,mode=mode,result=str(reports[0]),sha256=sha(reports[0])))
            if name=='B':
                output=a.root/'json-layers'/'http-control'
                jd.main([*common,'--output',str(output),'--http-control'])
                control=load(output/'emission.json');binary=output/'http-control';source=output/'http-control.c'
                m=copy.deepcopy(m);m.update(diagnostic_only=True,normal_score_eligible=False,
                    diagnostic_role='same-wire-volume-sync-worker-control-without-Transform')
                m['stacks']['ku']['command']=[str(binary)]
                build=m['stacks']['ku']['build'];build['native']['server']=dict(normal,
                    generated_c=str(source),generated_c_sha256=sha(source),binary=str(binary),binary_sha256=sha(binary),
                    diagnostic_receipt=str(output/'emission.json'))
                build['diagnostic']=control;save(a.root/'HC/manifest.json',m)
        return dict(execution_complete=True,records=records,transport_control='HC',diagnostic_only=True,goal_status='unknown')
    except (OSError,ValueError,RuntimeError,subprocess.SubprocessError) as error:
        # A derived diagnostic failure cannot turn a normal product window into a pass or stop unrelated measurements.
        return dict(execution_complete=False,records=records,error=repr(error),diagnostic_only=True,goal_status='blocked')


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
            if name in ('A0','A','B') or re.fullmatch(r'C[1-7]',name):b.ku()
            elif name=='R':b.rust();b.go_build(client=True)
            else:b.ku_native(Path(load(a.root/'B/manifest.json')['stacks']['ku']['build']['cli']))
            return b.finish()
        except BaseException as e:b.finish(failed=repr(e));raise
    build_set=getattr(a,'build_set','full');diagnostic=build_set in ('diagnostic','followup-screen')
    prepatch=getattr(a,'prepatch',FOLLOWUP_PREPATCH)
    variants=getattr(a,'screen_variants',[])
    builds=([('A',prepatch,None),*[(f'C{i+1}',commit,None) for i,commit in enumerate(variants)]] if build_set=='followup-screen' else [])+[
        ('B',a.candidate,None),('R',a.candidate,None)] if diagnostic else [('A0',a.baseline,None),('B',a.candidate,None),('R',a.candidate,None),
        ('BJ',a.candidate,{'ku_json':'typed'}),('BP',a.candidate,{'ku_pg_read':'typed'}),('BD',a.candidate,{'ku_format':'direct'})]
    for name,commit,mode in builds:
        step(state,a.root,'build-'+name,lambda n=name,c=commit,m=mode:one(n,c,m))
    rm=load(a.root/'R/manifest.json');client=rm['client'];protocol=Path(rm['protocol']);versions={}
    b_normal=load(a.root/'B/manifest.json')['stacks']['ku']['build']['native']['server']
    fixed_control=build_set=='followup-screen' and 'KU_HTTP_WORKER_POLICY' in Path(b_normal['generated_c']).read_text()
    names=((['A',*[f'C{i+1}' for i in range(len(variants))]] if build_set=='followup-screen' else [])
        +['B',*(['BF'] if fixed_control else []),'R','RS'] if diagnostic else ['A0','B','R','BJ','BP','BD','RS','RJ','RP','RPS'])
    for name in names:
        d=a.root/name;is_r=name.startswith('R');m=load(a.root/('R' if is_r else 'B' if name=='BF' else name)/'manifest.json')
        m=normalize_manifest(m,protocol,client);stack='rust' if is_r else 'ku';m['stacks']={stack:m['stacks'][stack]}
        if not is_r and 'KU_HTTP_WORKER_POLICY' in Path(m['stacks']['ku']['build']['native']['server']['generated_c']).read_text():
            m['declared_worker_policy']='fixed' if name=='BF' else 'elastic'
            m['service_env']={**m['service_env'],'KU_HTTP_WORKER_POLICY':m['declared_worker_policy']}
        elif not is_r:m['declared_worker_policy']='legacy-fixed'
        if is_r:m['stacks'][stack]['env'].update(BENCH_REQUEST_TIMEOUT_MS='30000',BENCH_LISTEN_BACKLOG='4096',
            BENCH_PG_MODE='reset-equivalent' if name in ('RS','RPS') else 'normal',
            BENCH_PG_API='typed-text-key' if name in ('RP','RPS') else 'normal',BENCH_JSON_MODE='typed' if name=='RJ' else 'generic')
        if name=='BD':m.update(diagnostic_only=True,normal_score_eligible=False)
        save(d/'manifest.json',m);binary=m['stacks'][stack]['command'][0]
        versions[name]=dict(root=str(d),stack=stack,source_commit=m['candidate_base'],binary_sha256=sha(binary))
    fixture=normalize_manifest(copy.deepcopy(load(a.root/'B/manifest.json')),protocol,client)
    fixture['stacks']={};save(a.root/'fixture/manifest.json',fixture)
    if diagnostic:versions['D']=step(state,a.root,'build-ledger',lambda:build_ledger(a))
    layers=None
    if build_set=='followup-screen':
        layers=step(state,a.root,'build-json-layers',lambda:build_json_layers(a))
        if layers.get('execution_complete') is True:
            m=load(a.root/'HC/manifest.json');versions['HC']=dict(root=str(a.root/'HC'),stack='ku',
                source_commit=m['candidate_base'],binary_sha256=sha(m['stacks']['ku']['command'][0]),
                diagnostic_only=True,normal_score_eligible=False)
    plan=followup_plan(versions,prepatch) if build_set=='followup-screen' else diagnostic_plan(versions) if diagnostic else frozen_plan(versions)
    if layers is not None:plan['json_layer_diagnosis']=layers
    save(a.root/'frozen-plan.json',plan)
    identities={n:fc.version_identity(plan,n) for n in versions};save(a.root/'build-identities.json',identities)
    if build_set=='followup-screen':
        for name in plan['candidate_versions']:
            if identities['A']['files']['expanded source']['sha256']!=identities[name]['files']['expanded source']['sha256']:
                raise ValueError('paired normal Ku business inputs differ: '+name)
        if bl.benchmark_stack_sources(a.root/'A/src/tests/benchmarks/cross_stack/rust')!=load(a.root/'R/manifest.json')['stacks']['rust']['build']['source_files_sha256']:
            raise ValueError('reference business source differs from frozen prepatch')
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
    return {'execution_complete':True,'goal_status':'unknown','plan_sha256':sha(a.root/'frozen-plan.json'),'json_layer_diagnosis':layers,
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


def run(a,state,tail_only=False,stability_only=False):
    if stability_only and not tail_only:raise ValueError('stability-only requires tail scope')
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
    def blocked(reason,axes):
        pending=['final','capacity','stability'] if 'final' not in axes and not tail_only else ['capacity','stability']
        if stability_only:pending=['stability']
        state['deferred']={label:reason for label in pending}
        return dict(execution_complete=False,goal_status='blocked',candidate_accepted=False,
            environment_quality=environment['quality'],fixed_matrix_executed='final' in axes,
            axes=axes,reason=reason,scope='Retained original windows; downstream measurements not started or retried.')
    try:
        fixture.start();private=load(a.root/'fixture/manifest.json')
        private['database']['cpu_set']=load(fixture.state_path)['limits']['cpuset.cpus'];save(fixture.manifest_path,private)
        fc.Harness=HostedHarness
        fixed=copy.deepcopy(plan);fixed.pop('capacity',None);fixed.pop('stability',None)
        control=copy.deepcopy(fixed);control.update(targets=[],schedule=[dict(phase='control',workload='json_object',rate=500,
            orders=[['A0'],['A0'],['A0']],warmup_seconds=5,score_seconds=10)])
        control['stop_after_consecutive_unusable_windows']=2
        summary=execute('control',control,'all');environment=environment_record(summary,a.root/'environment-qualification.json')
        if environment['quality']!='pass':
            return blocked('environment control quality is '+environment['quality'],{'control':summary})
        if summary.get('execution_complete') is not True:
            return blocked('environment control execution incomplete',{'control':summary})
        final=None if tail_only else execute('final',fixed,'all')
        checked=summary if tail_only else final
        function_ok=(checked.get('execution_complete') is True and bool(checked.get('rows')) and all(
            r.get('status')=='observed' and r.get('functional_valid') is True and r.get('accounting_valid') is True
            and r.get('arrival_schedule_quality')=='pass' and (tail_only or r.get('environment_quality')=='pass')
            and r.get('measurement_invalid') is False and r.get('natural_reclamation') is True for r in checked['rows']))
        if not function_ok:
            return blocked('fixed/control measurement incomplete, invalid or function/accounting/cleanup failed',
                {'control':summary,**({'final':final} if final is not None else {})})
        if function_ok:
            def capacities():
                results=[]
                for kind in plan['capacity']['workloads']:
                    config=copy.deepcopy(plan);config['capacity']['workloads']=[kind]
                    config['capacity']['maximum_rate']=plan['capacity']['maximum_rates'][kind]
                    results.append(execute('capacity-'+kind,config,'capacity'))
                return {'capacity':[r for result in results for r in result['capacity']],
                    'execution_complete':all(r.get('execution_complete') is True for r in results),'goal_status':'unknown'}
            if not stability_only:step(state,a.root,'capacity',capacities,resume=True)
            if not state['steps'].get('stability'):
                execute('stability',plan,'stability') # once-only; no resume/replacement of continuous evidence
        if tail_only:
            capacity=state['steps'].get('capacity',{}).get('result',{})
            continuous=state['steps'].get('stability',{}).get('result',{})
            required=[summary,continuous] if stability_only else [summary,capacity,continuous]
            axes={'control':summary,'stability':continuous}
            if not stability_only:axes['capacity']=capacity
            return {'execution_complete':all(x.get('execution_complete') is True for x in required),
                'goal_status':'unknown','fixed_matrix_executed':False,'cross_host_pooling':False,
                'capacity_executed':not stability_only,'axes':axes,
                'environment_quality':environment['quality'],'candidate_accepted':False,
                'scope':'tail on this instance only; no fixed-matrix rows imported, fabricated or pooled across hosts'}
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


def diagnostic_context(service):
    """Instantaneous bounded Linux evidence; a wait stack is not a CPU sample."""
    def read(path):
        try:
            with Path(path).open('rb') as stream:return stream.read(65536).decode(errors='replace')
        except OSError as error:return {'unavailable':repr(error)}
    proc=Path('/proc')/str(service.process.pid)
    try:tasks=sorted(proc.joinpath('task').iterdir(),key=lambda p:int(p.name))
    except OSError:tasks=[]
    return dict(monotonic_ns=time.monotonic_ns(),wall_time_ns=time.time_ns(),
        vmstat=read('/proc/vmstat'),meminfo=read('/proc/meminfo'),host_cpu=read('/proc/stat'),
        service_status=read(proc/'status'),smaps_rollup=read(proc/'smaps_rollup'),
        task_count=len(tasks),task_limit=128,
        waits=[dict(tid=int(task.name),stack=read(task/'stack'),wchan=read(task/'wchan'),
                    syscall=read(task/'syscall'),schedstat=read(task/'schedstat')) for task in tasks[:128]],
        scope='Live snapshots only. Host swap deltas do not identify service swap ownership; '
              'major/minor faults are not proof of swap. Wait snapshots do not measure wait duration.')


def diagnostic_memory(service, output):
    """Endpoint-only mapping evidence; never add overlapping accounting axes."""
    output=Path(output);output.parent.mkdir(parents=True,exist_ok=True);limit=4<<20
    with (Path('/proc')/str(service.process.pid)/'smaps').open('rb') as stream:raw=stream.read(limit+1)
    truncated=len(raw)>limit;raw=raw[:limit];output.write_bytes(raw)
    try:executable=os.readlink(Path('/proc')/str(service.process.pid)/'exe')
    except OSError:executable=None
    metrics={'Rss':'rss_bytes','Pss':'pss_bytes','Anonymous':'anonymous_bytes',
             'Private_Dirty':'private_dirty_bytes','Swap':'swap_bytes'}
    mappings=[];current=None
    for line in raw.decode(errors='replace').splitlines():
        match=re.match(r'^([0-9a-f]+)-([0-9a-f]+)\s+(\S+)\s+([0-9a-f]+)\s+\S+\s+\d+\s*(.*)$',line)
        if match:
            if current is not None:mappings.append(current)
            start,end,permissions,offset,name=match.groups()
            category=('named_heap' if name=='[heap]' else 'named_stack' if name.startswith('[stack') else
                'executable_file' if name and name==executable else 'shared_library_file' if name.startswith('/') and '.so' in name else
                'other_file' if name.startswith('/') else 'special_named' if name.startswith('[') else 'anonymous_mapping')
            current=dict(start=start,end=end,permissions=permissions,file_offset=offset,name=name,category=category)
        elif current is not None and ':' in line:
            key,value=line.split(':',1)
            if key in metrics:
                parts=value.split()
                if len(parts)==2 and parts[1]=='kB':current[metrics[key]]=int(parts[0])*1024
    if current is not None and not truncated:mappings.append(current)
    stack_threads=[];stack_ranges=set()
    try:tasks=sorted((Path('/proc')/str(service.process.pid)/'task').iterdir(),key=lambda p:int(p.name))
    except OSError:tasks=[]
    for task in tasks[:128]:
        item={'tid':int(task.name)}
        try:
            with task.joinpath('syscall').open('rb') as stream:raw_syscall=stream.read(4097)
            if len(raw_syscall)>4096:raise ValueError('syscall snapshot exceeded4096B bound')
            syscall=raw_syscall.decode(errors='replace').split()
            if len(syscall)<3 or syscall[0]=='running':raise ValueError('thread running or syscall SP unavailable')
            sp=int(syscall[-2],0)
            matches=[m for m in mappings if int(m['start'],16)<=sp<int(m['end'],16)]
            if len(matches)!=1:raise ValueError('SP has no unique retained VMA')
            m=matches[0];stack_ranges.add((m['start'],m['end']))
            item.update(stack_pointer=hex(sp),start=m['start'],end=m['end'],
                rss_bytes=m.get('rss_bytes'),pss_bytes=m.get('pss_bytes'),anonymous_bytes=m.get('anonymous_bytes'),
                attribution='SP-containing VMA; stack/TLS/TCB may share this mapping and are not split')
        except (OSError,ValueError) as error:item['unknown']=str(error)
        stack_threads.append(item)
    for m in mappings:
        if (m['start'],m['end']) in stack_ranges:m['category']='thread_stack_or_TLS'
    groups={}
    for item in mappings:
        group=groups.setdefault(item['category'],dict(mapping_count=0,**{v:0 for v in metrics.values()}))
        group['mapping_count']+=1
        for metric in metrics.values():group[metric]+=item.get(metric,0)
    return dict(raw_path=str(output),raw_sha256=sha(output),bytes=len(raw),limit_bytes=limit,
        complete=not truncated,truncated=truncated,categories=groups,mappings=mappings,
        thread_stack_VMA_evidence=stack_threads,thread_stack_VMA_unique=len(stack_ranges),
        scope='Endpoint mapping RSS/PSS/Anonymous/PrivateDirty/Swap are separate, overlapping views. '
              'Do not add them together, or add cgroup kernel and its kernel_stack/page-table/slab subfields. '
              'Anonymous VMAs can contain allocator mmap, pthread stacks and zero-fill BSS tails; smaps labels alone '
              'cannot distinguish them. Named heap excludes mmap allocations; virtual reserved size is not resident memory. '
              'D TLS counter bank and dump buffer are instrumentation BSS, not normal product memory.')


def diagnostic_symbols(service, output):
    """Preserve mapped ELFs before Service.stop removes its executable copy."""
    output=Path(output);symfs=output/'symfs';symfs.mkdir()
    with (Path('/proc')/str(service.process.pid)/'maps').open('rb') as stream:raw=stream.read((1<<20)+1)
    truncated=len(raw)>1<<20;raw=raw[:1<<20]
    (output/'maps.log').write_bytes(raw);files=[];issues=[];total=0;seen=set()
    if truncated:issues.append({'reason':'1MiB maps read bound; snapshot incomplete'})
    for line in raw.decode(errors='replace').splitlines():
        columns=line.split(None,5)
        if len(columns)!=6 or not columns[5].startswith('/'):continue
        name=columns[5]
        if name in seen:continue
        seen.add(name);source=Path(name)
        if '..' in source.parts or name.endswith(' (deleted)'):
            issues.append(dict(path=name,reason='not a current canonical mapped file'));continue
        try:
            with source.open('rb') as stream:magic=stream.read(4)
            if magic!=b'\x7fELF':continue
            size=source.stat().st_size
            if len(files)>=64 or total+size>128<<20:
                issues.append(dict(path=name,reason='64ELF/128MiB symbol snapshot bound'));continue
            destination=symfs/name.lstrip('/');destination.parent.mkdir(parents=True,exist_ok=True)
            try:os.link(source,destination)
            except OSError:shutil.copyfile(source,destination)
            expected=sha(source)
            if sha(destination)!=expected:raise ValueError('symbol ELF snapshot SHA mismatch')
            files.append(dict(mapped_path=name,copy=str(destination),bytes=size,sha256=expected));total+=size
        except (OSError,ValueError) as error:issues.append(dict(path=name,reason=repr(error)))
    record=dict(symfs=str(symfs),files=files,issues=issues,bytes=total,complete=not issues,
        scope='Exact mapped ELF snapshots for --symfs, including the later-deleted runtime executable. '
              'Kernel/vDSO symbols and unreadable mappings remain unknown; no implicit build-id cache assumption.')
    save(output/'symbol-identities.json',record);return record


def diagnostic_observe(call):
    try:return call()
    except Exception as error:return {'observer_error':repr(error),'complete':False}


def perf_capability(perf, directory):
    """One <=18s session; never repeat unsupported sampling for each business."""
    directory=Path(directory);directory.mkdir();began=time.monotonic();attempts=[]
    result=dict(available=False,callchain_available=False,attempts=attempts,
        session_budget_seconds=18,scope='CPU-event sampling capability only, not a service benchmark. '
              'Generic EOPNOTSUPP does not uniquely identify a physical PMU or guest-kernel cause.')
    full=['--call-graph','dwarf,8192','--inherit','--switch-events']
    workload='import time\nend=time.monotonic()+1\nx=1\nwhile time.monotonic()<end: x=(x*1664525+1013904223)&0xffffffff'
    def attempt(event,flags,label):
        remaining=18-(time.monotonic()-began)
        if remaining<=0:return False
        out=directory/label;out.mkdir();raw=out/'perf.data'
        row=dict(event=event,flags=flags)
        row['record']=diagnostic_command([perf,'record','--clockid','mono','-e',event,'-F','499',
            *flags,'-o',str(raw),'--',sys.executable,'-c',workload],out/'record.log',timeout=min(4,remaining),limit=1<<20)
        remaining=18-(time.monotonic()-began)
        if row['record']['complete'] and raw.is_file() and raw.stat().st_size and remaining>0:
            row['verify']=diagnostic_command([perf,'script','-i',str(raw),'-G','-F','event'],
                out/'events.log',timeout=min(2,remaining),limit=1<<20)
            if row['verify']['complete']:
                row['sample_count']=sum(event in line for line in (out/'events.log').read_text(errors='replace').splitlines())
        attempts.append(row)
        return row.get('sample_count',0)>0
    if perf:
        for event in ['cpu-clock','task-clock','cycles']:
            if attempt(event,[],event+'-minimal'):
                with_chain=attempt(event,full,event+'-full')
                result.update(available=True,event=event,selected_flags=full if with_chain else [],
                    callchain_available=with_chain,method='CPU_SELF_plus_DWARF' if with_chain else 'CPU_SELF_only',
                    interpretation='Clock samples weight CPU time; cycles samples weight cycles. Neither is elapsed blocking time.')
                break
    else:result['reason']='perf executable unavailable'
    if not result['available']:result['reason']='No tested minimal CPU event produced samples; subsequent D skips perf; runtime SELF unknown.'
    result['seconds']=time.monotonic()-began;save(directory/'capability.json',result);return result


def window_quality(row):
    faults=[];path=Path(row.get('score_report',''))
    if path.is_file():
        cost=load(path).get('service_cost',{});samples=Path(cost.get('samples_path',''))
        groups={}
        if samples.is_file():
            for sample in load(samples).get('samples',[]):
                for domain in sample.get('domains',[]):groups.setdefault(domain['label'],[]).append(domain)
        for label,values in groups.items():
            for group,key in [('cpu_stat','throttled_usec'),('memory_stat','pgmajfault'),('memory_events','oom'),('memory_events','oom_kill')]:
                points=[v.get(group,{}).get(key) for v in values]
                if len(points)>=2 and all(type(x)is int for x in points) and max(points)>min(points):faults.append(label+':'+key)
    row['observed_environment_faults']=faults
    signature=[row.get(k) for k in ('functional_valid','accounting_valid','measurement_invalid','arrival_schedule_quality','environment_quality')]+[sorted(faults)]
    invalid=(row.get('functional_valid') is False or row.get('accounting_valid') is False or
        row.get('measurement_invalid') is True or row.get('arrival_schedule_quality')!='pass' or row.get('environment_quality')=='fail' or bool(faults))
    return invalid,signature


def screen_pairs(rows,case,slo,numerator='B',denominator='A'):
    pairs=[];good=[]
    for group in range(1,4):
        selected={r['name']:r for r in rows if r['turn'].startswith('group-%d-'%group)}
        a,b,ref=(selected.get(n) for n in (denominator,numerator,case['reference']))
        if not all((a,b,ref)):continue
        valid=all(r.get('functional_valid') is True and r.get('accounting_valid') is True and r.get('all_correct') is True
            and r.get('natural_reclamation') is True and r.get('arrival_schedule_quality')=='pass'
            and r.get('environment_quality')=='pass' and not r.get('observed_environment_faults') for r in (a,b,ref))
        valid=valid and a.get('comparison_contract')==b.get('comparison_contract')==ref.get('comparison_contract')
        def ratio(x,y):return x/y if type(x)in(int,float) and type(y)in(int,float) and x>0 and y>0 else None
        p99a,p99b=(r.get('latency',{}).get('p99_ms') for r in (a,b))
        pair=dict(group=group,eligible=valid,cpu_B_over_A=ratio(b.get('full_cpu_us'),a.get('full_cpu_us')),
            cpu_B_over_R=ratio(b.get('full_cpu_us'),ref.get('full_cpu_us')),
            memory_B_over_A=ratio(b.get('memory',{}).get('steady_median_bytes'),a.get('memory',{}).get('steady_median_bytes')),
            p99_A_ms=p99a,p99_B_ms=p99b,p99_B_over_A=ratio(p99b,p99a),absolute_p99_slo_ms=slo,
            p99_absolute_pass=all(type(x)in(int,float) and x<=slo for x in (p99a,p99b)))
        pairs.append(pair)
        if valid and pair['cpu_B_over_A'] is not None:good.append((b['full_cpu_us'],a['full_cpu_us']))
    ci=fc.paired_interval([p[0] for p in good],[p[1] for p in good],minimum=3)
    return dict(numerator_name=numerator,denominator_name=denominator,ratio_keys_B_A_alias_for_declared_names=True,pairs=pairs,valid_pair_count=len(good),cpu_paired_bootstrap95=ci,
        all_three_cpu_directions_lower=len(good)>=3 and all(x<y for x,y in good),
        cpu_upper95_below_one=ci.get('status')=='estimated' and ci['upper95']<1,
        confidence_note='Only3 short paired windows; percentile bootstrap is descriptive and stationarity is unproven. '
                        'No average of p99 values; resource/tail regressions and cancellation regressions require root review.',
        adopted=False,formal_qualified=False)


def diagnostic(a,state,followup=False):
    import http_cost_diagnostic as hcd
    if os.geteuid()!=0:raise ValueError('diagnostic requires root cgroup control')
    plan=load(a.root/'frozen-plan.json')
    expected='ku-hosted-followup-screen-v1' if followup else 'ku-hosted-followup-diagnostic-v1'
    if plan.get('schema')!=expected:raise ValueError('matching diagnostic/screen build-set required')
    boot=sha('/proc/sys/kernel/random/boot_id')
    if state.setdefault('diagnostic_boot_sha256',boot)!=boot:raise ValueError('cross-host diagnostic resume forbidden')
    directory=a.root/'diagnostic';directory.mkdir(exist_ok=False)
    fixture=db.Database(a.root/'fixture',state['uid'],state['gid']);original=fc.Harness;private=None
    state['execution_environment']=dict(kernel=platform.release(),machine=platform.machine(),
        cpu_count=os.cpu_count(),coordinator_affinity=sorted(os.sched_getaffinity(0)),
        parent_cgroup_limits={n:(Path('/sys/fs/cgroup')/n).read_text().strip() for n in
            ['cpu.max','memory.max','memory.swap.max'] if (Path('/sys/fs/cgroup')/n).exists()},
        kernel_observation_controls={n:Path('/proc/sys/kernel',n).read_text().strip() for n in
            ['perf_event_paranoid','kptr_restrict','sched_schedstats'] if Path('/proc/sys/kernel',n).exists()})
    diagnostic_command(['ps','-eo','pid,ppid,nlwp,rss,stat,comm'],directory/'processes-before.log')
    perf=shutil.which('perf',path=str(a.tools/'bin')+os.pathsep+os.environ.get('PATH',''))
    if perf:diagnostic_command([perf,'version'],directory/'perf-version.log')
    rows=[];stopped=[];environment=None;control_rows=[];capability=None
    class DiagnosticHarness(original):
        def __init__(self,*args,**kwargs):
            super().__init__(*args,**kwargs);self.manifest=copy.deepcopy(self.manifest)
            self.manifest['db_url']=private['db_url'];self.db_domain=ExternalDomain(private['db_cgroup'])
            if environment:self.manifest['environment_qualification']=environment
        def client(self,service,workload,rate,duration,output,**kwargs):
            if Path(output).name!='score':
                return super().client(service,workload,rate,duration,output,**kwargs)
            output=Path(output);output.mkdir(parents=True,exist_ok=True);stop=threading.Event()
            if self.root!=a.root/'D':
                def endpoint(name):
                    return dict(context=diagnostic_observe(lambda:diagnostic_context(service)),
                        threads=diagnostic_observe(lambda:hcd.snapshot(service.process.pid,service.domain.path)),
                        memory=diagnostic_observe(lambda:diagnostic_memory(service,output/(name+'-smaps.log'))))
                before=endpoint('before')
                try:return super().client(service,workload,rate,duration,output,**kwargs)
                finally:save(output/'control-endpoints.json',dict(before=before,
                    after=endpoint('after'),
                    no_perf_or_product_instrumentation=True,
                    scope='Two external endpoint snapshots, outside timed driver; existing1Hz resource sampler remains.'))
            record={'diagnostic_only':True,'normal_score_eligible':False,'thread':{},'contexts':[]}
            record['memory_before']=diagnostic_observe(lambda:diagnostic_memory(service,output/'before-smaps.log'))
            def threads():
                try:record['thread']=hcd.collect_thread_window(service.process.pid,min(duration+8,120),
                    cgroup=service.domain.path,stop_event=stop,max_samples=512,max_thread_records=50000)
                except Exception as error:record['thread']={'error':repr(error)}
            def context():
                while len(record['contexts'])<32:
                    record['contexts'].append(diagnostic_observe(lambda:diagnostic_context(service)))
                    raw=output/'perf.data'
                    if ((raw.is_file() and raw.stat().st_size>96<<20) or
                            ((output/'perf-record.log').is_file() and (output/'perf-record.log').stat().st_size>1<<20)):
                        record['perf_bounded_stop']='96MiB raw or1MiB log limit; partial evidence retained'
                        if process is not None and process.poll() is None:
                            try:os.killpg(process.pid,signal.SIGINT)
                            except ProcessLookupError:pass
                        break
                    if stop.wait(1):break
            probes=[threading.Thread(target=threads,daemon=True),threading.Thread(target=context,daemon=True)]
            process=None;log=None
            try:
                for probe in probes:
                    try:probe.start()
                    except RuntimeError as error:record.setdefault('observer_start_errors',[]).append(repr(error))
                record['symbol_snapshot']=diagnostic_observe(lambda:diagnostic_symbols(service,output))
                if perf and capability and capability['available']:
                    command=[perf,'record','--clockid','mono','-e',capability['event'],'-F',str(plan['perf_frequency']),
                        *capability['selected_flags'],'-p',str(service.process.pid),
                        '-o',str(output/'perf.data'),'--','sleep',str(duration+6)]
                    record['perf_command']=command;log=(output/'perf-record.log').open('xb')
                    try:process=subprocess.Popen(command,stdout=log,stderr=log,start_new_session=True)
                    except OSError as error:record['perf_error']=repr(error)
                else:record['perf_error']='capability probe unavailable; CPU attribution unknown; no repeated business sampling'
                record['perf_capability']=capability
                return super().client(service,workload,rate,duration,output,**kwargs)
            finally:
                stop.set()
                for probe in probes:
                    if probe.ident is not None:probe.join(timeout=5)
                record['observer_threads_stopped']=all(not probe.is_alive() for probe in probes)
                if process is not None:
                    if process.poll() is None:
                        try:os.killpg(process.pid,signal.SIGINT)
                        except ProcessLookupError:pass
                    try:process.wait(timeout=5)
                    except subprocess.TimeoutExpired:
                        try:os.killpg(process.pid,signal.SIGKILL)
                        except ProcessLookupError:pass
                        process.wait(timeout=5)
                        record['perf_forced_cleanup']=True
                    record['perf_exit_code']=process.returncode
                if log:log.close()
                record['memory_after']=diagnostic_observe(lambda:diagnostic_memory(service,output/'after-smaps.log'))
                save(output/'diagnostic-observations.json',record)
    try:
        fixture.start();private=load(a.root/'fixture/manifest.json')
        private['database']['cpu_set']=load(fixture.state_path)['limits']['cpuset.cpus'];save(fixture.manifest_path,private)
        capability=perf_capability(perf,directory/'perf-capability')
        fc.Harness=DiagnosticHarness
        if followup:
            c=plan['environment_controls']
            previous=None;consecutive=0
            for i in range(c['count']):
                out=directory/'environment-controls'/('control-%d'%(i+1))
                try:row=fc.window(plan,c['name'],c['workload'],c['rate'],c['warmup_seconds'],c['score_seconds'],out,
                    job={'phase':'control','contract':'dom'})
                except Exception as error:row=load(out/'window.json') if (out/'window.json').is_file() else {'error':repr(error)}
                row.update(turn='environment-control',name=c['name'],score_eligible=False)
                control_rows.append(row)
                invalid,signature=window_quality(row)
                consecutive=consecutive+1 if invalid and signature==previous else 1 if invalid else 0
                previous=signature if invalid else None
                if consecutive>=2:break
            environment=environment_record(dict(rows=control_rows),directory/'environment-qualification.json')
            environment.update(control_version='A',scope='Three independent frozen prepatch A controls on this runner; not historical A0.')
            save(directory/'environment-qualification.json',environment)
            if environment['quality']=='fail':
                result=dict(execution_complete=False,rows=[],stopped=[{'reason':'prepatch environment qualification failed; no business scores started'}],
                    environment_controls=control_rows,environment_qualification=environment,perf_capability=capability,
                    goal_status='blocked',formal_qualified=False,candidate_accepted=False,scope_window_count=len(control_rows))
                save(directory/'summary.json',result);return result
        for case in plan['cases']:
            failures=0;previous_failure=None;case_rows=[]
            turns=diagnostic_turns(plan,case,followup)
            for turn,name in turns:
                output=directory/case['workload']/turn
                try:row=fc.window(plan,name,case['workload'],case['rate'],plan['warmup_seconds'],
                    plan['score_seconds'],output,job={'phase':'explore','contract':case['contract']})
                except Exception as error:
                    row=load(output/'window.json') if (output/'window.json').is_file() else {'error':repr(error)}
                row.update(turn=turn,workload=case['workload'],name=name,diagnostic_only=name=='D',
                    normal_score_eligible=name!='D',formal_qualified=False,candidate_accepted=False)
                if name=='D':
                    log=output/'service/service.log';ledgers=[]
                    if log.is_file():
                        with log.open('rb') as stream:text=stream.read((8<<20)+1)
                        if len(text)>8<<20:ledgers.append({'unavailable':'service log exceeds8MiB read bound; original retained'})
                        for line in text.decode(errors='replace').splitlines() if len(text)<=8<<20 else []:
                            if line.startswith('KU_FLOW_DIAG '):
                                try:
                                    value=json.loads(line[len('KU_FLOW_DIAG '):])
                                    if not isinstance(value,dict):raise ValueError('expected object')
                                    ledgers.append(value)
                                except ValueError:ledgers.append({'unparseable':line[:1024]})
                    save(output/'flow-ledger.json',dict(records=ledgers,record_count=len(ledgers),
                        scope='Whole service lifetime including readiness and warmup; no score-only normalization.',
                        complete_record_count=sum(x.get('complete') is True for x in ledgers)))
                    raw=output/'score/perf.data'
                    if perf and raw.is_file() and raw.stat().st_size:
                        symfs=output/'score/symfs';symbols=['--symfs',str(symfs)] if symfs.is_dir() else []
                        diagnostic_command([perf,'report','-i',str(raw),*symbols,'--stdio','--no-children',
                            '--show-nr-samples','--sort','symbol,dso,comm','--percent-limit','0'],output/'score/perf-self.log',timeout=60)
                        diagnostic_command([perf,'script','-i',str(raw),*symbols,'--ns','--show-switch-events',
                            '--show-lost-events','--show-task-events','-F','+pid,+tid,+cpu,+ip,+symoff,+srcline,+misc'],
                            output/'score/perf-stacks.log',timeout=60,limit=64<<20)
                    row['flow_ledger_path']=str(output/'flow-ledger.json')
                    row['flow_ledger_available']=len(ledgers)==1 and 'unparseable' not in ledgers[0] and 'unavailable' not in ledgers[0]
                    row['flow_ledger_complete']=row['flow_ledger_available'] and ledgers[0].get('complete') is True
                    row['thread_and_perf_observation_path']=str(output/'score/diagnostic-observations.json')
                case_rows.append(row);rows.append(row);save(directory/'results.json',dict(rows=rows,stopped=stopped))
                invalid,signature=window_quality(row)
                failures=failures+1 if invalid and signature==previous_failure else 1 if invalid else 0
                previous_failure=signature if invalid else None
                if row.get('functional_valid') is False or row.get('accounting_valid') is False or failures>=2:
                    stopped.append(dict(workload=case['workload'],after=turn,
                        reason='confirmed functional/accounting failure' if row.get('functional_valid') is False or
                            row.get('accounting_valid') is False else 'two consecutive unusable measurement windows; same quality signature; cause not inferred',
                        quality_signature=signature,
                        no_retry=True));break
            controls=[r.get('full_cpu_us') for r in case_rows if r['name']=='B']
            save(directory/case['workload']/'comparison.json',dict(rows=case_rows,
                paired_screen=screen_pairs(case_rows,case,plan['latency_slo_ms'][case['workload']]) if followup else None,
                candidate_screens={n:screen_pairs(case_rows,case,plan['latency_slo_ms'][case['workload']],n)
                    for n in plan.get('candidate_versions',['B']) if any(r['name']==n for r in case_rows)} if followup else {},
                worker_policy_screen=screen_pairs(case_rows,case,plan['latency_slo_ms'][case['workload']],'B','BF')
                    if followup and 'BF' in plan['versions'] else None,
                normal_control_cpu_max_min=max(controls)/min(controls) if len(controls)>=2 and
                    all(type(x) in (int,float) and x>0 for x in controls) else None,
                no_formal_performance_claim=True,diagnostic_cost_not_pooled=True,
                interpretation='Before/after drift and sampling perturbation require review. D and normal differ in '
                    'instrumentation/layout; these windows do not isolate perf overhead from ledger/roles overhead.'))
        growth_rows=[];control_row=None
        if followup and plan.get('worker_growth') and not stopped:
            config=plan['worker_growth']
            for mode in ('cold','burst'):
                for group,order in enumerate(config['orders'],1):
                    for name in order:
                        out=directory/'worker-growth'/mode/f'group-{group}-{name}'
                        try:
                            row=fc.window(plan,name,config['workload'],config[mode]['rate'],0,config[mode]['seconds'],out,
                                job={'phase':'diagnostic','contract':'worker-growth-'+mode})
                        except Exception as error:
                            row=load(out/'window.json') if (out/'window.json').is_file() else {}
                            row['error']=repr(error)
                        invalid,_=window_quality(row)
                        p99=row.get('latency',{}).get('p99_ms')
                        row.update(name=name,group=group,mode=mode,diagnostic_only=True,normal_score_eligible=False,
                            valid=not invalid and row.get('all_correct') is True and row.get('natural_reclamation') is True
                                and type(p99)in(int,float) and p99<=config['maximum_p99_ms'])
                        growth_rows.append(row);save(out/'window.json',row)
                        if not row['valid']:break
                    if growth_rows and not growth_rows[-1]['valid']:break
                if growth_rows and not growth_rows[-1]['valid']:break
        if followup and plan.get('transport_control') and not stopped:
            c=plan['transport_control'];out=directory/'transport-control'
            try:
                control_row=fc.window(plan,c['name'],c['workload'],c['rate'],c['warmup_seconds'],c['score_seconds'],out,
                    job={'phase':'diagnostic','contract':'same-volume-control-not-Transform'})
            except Exception as error:
                control_row=load(out/'window.json') if (out/'window.json').is_file() else {}
                control_row['error']=repr(error)
            window_quality(control_row)
            control_row.update(diagnostic_only=True,normal_score_eligible=False,formal_qualified=False,
                candidate_accepted=False,scope=c['scope'])
            save(out/'window.json',control_row)
        expected_rows=sum(len(diagnostic_turns(plan,c,followup)) for c in plan['cases'])
        axes=diagnostic_axes(plan,rows,expected_rows,stopped,control_row,growth_rows,followup)
        result=dict(**axes,axes=axes,rows=rows,stopped=stopped,
            goal_status='unknown' if axes['execution_complete'] else 'blocked',formal_qualified=False,candidate_accepted=False,
            scope=plan['scope'],additional_anomaly_windows=0,environment_controls=control_rows,
            environment_qualification=environment,perf_capability=capability,
            scope_window_count=len(rows)+len(control_rows)+(control_row is not None)+len(growth_rows),transport_control=control_row,
            worker_growth=growth_rows,
            json_layer_diagnosis=plan.get('json_layer_diagnosis'))
        save(directory/'summary.json',result);return result
    finally:
        fc.Harness=original
        state['database_cleanup']=fixture.stop();save(a.root/'hosted-state.json',state)
        diagnostic_command(['ps','-eo','pid,ppid,nlwp,rss,stat,comm'],directory/'processes-after.log')


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
                    'observational_quality_passed':len(complete)==3 and all(r.get('status')=='observed'
                        and r.get('functional_valid') is True and r.get('accounting_valid') is True
                        and r.get('natural_reclamation') is True and r.get('measurement_invalid') is not True
                        and r.get('arrival_schedule_quality')==r.get('environment_quality')=='pass'
                        for g in complete for r in [g[left],g[right]]),
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
        path.suffix in ('.json','.ku','.c','.md','.log','.sql','.gz','.csv','.py','.lock','.toml','.rs','.go','.mod','.sum','.profdata') or (path.suffix=='.profraw' and 'http-profiles' in parts) or
        (parts[0]=='diagnostic' and (path.name=='perf.data' or 'symfs' in parts)) or 'bin' in parts or path.name=='libpq.so')


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
        caches_included=False,private_uris_redacted=True,
        redaction_scope='Structured JSON and text logs only; raw perf/ELF bytes remain exact and may retain ephemeral private-fixture values.',
        source_scope='normal generated C/Ku inputs plus benchmark source; product Git commits in frozen plan')
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
    p.add_argument('--phase',required=True,choices=['build','run','tail','lifecycle','compiler','diagnostic','followup-screen','archive']);p.add_argument('--root',required=True,type=Path)
    p.add_argument('--source',required=True,type=Path);p.add_argument('--tools',required=True,type=Path)
    p.add_argument('--baseline',default=BASELINE);p.add_argument('--candidate',required=True)
    p.add_argument('--prepatch',default=FOLLOWUP_PREPATCH)
    p.add_argument('--screen-variants',default='',help='Up to seven explicit full commits, comma separated; normal preregistered short ablations only')
    p.add_argument('--build-set',choices=['full','diagnostic','followup-screen'],default='full')
    a=p.parse_args(argv);a.root=a.root.resolve();a.source=a.source.resolve(strict=a.phase!='archive');a.tools=a.tools.resolve(strict=a.phase!='archive')
    if a.phase!='archive' and (a.root==a.source or a.source in a.root.parents or a.root in a.source.parents):p.error('output must be independent of checkout')
    a.screen_variants=a.screen_variants.split(',') if a.screen_variants else []
    if len(a.screen_variants)>7 or len(set(a.screen_variants))!=len(a.screen_variants):p.error('at most seven distinct screen variants')
    if any(not re.fullmatch('[0-9a-f]{40}',x) for x in [a.baseline,a.candidate,a.prepatch,*a.screen_variants]) or a.baseline!=BASELINE:p.error('frozen full commit identities required')
    if a.build_set!='followup-screen' and a.screen_variants:p.error('screen variants require followup-screen build-set')
    if a.prepatch in a.screen_variants or a.candidate in a.screen_variants:p.error('screen variants must differ from prepatch and final candidate')
    if a.phase!='archive' and (platform.system()!='Linux' or platform.machine() not in ('aarch64','arm64')):p.error('actual Linux ARM64 required')
    if a.phase=='diagnostic' and a.build_set!='diagnostic':p.error('diagnostic phase requires --build-set diagnostic')
    if a.phase=='followup-screen' and a.build_set!='followup-screen':p.error('screen phase requires --build-set followup-screen')
    if a.build_set=='followup-screen' and a.candidate==a.prepatch:p.error('screen candidate must differ from frozen prepatch')
    if a.phase in ('run','tail','lifecycle','compiler') and a.build_set!='full':p.error('formal phases require full build-set')
    a.root.mkdir(parents=True,exist_ok=True);identity=dict(baseline=a.baseline,candidate=a.candidate,entry_sha256=sha(__file__),build_set=a.build_set,
        prepatch=a.prepatch if a.build_set=='followup-screen' else None,screen_variants=a.screen_variants)
    state=load(a.root/'hosted-state.json') if (a.root/'hosted-state.json').exists() else {'identity':identity,'steps':{},'goal_status':'unknown'}
    if state['identity']!=identity and a.phase!='archive':p.error('identity changed; new root required')
    if a.phase=='archive':result=archive(a,state)
    else:
        def stop(*unused):raise KeyboardInterrupt('hosted coordinator interrupted')
        signal.signal(signal.SIGTERM,stop);signal.signal(signal.SIGALRM,stop)
        if a.phase in ('run','tail','lifecycle','compiler'):signal.alarm(130*60-30)
        if a.phase=='diagnostic':signal.alarm(20*60-30)
        if a.phase=='followup-screen':signal.alarm((25+3*len(a.screen_variants))*60-30)
        operation={'build':build,'run':run,'tail':lambda a,s:run(a,s,tail_only=True),
            'lifecycle':lambda a,s:run(a,s,tail_only=True,stability_only=True),'compiler':compiler,'diagnostic':diagnostic,
            'followup-screen':lambda a,s:diagnostic(a,s,followup=True)}[a.phase]
        result=step(state,a.root,a.phase,lambda:operation(a,state),resume=a.phase in ('run','tail','lifecycle','compiler'))
    if a.phase!='archive':state['goal_status']=result.get('goal_status','unknown');save(a.root/'hosted-state.json',state)
    print(json.dumps(result));return 2 if a.phase in ('run','tail','lifecycle','diagnostic','followup-screen') and result.get('execution_complete') is not True else 0

if __name__=='__main__':raise SystemExit(main())
