#!/usr/bin/env python3
"""Serial ARM64 hosted A0/B/R evidence. Builds and observations are separate from goals.
No product patch, remote dispatch, SDK matrix, installation or release operation.
"""
from __future__ import annotations
import argparse, copy, gzip, hashlib, io, json, os
from pathlib import Path
import platform, pwd, re, select, shutil, signal, struct, subprocess, sys, tarfile, threading, time
import build_linux as bl
import focus_cycle as fc
import database as db
from linux_harness import ExternalDomain, GIB

ARCHIVE_LIMIT = 384<<20
BASELINE = '956727188a058119fdb3790959063f1496ccb4cc'
FOLLOWUP_PREPATCH = BASELINE
PG_QUEUE_PREPATCH = 'f39c673df0cd08232f184141a7e2aedef4f585c9'
HTTP_SHORT_PREPATCH = 'b765f9bc5d3a6e71353a562e27cef7b4cbb35f69'
POLICY = {'arrival_limits_ms': {'p99_ms': 2, 'p999_ms': 10, 'max_ms': 50}}
RESOURCES = {'service_cores': 2, 'service_memory_bytes': GIB,
             'driver_cores': 1, 'driver_cpuset': '2', 'driver_memory_bytes': GIB}
SERVICE_ENV = {'KU_EVENT_LOOPS': '{cores}', 'KU_BLOCKING_THREADS': '17',
               'KU_THREAD_BUDGET': '64', 'KU_TASK_MAX_BYTES': '67108864',
               'KU_HTTP_REQUEST_TIMEOUT_MS': '30000'}
ADMISSION = dict(required=True,name='B',connections=1184,planned_cancel=16,explicit_503=32,
    limits={'max_connections':4096,'max_active_requests':1024,'max_pending_requests':128},
    recovery_maximum_seconds=5,recovery_origin='before final body-byte release; owned-socket withdrawal if release never starts',
    normal_score_eligible=False,counts_as_timed_window=False,
    scope='Finite actual admission/recovery, separate from fixed CPU, capacity and diagnostic windows')


def load(p): return json.loads(Path(p).read_text())
def sha(p): return bl.digest(p)
def save(p, v): bl.write_json(p, v)
def encoded(v): return (json.dumps(v, sort_keys=True, separators=(',', ':'))+'\n').encode()


def validate_screen_selection(a,build_set):
    if getattr(a,'http_short_screen',False):
        if getattr(a,'http_short_goal','cpu') not in ('cpu','memory'):
            raise ValueError('--http-short-goal must be cpu or memory')
        if build_set!='diagnostic' or any(getattr(a,k,False) for k in ('task_json_screen','common_http_screen',
                'final_task_json','pg_failure_probe','normal_self_probe','pg_queue_screen','pg_driver_screen','memory_owner_probe')):
            raise ValueError('--http-short-screen requires diagnostic build-set and no other opt-in screen')
        if getattr(a,'prepatch',None)!=HTTP_SHORT_PREPATCH or a.candidate==HTTP_SHORT_PREPATCH:
            raise ValueError('--http-short-screen requires frozen b765f9b prepatch and a distinct candidate')
    if getattr(a,'memory_owner_probe',False) and (build_set!='diagnostic' or any(getattr(a,k,False) for k in
            ('task_json_screen','common_http_screen','final_task_json','pg_failure_probe','normal_self_probe','pg_queue_screen','pg_driver_screen','http_short_screen'))):
        raise ValueError('--memory-owner-probe requires diagnostic build-set and no other opt-in mode')
    if getattr(a,'pg_driver_screen',False) and (build_set!='diagnostic' or any(getattr(a,k,False) for k in
            ('task_json_screen','common_http_screen','final_task_json','pg_failure_probe','normal_self_probe','pg_queue_screen'))):
        raise ValueError('--pg-driver-screen requires diagnostic build-set and no other opt-in screen')
    if getattr(a,'pg_queue_screen',False) and (build_set!='diagnostic' or any(getattr(a,k,False) for k in
            ('task_json_screen','common_http_screen','final_task_json','pg_failure_probe','normal_self_probe','pg_driver_screen'))):
        raise ValueError('--pg-queue-screen requires diagnostic build-set and no other opt-in screen')
    if getattr(a,'pg_queue_screen',False) and (getattr(a,'prepatch',None)!=PG_QUEUE_PREPATCH or a.candidate==PG_QUEUE_PREPATCH):
        raise ValueError('--pg-queue-screen requires frozen f39c673 prepatch and a distinct fixed candidate')
    if getattr(a,'normal_self_probe',False) and (build_set!='diagnostic' or any(getattr(a,k,False) for k in
            ('task_json_screen','common_http_screen','final_task_json','pg_failure_probe','pg_queue_screen','pg_driver_screen'))):
        raise ValueError('--normal-self-probe requires diagnostic build-set and no other opt-in screen')
    if getattr(a,'pg_failure_probe',False) and (build_set!='diagnostic' or any(getattr(a,k,False) for k in
            ('task_json_screen','common_http_screen','final_task_json','normal_self_probe','pg_queue_screen','pg_driver_screen'))):
        raise ValueError('--pg-failure-probe requires diagnostic build-set and no other opt-in screen')
    if getattr(a,'final_task_json',False) and (build_set!='full' or getattr(a,'task_json_screen',False) or getattr(a,'common_http_screen',False)):
        raise ValueError('--final-task-json requires full build-set and cannot select followup screens')
    if getattr(a,'task_json_screen',False) and build_set!='followup-screen':
        raise ValueError('--task-json-screen requires followup-screen build-set')
    if getattr(a,'common_http_screen',False) and (build_set!='followup-screen' or not getattr(a,'task_json_screen',False)):
        raise ValueError('--common-http-screen requires followup-screen build-set and --task-json-screen')


def hosted_identity(a,build_set):
    validate_screen_selection(a,build_set)
    identity=dict(baseline=a.baseline,candidate=a.candidate,entry_sha256=sha(__file__),build_set=build_set,
        prepatch=a.prepatch if build_set=='followup-screen' or getattr(a,'pg_queue_screen',False) or getattr(a,'http_short_screen',False) else None,screen_variants=a.screen_variants,
        task_json_screen=getattr(a,'task_json_screen',False),common_http_screen=getattr(a,'common_http_screen',False),final_task_json=getattr(a,'final_task_json',False),pg_failure_probe=getattr(a,'pg_failure_probe',False))
    if getattr(a,'normal_self_probe',False):identity['normal_self_probe']=True
    if getattr(a,'pg_queue_screen',False):identity['pg_queue_screen']=True
    if getattr(a,'pg_driver_screen',False):identity['pg_driver_screen']=True
    if getattr(a,'http_short_screen',False):
        identity.update(http_short_screen=True,http_short_goal=getattr(a,'http_short_goal','cpu'))
    if getattr(a,'memory_owner_probe',False):identity['memory_owner_probe']=True
    return identity


def require_task_json_support(root):
    task_ir=Path(root)/'B/src/src/ir/task.rs'
    if not task_ir.is_file() or '\n    JsonDecode {' not in task_ir.read_text():
        raise ValueError('explicit Task JSON mode requires a frozen B product with native Task JSON support')


def frozen_plan(versions,final_task_json=False):
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
    plan=dict(versions=versions,resources=RESOURCES,quality_policy=POLICY,rates={'json_object':500,'db':200,'ok':2000,'cpu':80},
        schedule=schedule,targets=targets,admission=copy.deepcopy(ADMISSION),resume={'maximum_replacements':0},bootstrap_resamples=20000,
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
    if final_task_json:
        if 'BT' not in versions:raise ValueError('--final-task-json requires the identified BT build')
        plan.update(final_task_json=True,task_json_admission=dict(copy.deepcopy(ADMISSION),name='BT'),
            coordinator_run_budget_seconds=150*60-30,
            coordinator_budget_scope='Whole serial95-window/deeper-capacity/30min test coordinator budget only; product, driver, recovery and per-window deadlines unchanged')
        contract='task-json-supplemental-dom'
        plan['task_json_supplemental']=dict(supplemental_only=True,original_worker_score_eligible=False,
            schedule=[dict(phase='final',workload='json_object',rate=500,contract=contract,
                orders=[['B','BT','R'],['BT','R','B'],['R','B','BT'],['B','R','BT'],['BT','B','R']],
                warmup_seconds=10,score_seconds=30)],
            targets=[dict(phase='final',workload='json_object',contract=contract,numerator='BT',denominator='R',
                maximum_ratio=limit,metric=metric,minimum_pairs=5) for metric,limit in
                [('full_cpu_us',.90),('memory.steady_median_bytes',1)]],
            scope='Separate15 normal O2/helpers/text Task JSON windows; original80 schedule and16 targets unchanged; BT/B is cost change only')
        plan['capacity'].update(maximum_points=12,maximum_coarse_points=10,
            maximum_rates={'json_object':64000,'db':64000})
    return plan


def supplemental_task_json_plan(plan):
    config=copy.deepcopy(plan)
    for key in ('capacity','stability','admission','task_json_admission','task_json_supplemental'):config.pop(key,None)
    config.update(copy.deepcopy(plan['task_json_supplemental']))
    return config


def supplemental_task_json_costs(config,rows):
    targets=[dict(t,denominator='B',maximum_ratio=1) for t in config['targets']]
    comparisons=fc.compare_goals(dict(config,targets=targets),rows)
    return [dict(metric=g['target']['metric'],paired_windows=g['paired_windows'],interval=g['interval'],
        formal_qualified=g['formal_qualified'],scope='BT/B cost change only; not a performance acceptance target') for g in comparisons]



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
        resources=RESOURCES, quality_policy=POLICY,admission=copy.deepcopy(ADMISSION),
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


def pg_failure_plan(versions):
    """One identified normal Ku pressure point; failure evidence, never a score."""
    return dict(schema='ku-hosted-pg-failure-probe-v1',versions=versions,
        resources=copy.deepcopy(RESOURCES),quality_policy=copy.deepcopy(POLICY),
        pg_failure_probe=dict(name='B',workload='db',rate=12800,warmup_seconds=10,
            score_seconds=30,contract='pg-default'),maximum_windows=1,maximum_replacements=0,
        diagnostic_only=True,normal_score_eligible=False,formal_qualified=False,
        observation_alarm_seconds=120,workflow_limit_seconds=180,
        purpose='Investigate HTTP500 at the original12800/s PG point. No SQL retry, changed timeout, '
                'new matrix, perf probe or performance acceptance. A captured failure remains a failure.')


def normal_self_plan(versions):
    """Normal Release SELF, bounded by two unsampled controls per stack."""
    return dict(schema='ku-hosted-normal-kernel-chains-v1',versions=versions,
        resources=copy.deepcopy(RESOURCES),quality_policy=copy.deepcopy(POLICY),
        workload='json_object',contract='dom',rate=4000,warmup_seconds=10,score_seconds=15,
        order=[[name,turn] for name in ('B','R') for turn in ('before','sample','after')],
        event='cpu-clock',perf_frequency=499,perf_flags=['--call-graph','fp','--kernel-callchains'],maximum_windows=6,maximum_replacements=0,
        maximum_control_cpu_ratio=1.10,observation_alarm_seconds=330,workflow_limit_seconds=420,
        diagnostic_only=True,normal_score_eligible=False,formal_qualified=False,
        purpose='Identify kernel-only callchains in fixed reused normal B/R synchronous JSON; only original Release binaries, '
                'same fixed corpus and driver, before/sample/after per stack. No D ledger, retries, '
                'capacity, product change or performance acceptance. Kernel unwinding depends on the active kernel; '
                'no user callchain completeness, blocked-time or additive inclusive-CPU claim.')


def http_short_plan(versions,goal='cpu'):
    if goal not in ('cpu','memory'):raise ValueError('HTTP short goal must be cpu or memory')
    controls=[dict(name='A',phase='environment-control',workload='json_object',contract='dom',group=g,
        rate=500,warmup_seconds=5,score_seconds=10) for g in (1,2,3)]
    windows=[dict(name=n,phase='normal',workload=w,contract=c,group=g,rate=r,warmup_seconds=3,score_seconds=15)
        for w,r,c in (('json_object',500,'dom'),('ok',2000,'text'),('db',200,'pg-strict'))
        for g,order in enumerate((('A','B'),('B','A'),('A','B')),1) for n in order]
    windows += [dict(name=n,phase='compute-guard',workload='cpu',contract='compute-guard',group=1,
        rate=100,warmup_seconds=3,score_seconds=15) for n in ('A','B')]
    thresholds=(dict(json_geometric_ratio_maximum=.98,json_upper95_strictly_below=1,
        other_business_regression='lower95>1 blocks; crossing1 remains inconclusive',compute_ratio_maximum=1.03)
        if goal=='cpu' else dict(json_memory_geometric_ratio_maximum=.95,json_memory_upper95_strictly_below=1,
            cpu_lower95_maximum=1,cpu_upper95_maximum=1.03,compute_ratio_maximum=1.03,
            memory_metric='memory.steady_median_bytes',memory_scope='Whole service cgroup memory.current; score middle60% median'))
    thresholds['final_decision']='root reviews raw host/CPU/memory/correctness/arrival evidence; never automatic adoption'
    return dict(schema='ku-hosted-http-short-screen-v1',versions=versions,prepatch=HTTP_SHORT_PREPATCH,http_short_goal=goal,
        resources=copy.deepcopy(RESOURCES),quality_policy=copy.deepcopy(POLICY),windows=controls+windows,
        maximum_windows=23,maximum_replacements=0,observation_alarm_seconds=690,workflow_limit_seconds=720,
        adoption_thresholds=thresholds,
        diagnostic_only=True,normal_score_eligible=False,formal_qualified=False,
        purpose='Three original frozen-A environment controls then20 serial normal A/B windows for one HTTP candidate. Three JSON/ok/strictPG '
                'pairs and one coarse compute guard. Identical business, driver, deadlines, resources and SQL; '
                'Compute100/s supplies1500 scored arrivals for the unchanged p999 gate; this guard load is separate '
                'from original formal CPU results. No R/D/HC/BF, cold/burst, long run, capacity, retry or formal qualification. Unknown environment stays unknown.')


def http_short_comparison(rows,complete,environment=None,goal='cpu'):
    if goal not in ('cpu','memory'):raise ValueError('HTTP short goal must be cpu or memory')
    result=dict(complete=complete,eligible_for_root_review=False,candidate_accepted=False,comparisons={},
        formal_qualified=False,normal_score_eligible=False,reasons=[],environment_qualification=environment,http_short_goal=goal)
    if not environment or environment.get('quality')!='pass':result['reasons'].append('environment qualification unresolved or failed')
    if not complete:
        result['reasons'].append('incomplete or invalid observation/cleanup; no scoring or retry');return result
    rows=[r for r in rows if r['phase']!='environment-control']
    if any(r.get('environment_quality')!='pass' for r in rows):
        result['reasons'].append('one or more business windows have unresolved environment qualification')
    for workload in ('json_object','ok','db'):
        selected=[r for r in rows if r['workload']==workload]
        pairs=[{r['name']:r for r in selected if r['group']==g} for g in (1,2,3)]
        result['comparisons'][workload]=fc.paired_interval([p['B']['full_cpu_us'] for p in pairs],
            [p['A']['full_cpu_us'] for p in pairs],minimum=3)
    cpu={r['name']:r for r in rows if r['workload']=='cpu'}
    result['compute_guard']=dict(B_over_A=cpu['B']['full_cpu_us']/cpu['A']['full_cpu_us'],pairs=1,
        confidence_interval=None,scope='Correctness and coarse guard only; one pair cannot establish a precise confidence interval')
    if goal=='cpu':
        j=result['comparisons']['json_object']
        if j.get('status')!='estimated' or j['estimate']>.98 or j['upper95']>=1:
            result['reasons'].append('JSON CPU criterion not demonstrated: geometric B/A<=.98 and upper95<1 required')
        for workload in ('ok','db'):
            interval=result['comparisons'][workload]
            if interval.get('status')!='estimated' or interval['lower95']>1:
                result['reasons'].append(workload+' CPU regression or unavailable interval')
    else:
        selected=[r for r in rows if r['workload']=='json_object']
        pairs=[{r['name']:r for r in selected if r['group']==g} for g in (1,2,3)]
        result['json_memory']=j=fc.paired_interval([fc.metric(p['B'],'memory.steady_median_bytes') for p in pairs],
            [fc.metric(p['A'],'memory.steady_median_bytes') for p in pairs],minimum=3)
        result['memory_scope']='Whole service cgroup memory.current sampled during score; middle60% median; not endpoints, RSS, virtual or requested bytes'
        if j.get('status')!='estimated' or j['estimate']>.95 or j['upper95']>=1:
            result['reasons'].append('JSON service memory criterion not demonstrated: geometric B/A<=.95 and upper95<1 required')
        for workload,interval in result['comparisons'].items():
            if interval.get('status')!='estimated' or interval['lower95']>1 or interval['upper95']>1.03:
                result['reasons'].append(workload+' CPU guard requires lower95<=1 and upper95<=1.03')
    if result['compute_guard']['B_over_A']>1.03:result['reasons'].append('coarse compute ratio>1.03')
    result['eligible_for_root_review']=not result['reasons']
    result['note']='Screen evidence only; no significant CPU increase is not proof of strictly zero cost. CI crossing1 does not prove equivalence. Raw host/CPU/memory/arrival review and adoption remain with root.'
    return result


def pg_driver_plan(versions):
    return dict(schema='ku-hosted-pg-driver-screen-v1',versions=versions,
        resources=copy.deepcopy(RESOURCES),quality_policy=copy.deepcopy(POLICY),
        windows=[dict(concurrency=n,rate=12800,warmup_seconds=3,score_seconds=10) for n in (256,1024)],
        recovery=dict(rate=100,seconds=2,maximum_seconds=5,origin='score planned cutoff, including request tail and CSV export'),
        timeout_seconds=3,queue=65536,diagnostic_arrival_ledger=True,maximum_windows=2,maximum_replacements=0,
        observation_alarm_seconds=180,workflow_limit_seconds=240,diagnostic_only=True,normal_score_eligible=False,
        formal_qualified=False,candidate_accepted=False,
        purpose='Only sender concurrency256 versus1024 on one fresh normal B product. Both arms explicitly retain existing '
                'enqueue timestamps in the same identified diagnostic driver. Failed warmup is retained and score may be '
                'observed, never accepted; no timeout renewal, SQL retry, product change, matrix or capacity ranking.')


def pg_queue_plan(versions,prepatch):
    return dict(schema='ku-hosted-pg-queue-screen-v1',versions=versions,prepatch=prepatch,
        resources=copy.deepcopy(RESOURCES),quality_policy=copy.deepcopy(POLICY),
        windows=[dict(name=name,phase='normal',group=group,rate=200,warmup_seconds=3,score_seconds=15)
            for group,order in enumerate((('A','B'),('B','A'),('A','B')),1) for name in order]+[
            dict(name=name,phase='pressure',group=0,rate=12800,warmup_seconds=10,score_seconds=30) for name in ('A','B')],
        maximum_windows=8,maximum_replacements=0,observation_alarm_seconds=420,workflow_limit_seconds=480,
        diagnostic_only=True,normal_score_eligible=False,formal_qualified=False,
        purpose='Three interleaved normal PG200 pairs plus one A/B PG12800 observation. Same SQL, resources, '
                'deadlines and original driver. Pressure failures stay failures and do not become performance scores. '
                'Unregistered environment stays unknown; no formal ranking, capacity, retry, perf or matrix.')


def followup_plan(versions, prepatch=FOLLOWUP_PREPATCH,task_json_screen=False,common_http_screen=False):
    if common_http_screen and not task_json_screen:
        raise ValueError('--common-http-screen requires --task-json-screen')
    if task_json_screen and 'BT' not in versions:
        raise ValueError('--task-json-screen requires the identified BT build')
    plan=diagnostic_plan(versions)
    plan.update(schema='ku-hosted-followup-screen-v1',prepatch=prepatch,
        task_json_screen=task_json_screen,common_http_screen=common_http_screen,
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
    if task_json_screen:
        # This is a separately identified execution path, not a replacement
        # for the frozen worker fixture or its original sixteen goal rows.
        names=['A','B','BT','reference']
        plan['paired_orders']=[names,names[1:]+names[:1],names[2:]+names[:2]]
        plan['candidate_versions']=['B','BT']
        plan['cases']=[dict(workload='json_object',rate=500,reference='R',contract='dom'),
            dict(workload='json_escape',rate=500,reference='R',contract='escaped-unicode',ledger=False)]
        plan['latency_slo_ms']={'json_object':100,'json_escape':100}
        plan['task_json_comparison']=dict(numerator='BT',denominator='B',reference='R',
            diagnostic_only=True,original_worker_score_eligible=False,
            admission_scope='Separate serial B worker and BT Task admission/recovery gates; both are required, neither is a timed score',
            scope='Normal O2/helpers/text native Task JSON locals versus the same-current synchronous worker; '
                  'old A/B worker comparison retained; no PG/old-ablation repetition or old-goal substitution')
        plan['task_json_admission']=dict(copy.deepcopy(ADMISSION),name='BT')
        plan['scope']='Three frozen prepatch JSON environment controls, then separate A/B/BT/R JSON object and Unicode pairs, '
        plan['scope']+='plus the unchanged A/B/R CPU guard. BT uses normal O2/helpers/text without probes; '
        plan['scope']+='it is excluded from original worker goals. B and BT admission/recovery are required separately; cold/burst and HC retain their separate scope.'
    elif 'C1' in versions and 'C2' in versions:
        names=['A','C1','C2','B','reference']
        plan['cases'].append(dict(workload='json_escape',rate=500,reference='R',contract='escaped-unicode',
            paired_orders=[names,names[2:]+names[:2],names[4:]+names[:4]],ledger=False))
        plan['latency_slo_ms']['json_escape']=100
    else:
        # Every new combination must check the Unicode path, even after old
        # ablations are withdrawn. Reuse the normal A/B/reference order.
        plan['cases'].append(dict(workload='json_escape',rate=500,reference='R',contract='escaped-unicode',
            paired_orders=copy.deepcopy(plan['paired_orders']),ledger=False))
        plan['latency_slo_ms']['json_escape']=100
    if common_http_screen:
        normal_orders=[['A','B','reference'],['B','reference','A'],['reference','A','B']]
        plan['cases']=[dict(workload='ok',rate=2000,reference='R',contract='text',ledger=False,paired_orders=copy.deepcopy(normal_orders)),
            *plan['cases'],dict(workload='db',rate=200,reference='RS',contract='pg-strict',ledger=False,paired_orders=copy.deepcopy(normal_orders))]
        plan['latency_slo_ms'].update(ok=100,db=250)
        plan['scope']='Explicit common HTTP screen: normal A/B/reference pairs for ok, strict PG and CPU guard; '
        plan['scope']+='only JSON object/Unicode add separate same-current BT. Original worker goals stay separate. '
        plan['scope']+='B and BT admission/recovery remain required; unchanged worker-growth and HC are not rerun or declared newly passed.'
    plan['cases'].append(dict(workload='cpu',rate=80,reference='R',contract='compute-guard',
        paired_orders=[['A','B','reference'],['B','reference','A'],['reference','A','B']],ledger=False))
    plan['latency_slo_ms']['cpu']=1000
    if fixed and not common_http_screen:
        plan['worker_growth']=dict(names=['B','BF'],orders=[['B','BF'],['BF','B'],['B','BF']],
            cold=dict(rate=500,seconds=3),burst=dict(rate=8000,seconds=5),warmup_seconds=0,
            workload='json_object',maximum_p99_ms=100,diagnostic_only=True,normal_score_eligible=False)
    if 'HC' in versions and not common_http_screen:
        plan['transport_control']=dict(name='HC',workload='json_object',rate=500,warmup_seconds=3,score_seconds=15,
            diagnostic_only=True,normal_score_eligible=False,
            scope='Same1048bytebody/1071byteresponseandworkerpath;omitsTransform;notJSON-onlytimeorranking')
    plan['maximum_windows']=3+sum(len(diagnostic_turns(plan,c,True)) for c in plan['cases'])
    if plan.get('worker_growth'):plan['maximum_windows']+=12
    if plan.get('transport_control'):plan['maximum_windows']+=1
    return plan


def diagnostic_turns(plan,case,followup):
    if not followup:
        return [(turn,case['reference'] if turn.startswith('reference') else 'D' if turn=='ledger-sample' else 'B')
                for turn in plan['order']]
    turns=[(f'group-{i+1}-{name}',case['reference'] if name=='reference' else name)
        for i,order in enumerate(case.get('paired_orders',plan['paired_orders'])) for name in order]
    if case.get('ledger',True):turns.append(('ledger-sample','D'))
    return turns


def diagnostic_axes(plan,rows,expected,stopped,control,growth,followup,admission=None):
    normal=len(rows)==expected and not stopped
    qualified=not followup or all((r.get('name')=='D' and r.get('diagnostic_only')) or
        (r.get('functional_valid') is True and r.get('accounting_valid') is True
         and r.get('all_correct') is True and r.get('natural_reclamation') is True
         and r.get('arrival_schedule_quality')=='pass' and r.get('environment_quality')=='pass'
         and not r.get('measurement_invalid') and not r.get('observed_environment_faults')) for r in rows)
    layers=not followup or plan.get('json_layer_diagnosis',{}).get('execution_complete') is True
    omitted_common_controls=followup and plan.get('common_http_screen') is True
    control_ok=not followup or omitted_common_controls or (isinstance(control,dict) and control.get('functional_valid') is True
        and control.get('accounting_valid') is True and control.get('all_correct') is True
        and control.get('natural_reclamation') is True and control.get('arrival_schedule_quality')=='pass'
        and not control.get('measurement_invalid') and not control.get('observed_environment_faults'))
    growth_ok=not plan.get('worker_growth') or len(growth)==12 and all(r.get('valid') for r in growth)
    admission_ok=isinstance(admission,dict) and admission.get('passed') is True and admission.get('execution_complete') is True
    result=dict(normal_windows_complete=normal,normal_windows_qualified=qualified,json_layers_complete=layers,
        transport_control_complete=control_ok,worker_growth_complete=growth_ok,admission_complete=admission_ok,
        execution_complete=normal and qualified and layers and control_ok and growth_ok and admission_ok)
    if omitted_common_controls:
        result.update(transport_control_complete=None,worker_growth_complete=None,
            unchanged_control_scope='worker growth and HC not required and not run in this common screen; no new pass')
    return result


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
    except Exception as error:
        # Keep ordinary measurements independent of derived tool failures,
        # including programming errors. Interrupt/termination still propagates.
        return dict(execution_complete=False,records=records,error=repr(error),diagnostic_only=True,goal_status='blocked')


def build(a,state):
    if os.geteuid()==0: raise ValueError('build must run as the ordinary checkout user')
    validate_screen_selection(a,getattr(a,'build_set','full'))
    if getattr(a,'memory_owner_probe',False) or getattr(a,'normal_self_probe',False):raise ValueError('explicit observation requires verified fixed archive reuse, not a fresh build')
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
            elif name=='Driver':b.go_build(client=True)
            else:b.ku_native(Path(load(a.root/'B/manifest.json')['stacks']['ku']['build']['cli']))
            return b.finish()
        except BaseException as e:b.finish(failed=repr(e));raise
    build_set=getattr(a,'build_set','full');diagnostic=build_set in ('diagnostic','followup-screen')
    prepatch=getattr(a,'prepatch',FOLLOWUP_PREPATCH)
    variants=getattr(a,'screen_variants',[])
    builds=([('A',prepatch,None),*[(f'C{i+1}',commit,None) for i,commit in enumerate(variants)]] if build_set=='followup-screen' else [])+[
        ('B',a.candidate,None),('R',a.candidate,None)] if diagnostic else [('A0',a.baseline,None),('B',a.candidate,None),('R',a.candidate,None),
        ('BJ',a.candidate,{'ku_json':'typed'}),('BP',a.candidate,{'ku_pg_read':'typed'}),('BD',a.candidate,{'ku_format':'direct'})]
    if getattr(a,'pg_queue_screen',False) or getattr(a,'http_short_screen',False):builds=[('A',prepatch,None),('B',a.candidate,None),('Driver',a.candidate,None)]
    if getattr(a,'pg_driver_screen',False):builds=[('B',a.candidate,None),('Driver',a.candidate,None)]
    for name,commit,mode in builds:
        step(state,a.root,'build-'+name,lambda n=name,c=commit,m=mode:one(n,c,m))
    # Inspect the frozen product snapshot, never the current tools checkout.
    # Old products/build reuse keep their original plan and build identities.
    if getattr(a,'task_json_screen',False) or getattr(a,'final_task_json',False):
        require_task_json_support(a.root)
        step(state,a.root,'build-BT',lambda:one('BT',a.candidate,{'ku_json_execution':'task'}))
    return complete_build(a,state,build_set,prepatch,variants)


def complete_build(a,state,build_set,prepatch,variants):
    """Freeze/verify prepared normal builds; shared by fresh and byte-verified reuse."""
    task_json_screen=getattr(a,'task_json_screen',False)
    final_task_json=getattr(a,'final_task_json',False)
    validate_screen_selection(a,build_set)
    diagnostic=build_set in ('diagnostic','followup-screen')
    rm=load(a.root/('Driver' if getattr(a,'pg_queue_screen',False) or getattr(a,'pg_driver_screen',False) or getattr(a,'http_short_screen',False) else 'R')/'manifest.json');client=copy.deepcopy(rm['client']);protocol=Path(rm['protocol']);versions={}
    if getattr(a,'pg_driver_screen',False):client['command'].append('--diagnostic-arrival-ledger')
    b_normal=load(a.root/'B/manifest.json')['stacks']['ku']['build']['native']['server']
    fixed_control=build_set=='followup-screen' and 'KU_HTTP_WORKER_POLICY' in Path(b_normal['generated_c']).read_text()
    names=((['A',*[f'C{i+1}' for i in range(len(variants))]] if build_set=='followup-screen' else [])
        +['B',*(['BF'] if fixed_control else []),'R','RS'] if diagnostic else ['A0','B','R','BJ','BP','BD','RS','RJ','RP','RPS'])
    if getattr(a,'pg_queue_screen',False) or getattr(a,'http_short_screen',False):names=['A','B']
    if getattr(a,'pg_driver_screen',False):names=['B']
    if getattr(a,'normal_self_probe',False):names=['B','R']
    if task_json_screen or final_task_json:
        require_task_json_support(a.root)
        if not (a.root/'BT/manifest.json').is_file():raise ValueError('explicit Task JSON mode requires the completed BT build')
        names.append('BT')
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
        if name=='BT':m.update(diagnostic_only=not final_task_json,normal_score_eligible=final_task_json,
            supplemental_only=final_task_json,diagnostic_role='native-task-json-local-values',original_worker_score_eligible=False)
        save(d/'manifest.json',m);binary=m['stacks'][stack]['command'][0]
        versions[name]=dict(root=str(d),stack=stack,source_commit=m['candidate_base'],binary_sha256=sha(binary))
        if name=='BT':versions[name].update(diagnostic_only=not final_task_json,normal_score_eligible=final_task_json,
            supplemental_only=final_task_json,original_worker_score_eligible=False,json_execution_mode='task')
    fixture=normalize_manifest(copy.deepcopy(load(a.root/'B/manifest.json')),protocol,client)
    fixture['stacks']={};save(a.root/'fixture/manifest.json',fixture)
    if diagnostic and not any(getattr(a,k,False) for k in ('normal_self_probe','pg_queue_screen','pg_driver_screen','http_short_screen')):versions['D']=step(state,a.root,'build-ledger',lambda:build_ledger(a))
    layers=None
    if build_set=='followup-screen':
        layers=step(state,a.root,'build-json-layers',lambda:build_json_layers(a))
        if layers.get('execution_complete') is True:
            m=load(a.root/'HC/manifest.json');versions['HC']=dict(root=str(a.root/'HC'),stack='ku',
                source_commit=m['candidate_base'],binary_sha256=sha(m['stacks']['ku']['command'][0]),
                diagnostic_only=True,normal_score_eligible=False)
    if getattr(a,'http_short_screen',False):plan=http_short_plan(versions,getattr(a,'http_short_goal','cpu'))
    elif getattr(a,'pg_driver_screen',False):plan=pg_driver_plan(versions)
    elif getattr(a,'pg_queue_screen',False):plan=pg_queue_plan(versions,prepatch)
    elif getattr(a,'normal_self_probe',False):plan=normal_self_plan(versions)
    elif getattr(a,'pg_failure_probe',False):plan=pg_failure_plan(versions)
    elif build_set=='followup-screen':plan=followup_plan(versions,prepatch,task_json_screen=task_json_screen,common_http_screen=getattr(a,'common_http_screen',False))
    elif diagnostic:plan=diagnostic_plan(versions)
    else:plan=frozen_plan(versions,final_task_json=final_task_json)
    if layers is not None:plan['json_layer_diagnosis']=layers
    save(a.root/'frozen-plan.json',plan)
    identities={n:fc.version_identity(plan,n) for n in versions};save(a.root/'build-identities.json',identities)
    if (getattr(a,'pg_queue_screen',False) or getattr(a,'http_short_screen',False)) and identities['A']['files']['expanded source']['sha256']!=identities['B']['files']['expanded source']['sha256']:
        raise ValueError('A/B expanded business inputs differ')
    if build_set=='followup-screen':
        for name in plan['candidate_versions']:
            if name=='BT':continue
            if identities['A']['files']['expanded source']['sha256']!=identities[name]['files']['expanded source']['sha256']:
                raise ValueError('paired normal Ku business inputs differ: '+name)
        if bl.benchmark_stack_sources(a.root/'A/src/tests/benchmarks/cross_stack/rust')!=load(a.root/'R/manifest.json')['stacks']['rust']['build']['source_files_sha256']:
            raise ValueError('reference business source differs from frozen prepatch')
    if 'BT' in versions:
        worker=Path(identities['B']['files']['expanded source']['path']).read_text()
        selected=Path(identities['BT']['files']['expanded source']['path']).read_text()
        expected=bl.ku_json_execution_evidence(worker,selected,'task')
        actual=load(a.root/'BT/manifest.json')['stacks']['ku']['build']
        if (actual.get('json_execution_mode')!='task' or actual.get('json_execution')!=expected
                or identities['B']['files']['Ku CLI']['sha256']!=identities['BT']['files']['Ku CLI']['sha256']):
            raise ValueError('Task JSON entry is not an identified same-CLI single-declaration derivation')
        if final_task_json:
            worker_mode={k:v for k,v in identities['B']['build_mode'].items() if k!='json_execution_mode'}
            task_mode={k:v for k,v in identities['BT']['build_mode'].items() if k!='json_execution_mode'}
            if actual.get('diagnostic') or worker_mode!=task_mode:
                raise ValueError('formal Task JSON must be an uninstrumented same-mode/same-CLI build')
        save(a.root/'task-json-source-identity.json',dict(**expected,source_commit=a.candidate,
            worker_binding=identities['B'],task_binding=identities['BT'],formal_qualified=False))
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
    plan=load(a.root/'frozen-plan.json')
    if plan.get('final_task_json') and tail_only:raise ValueError('final Task JSON requires the complete performance-v2 run, not tail-only')
    fixture=db.Database(a.root/'fixture',state['uid'],state['gid'])
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
        if 'admission' not in axes:pending.insert(0,'admission')
        if plan.get('final_task_json') and 'task_json_supplemental' not in axes:pending.append('task-json-supplemental')
        state['deferred']={label:reason for label in pending}
        return dict(execution_complete=False,goal_status='blocked',candidate_accepted=False,
            environment_quality=environment['quality'],fixed_matrix_executed='final' in axes,
            axes=axes,reason=reason,scope='Retained original windows; downstream measurements not started or retried.')
    try:
        fixture.start();private=load(a.root/'fixture/manifest.json')
        private['database']['cpu_set']=load(fixture.state_path)['limits']['cpuset.cpus'];save(fixture.manifest_path,private)
        fc.Harness=HostedHarness
        fixed=copy.deepcopy(plan);fixed.pop('capacity',None);fixed.pop('stability',None);fixed.pop('admission',None);fixed.pop('task_json_admission',None);fixed.pop('task_json_supplemental',None)
        control=copy.deepcopy(fixed);control.update(targets=[],schedule=[dict(phase='control',workload='json_object',rate=500,
            orders=[['A0'],['A0'],['A0']],warmup_seconds=5,score_seconds=10)])
        control['stop_after_consecutive_unusable_windows']=2
        summary=execute('control',control,'all');environment=environment_record(summary,a.root/'environment-qualification.json')
        if environment['quality']!='pass':
            return blocked('environment control quality is '+environment['quality'],{'control':summary})
        if summary.get('execution_complete') is not True:
            return blocked('environment control execution incomplete',{'control':summary})
        admission=required_screen_admission(plan,a.root,state,boot,a.root/'results/admission')
        if admission.get('passed') is not True:
            return blocked('actual admission/recovery gate did not pass; retained without retry',{'control':summary,'admission':admission})
        final=None if tail_only else execute('final',fixed,'all')
        checked=summary if tail_only else final
        function_ok=(checked.get('execution_complete') is True and bool(checked.get('rows')) and all(
            r.get('status')=='observed' and r.get('functional_valid') is True and r.get('accounting_valid') is True
            and r.get('arrival_schedule_quality')=='pass' and (tail_only or r.get('environment_quality')=='pass')
            and r.get('measurement_invalid') is False and r.get('natural_reclamation') is True for r in checked['rows']))
        if not function_ok:
            return blocked('fixed/control measurement incomplete, invalid or function/accounting/cleanup failed',
                {'control':summary,'admission':admission,**({'final':final} if final is not None else {})})
        supplemental=None
        if plan.get('final_task_json'):
            config=supplemental_task_json_plan(plan)
            supplemental=execute('task-json-supplemental',config,'all')
            supplemental['BT_B_cost_change']=supplemental_task_json_costs(config,supplemental.get('rows',[]))
            save(a.root/'results/task-json-supplemental/summary-with-costs.json',supplemental)
            supplemental_ok=(supplemental.get('execution_complete') is True and len(supplemental.get('rows',[]))==15 and all(
                row.get('status')=='observed' and row.get('functional_valid') is True and row.get('accounting_valid') is True
                and row.get('arrival_schedule_quality')=='pass' and row.get('environment_quality')=='pass'
                and row.get('measurement_invalid') is False and row.get('natural_reclamation') is True for row in supplemental['rows']))
            if not supplemental_ok:
                return blocked('supplemental Task JSON execution/quality/function/accounting/cleanup failed; original16 results retained',
                    {'control':summary,'admission':admission,'final':final,'task_json_supplemental':supplemental})
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
            required=[summary,admission,continuous] if stability_only else [summary,admission,capacity,continuous]
            axes={'control':summary,'admission':admission,'stability':continuous}
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
            state['steps'].get('stability',{}).get('result',{}),goal,admission)
        if supplemental is not None:
            acceptance.update(task_json_supplemental=supplemental,supplemental_goal_status=supplemental.get('goal_status','unknown'),
                original_16_goal_status=final.get('goal_status','unknown'),
                supplemental_scope='Independent15 windows/2 goals; never substitute into original80/16. Functional failures block the entire run.')
        return dict(acceptance,core_advantage_goal_status=core_status,
            admission=admission,
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
        callchain_completeness='unknown',
        session_budget_seconds=18,scope='CPU-event sampling capability only, not a service benchmark. '
              'Generic EOPNOTSUPP does not uniquely identify a physical PMU or guest-kernel cause.')
    full=['--call-graph','dwarf,8192','--inherit','--switch-events']
    dwarf=['--call-graph','dwarf,8192']
    workload='import time\nend=time.monotonic()+1\nx=1\nwhile time.monotonic()<end: x=(x*1664525+1013904223)&0xffffffff'
    def remaining():
        # diagnostic_command can spend5s reaping a killed observer and polls
        # every50ms. Reserve that cleanup inside this one shared18s session.
        return 18-(time.monotonic()-began)-5.1
    def attempt(event,flags,label):
        left=remaining()
        if left<=0:return None
        out=directory/label;out.mkdir();raw=out/'perf.data'
        row=dict(event=event,flags=flags)
        row['record']=diagnostic_command([perf,'record','--clockid','mono','-e',event,'-F','499',
            *flags,'-o',str(raw),'--',sys.executable,'-c',workload],out/'record.log',timeout=min(4,left),limit=1<<20)
        left=remaining()
        if row['record']['complete'] and raw.is_file() and raw.stat().st_size and left>0:
            fields=['-F','event,ip,sym'] if flags else ['-G','-F','event']
            row['verify']=diagnostic_command([perf,'script','-i',str(raw),*fields],
                out/'events.log',timeout=min(2,left),limit=1<<20)
            if row['verify']['complete']:
                lines=(out/'events.log').read_text(errors='replace').splitlines()
                row['sample_count']=sum(event+':' in line for line in lines)
                if flags:
                    frames=set();multi=0;active=False
                    for line in lines:
                        if event+':' in line:
                            multi+=len(frames)>=2;frames=set()
                            active=True
                            line=line.split(event+':',1)[1]
                        if not active:continue
                        match=re.fullmatch(r'\s*(?:0x)?([0-9a-fA-F]+)\s+(\S+).*',line)
                        if (match and int(match[1],16) and match[2] not in ('[unknown]','??')
                                and not match[2].startswith('0x')):
                            frames.add(int(match[1],16))
                    row['multi_frame_samples']=multi+(len(frames)>=2)
                    row['callchain_check']='at least two distinct named nonzero PCs in one sample; completeness unknown'
        attempts.append(row)
        return row
    if perf:
        for event in ['cpu-clock','task-clock','cycles']:
            minimal=attempt(event,[],event+'-minimal')
            if minimal and minimal.get('sample_count',0)>0:
                chain=attempt(event,full,event+'-full')
                if not chain or not chain.get('multi_frame_samples',0):
                    # Failure of the full flag group does not isolate DWARF.
                    # One smaller combination, within the same total budget.
                    chain=attempt(event,dwarf,event+'-dwarf-only')
                with_chain=bool(chain and chain.get('multi_frame_samples',0))
                result.update(available=True,event=event,selected_flags=chain['flags'] if with_chain else [],
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
            and r.get('environment_quality')=='pass' and not r.get('measurement_invalid')
            and not r.get('observed_environment_faults') for r in (a,b,ref))
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


def task_json_screen(rows,case,slo):
    if not any(r['name']=='BT' for r in rows):return None
    return dict(screen_pairs(rows,case,slo,'BT','B'),diagnostic_only=True,
        original_worker_score_eligible=False,
        scope='Same-current native Task JSON versus synchronous worker; distinct execution-path diagnostic, '
              'not an original sixteen-goal result or proof from changing the fixture declaration alone')


def pg_failure_export(a,directory,*,output_name='pg-failure-observation',schema='ku-pg-failure-small-artifact-v1',selected=None):
    """Small redacted receipts only; raw originals remain in the full archive."""
    output=a.root/output_name;output.mkdir(exist_ok=False);output.chmod(0o755)
    entries=[];skipped=[];total=0;paths=[];metadata_reserve=128<<10
    for path in directory.rglob('*') if selected is None else selected:
        if not path.is_file() or path.is_symlink() or path.suffix not in ('.json','.log'):continue
        if len(paths)>=64:
            skipped.append(dict(path='*',reason='small artifact64-file limit; other raw receipts retained'));break
        paths.append(path)
    for path in sorted(paths):
        relative=path.relative_to(directory)
        if len(str(relative))>512:
            skipped.append(dict(path=str(relative)[:512],reason='receipt path exceeds512 characters'));continue
        if path.stat().st_size>2<<20:
            skipped.append(dict(path=str(relative),reason='receipt exceeds2MiB; retained in full archive'));continue
        raw=path.read_bytes()
        try:data=encoded(redact(json.loads(raw),a.root)) if path.suffix=='.json' else redact(raw.decode(),a.root).encode()
        except (ValueError,UnicodeError):
            skipped.append(dict(path=str(relative),reason='unreadable text receipt; raw not exported'));continue
        if total+len(data)>(8<<20)-metadata_reserve:
            skipped.append(dict(path=str(relative),reason='small artifact8MiB limit; retained in full archive'));continue
        dest=output/relative;dest.parent.mkdir(parents=True,exist_ok=True);dest.write_bytes(data);dest.chmod(0o644);total+=len(data)
        parent=dest.parent
        while parent!=output:
            parent.chmod(0o755);parent=parent.parent
        entries.append(dict(path=str(relative),original_sha256=hashlib.sha256(raw).hexdigest(),
            exported_sha256=hashlib.sha256(data).hexdigest(),bytes=len(data)))
    record=dict(schema=schema,entries=entries,skipped=skipped,bytes=total,
        metadata_reservation_bytes=metadata_reserve,total_limit_bytes=8<<20,
        raw_receipts_in_full_archive=True,redacted=True,normal_score_eligible=False)
    data=encoded(record)
    if len(data)>metadata_reserve:raise ValueError('small artifact metadata limit exceeded')
    (output/'export.json').write_bytes(data);(output/'export.json').chmod(0o644)
    return record


def pg_failure_probe(a,state):
    if os.geteuid()!=0:raise ValueError('PG failure probe requires root cgroup control')
    plan=load(a.root/'frozen-plan.json')
    if plan!=pg_failure_plan(plan.get('versions',{})):raise ValueError('matching frozen PG failure probe plan required')
    binding=fc.version_identity(plan,'B');mode=binding['build_mode']
    expected=dict(formatting_mode='helpers',json_response_mode='text',json_execution_mode='worker',
        pg_execution_mode='async',pg_read_mode='int',native_flags={'batch':['-std=c11','-O2'],'server':['-std=c11','-O2','-pthread']})
    if binding['stack']!='ku' or mode!=expected:raise ValueError('PG failure probe requires identified normal Release C11O2/helpers/text/async/int')
    directory=a.root/'pg-failure-probe';directory.mkdir(exist_ok=False)
    fixture=db.Database(a.root/'fixture',state['uid'],state['gid']);original=fc.Harness;private=None
    result=dict(schema='ku-pg-failure-observation-v1',diagnostic_only=True,normal_score_eligible=False,
        formal_qualified=False,candidate_accepted=False,goal_status='unknown',execution_complete=False,
        contract=plan['pg_failure_probe'],maximum_windows=1,windows_attempted=0,
        no_retry=True,perf_executed=False,scope=plan['purpose'])
    began=time.monotonic()
    class ProbeHarness(original):
        def __init__(self,*args,**kwargs):
            super().__init__(*args,**kwargs);self.manifest=copy.deepcopy(self.manifest)
            self.manifest['db_url']=private['db_url'];self.db_domain=ExternalDomain(private['db_cgroup'])
    try:
        fixture.start();private=load(a.root/'fixture/manifest.json')
        save(directory/'identity.json',dict(binding=binding,resources=plan['resources'],quality_policy=plan['quality_policy'],
            database_limits=load(fixture.state_path)['limits'],contract=plan['pg_failure_probe'],
            boot_sha256=sha('/proc/sys/kernel/random/boot_id'),purpose=plan['purpose']))
        fc.Harness=ProbeHarness;c=plan['pg_failure_probe'];output=directory/'window'
        result['windows_attempted']=1
        try:
            row=fc.window(plan,c['name'],c['workload'],c['rate'],c['warmup_seconds'],c['score_seconds'],
                output,binding=binding,job={'phase':'diagnostic','contract':c['contract']})
        except Exception as error:
            row=load(output/'window.json') if (output/'window.json').is_file() else {}
            row['observer_error']=repr(error)
        result['window']=row
        clients=[]
        for phase in ['warmup',*(['score'] if row.get('score_started') else [])]:
            report=output/phase/'client.json';path=output/phase/'driver-cleanup.json'
            record=dict(phase=phase,report_present=report.is_file(),cleanup_present=path.is_file(),cleanup_ok=False)
            if report.is_file() and report.stat().st_size<=2<<20:
                client=load(report);policy=client.get('first_errors_policy',{})
                record.update(report_sha256=sha(report),first_errors_policy=policy,
                    first_errors=client.get('first_errors',[])[:24],results=client.get('ledger',{}).get('results',{}),
                    error_sampling_policy_valid=policy.get('total_limit')==24 and policy.get('per_class_limit')==4
                        and policy.get('detail_limit_bytes')==256 and 'http_500' in policy.get('classes',[]))
            if path.is_file() and path.stat().st_size<=2<<20:
                cleanup_receipt=load(path);record['cleanup_sha256']=sha(path)
                record['cleanup_ok']=(cleanup_receipt.get('process_reaped') is True
                    and cleanup_receipt.get('cgroup_removed') is True
                    and cleanup_receipt.get('after',{}).get('process_count')==0)
            clients.append(record)
        result['client_evidence']=clients
        result['driver_cleanup_ok']=bool(clients) and all(x['report_present'] and x['cleanup_ok'] for x in clients)
        result['error_evidence_ready']=bool(clients) and all(x.get('error_sampling_policy_valid') is True for x in clients)
        term=row.get('termination');cleanup=row.get('cleanup',[])
        natural=(isinstance(term,dict) and fc.service_termination_succeeded('ku',term)) or (
            bool(cleanup) and all(fc.service_termination_succeeded('ku',x) for x in cleanup))
        result.update(functional_valid=row.get('functional_valid'),accounting_valid=row.get('accounting_valid'),
            natural_reclamation=natural,
            record_obtained=row.get('status') in ('warmup_failure','observed','functional_failure','accounting_failure')
                and 'observer_error' not in row)
    except BaseException as error:
        result.update(observer_error=repr(error),observation_interrupted=isinstance(error,(KeyboardInterrupt,SystemExit)))
        if not isinstance(error,Exception):raise
    finally:
        fc.Harness=original
        try:result['database_cleanup']=fixture.stop()
        except Exception as error:result['database_cleanup']={'error':repr(error)}
        state['database_cleanup']=result['database_cleanup']
        cleanup=result['database_cleanup'];database_ok=(cleanup.get('forced') is False and cleanup.get('cgroup_removed') is True
            and cleanup.get('after',{}).get('process_count')==0)
        result['seconds']=time.monotonic()-began
        result['execution_complete']=(result.get('record_obtained') is True and result.get('natural_reclamation') is True
            and result.get('driver_cleanup_ok') is True and result.get('error_evidence_ready') is True
            and database_ok and result['seconds']<=180 and 'observer_error' not in result)
        result['goal_status']='unknown' if result['execution_complete'] else 'blocked'
        save(directory/'summary.json',result)
        try:result['small_artifact']=pg_failure_export(a,directory)
        except Exception as error:
            result.update(execution_complete=False,goal_status='blocked',small_artifact_error=repr(error))
            save(directory/'summary.json',result);raise
        save(a.root/'hosted-state.json',state)
    return result



def kernel_chain_raw(path,start=None,end=None):
    """Validate the requested CPU-clock ABI, not text indentation or symbols."""
    path=Path(path)
    if not 0<path.stat().st_size<=16<<20:raise ValueError('bounded perf raw required')
    raw=path.read_bytes()
    if len(raw)<104 or raw[:8]!=b'PERFILE2':raise ValueError('PERFILE2 header required')
    attr_size,attrs,attrs_bytes,data,size=struct.unpack_from('<QQQQQ',raw,16)
    if not (attrs_bytes==attr_size and attr_size>=48 and attrs+attr_size<=len(raw) and 0<data<=len(raw) and size<=len(raw)-data):raise ValueError('single bounded perf attr/data required')
    sample_type=struct.unpack_from('<Q',raw,attrs+24)[0]
    # UAPI order: IDENTIFIER,IP,TID,TIME,ADDR,ID,STREAM_ID,CPU,PERIOD,READ,CALLCHAIN.
    fields=(65536,1,2,4,8,64,512,128,256);supported=32|sum(fields)
    if sample_type&295!=295 or sample_type&~supported:raise ValueError('requested bounded CALLCHAIN sample layout required')
    counts=dict(sample_type=sample_type,all_raw_samples=0,window_samples=0,kernel_multiframe_samples=0,raw_sha256=sha(path));offset=data
    while offset<data+size:
        if offset+8>data+size:raise ValueError('truncated perf record header')
        kind,misc,n=struct.unpack_from('<IHH',raw,offset)
        if n<8 or offset+n>data+size:raise ValueError('bad perf record bounds')
        if kind in (2,5,6,13):raise ValueError('lost or throttled perf records')
        if kind==9:
            cursor=offset+8;ip=stamp=0
            for bit in fields:
                if sample_type&bit:
                    if cursor+8>offset+n:raise ValueError('truncated fixed sample fields')
                    value=struct.unpack_from('<Q',raw,cursor)[0];cursor+=8
                    if bit==1:ip=value
                    if bit==4:stamp=value
            if cursor+8>offset+n:raise ValueError('missing callchain count')
            nr=struct.unpack_from('<Q',raw,cursor)[0];cursor+=8
            if nr>512 or cursor+nr*8!=offset+n:raise ValueError('bad callchain bounds')
            kernel=misc&7==1;frames=[]
            for addr in struct.unpack_from('<'+'Q'*nr,raw,cursor):
                if addr>=(1<<64)-4095:kernel=addr==(1<<64)-128
                elif kernel and addr:frames.append(addr)
            counts['all_raw_samples']+=1;inside=start is None or start<=stamp<=end
            if inside:counts['window_samples']+=1
            if misc&7==1 and ip and len(set(frames))>=2 and inside:
                counts['kernel_multiframe_samples']+=1
                if 'example' not in counts:counts['example']=[hex(x) for x in frames[:128]]
        offset+=n
    return counts


def kernel_context():
    result=dict(kernel=platform.release(),clock_ticks_per_second=os.sysconf('SC_CLK_TCK'),build_ids=[],
        symbols='perf names require exact current build-id review; unknown IPs remain unknown; no old ASLR base reuse')
    try:
        with Path('/sys/kernel/notes').open('rb') as stream:raw=stream.read(65537)
        if len(raw)>65536:raise ValueError('kernel notes bound')
        result['notes_sha256']=hashlib.sha256(raw).hexdigest();cursor=0
        while cursor+12<=len(raw):
            names,descs,kind=struct.unpack_from('<III',raw,cursor);cursor+=12
            name=raw[cursor:cursor+names];cursor+=(names+3)&~3;desc=raw[cursor:cursor+descs];cursor+=(descs+3)&~3
            if cursor>len(raw):raise ValueError('kernel note bounds')
            if name.rstrip(b'\0')==b'GNU' and kind==3:result['build_ids'].append(desc.hex())
    except (OSError,ValueError) as error:result['unavailable']=repr(error)
    return result


def kernel_raw_export(a,directory,bindings):
    output=a.root/'normal-kernel-raw';output.mkdir();rows=[]
    for name in ('B','R'):
        source=directory/name/'sample/score/perf.data'
        if not source.is_file():continue
        if source.is_symlink() or not 0<source.stat().st_size<=16<<20:raise ValueError('raw artifact size/type bound')
        target=output/(name+'.perf.data');shutil.copyfile(source,target);target.chmod(0o644)
        if sha(target)!=sha(source):raise ValueError('raw copy identity changed')
        rows.append(dict(path=target.name,bytes=target.stat().st_size,sha256=sha(target),binding=bindings[name]))
    record=dict(files=rows,total_limit_bytes=32<<20,per_file_limit_bytes=16<<20,
        original_bytes_no_redaction=True,complete=len(rows)==2,kernel_context=load(directory/'kernel-context.json') if (directory/'kernel-context.json').is_file() else {},
        scope='Two fixed allowlisted raw files only; exact B/R ELFs from reused verified archive; no measurement imports or rebuilt binary')
    save(output/'index.json',record);return record


def controlled_perf(client,pid,output,perf,record,*,deadline=None,limit=16<<20):
    """Readiness is an acknowledged enable, completion an acknowledged stop+exit0."""
    output=Path(output);raw=output/'perf.data';fds=[];process=watch=None;stop=threading.Event()
    def remaining(seconds):
        value=min(seconds,deadline-time.monotonic()) if deadline is not None else seconds
        if value<=0:raise TimeoutError('perf observer wall bound exceeded')
        return value
    try:
        read_ctl,write_ctl=os.pipe();fds.extend((read_ctl,write_ctl));read_ack,write_ack=os.pipe();fds.extend((read_ack,write_ack))
        os.set_blocking(write_ctl,False);os.set_blocking(read_ack,False)
        command=[perf,'record','--clockid','mono','-e','cpu-clock','-F','499','--call-graph','fp','--kernel-callchains','--delay=-1',
            f'--control=fd:{read_ctl},{write_ack}','-p',str(pid),'-o',str(raw)]
        record.update(command=command,raw_limit_bytes=limit,control=[],started_monotonic_ns=time.monotonic_ns(),
            control_ack_framing='Linux perf ACK_TAG sizeof includes trailing NUL: fixed5 bytes ack\\n\\0; consume full frame before next command')
        with (output/'perf-record.log').open('xb') as log:
            process=subprocess.Popen(command,stdout=log,stderr=log,stdin=subprocess.DEVNULL,
                start_new_session=True,pass_fds=(read_ctl,write_ack))
            record['perf_pid']=process.pid
            os.close(read_ctl);fds.remove(read_ctl);os.close(write_ack);fds.remove(write_ack)
            def control(command,seconds):
                began=time.monotonic_ns();until=time.monotonic()+remaining(seconds);data=b''
                payload=(command+'\n').encode()
                if os.write(write_ctl,payload)!=len(payload):raise RuntimeError('partial perf control write')
                while len(data)<5:
                    left=until-time.monotonic()
                    if left<=0 or not select.select([read_ack],[],[],max(0,left))[0]:raise TimeoutError('perf '+command+' ACK timeout')
                    chunk=os.read(read_ack,65-len(data));data+=chunk
                    if not chunk or len(data)>64:raise RuntimeError('perf ACK EOF/64-byte bound')
                if data!=b'ack\n\0':raise RuntimeError('unexpected perf ACK: '+repr(data))
                record['control'].append(dict(command=command,started_monotonic_ns=began,ack_monotonic_ns=time.monotonic_ns(),ack_wire_bytes=len(data)))
            def bounded():
                while not stop.wait(.1):
                    if process.poll() is not None:return
                    if (raw.exists() and raw.stat().st_size>limit) or (output/'perf-record.log').stat().st_size>1<<20:
                        record['output_limit_exceeded']=True
                        try:os.killpg(process.pid,signal.SIGKILL)
                        except ProcessLookupError:pass
                        return
            watch=threading.Thread(target=bounded,daemon=True);watch.start();control('enable',8)
            record['client_started_monotonic_ns']=time.monotonic_ns()
            try:result=client()
            finally:record['client_finished_monotonic_ns']=time.monotonic_ns()
            control('stop',2);process.wait(timeout=remaining(5));record['controlled_stop_exit']=process.returncode==0
            return result
    except BaseException as error:
        record['perf_observer_error']=repr(error);raise
    finally:
        stop.set()
        if watch is not None:watch.join(timeout=1)
        if process is not None and process.poll() is None:
            try:os.killpg(process.pid,signal.SIGKILL)
            except ProcessLookupError:pass
            record['forced_cleanup']=True
            try:process.wait(timeout=1)
            except subprocess.TimeoutExpired:record['cleanup_error']='owned perf did not reap within1 second after KILL'
        for fd in fds:os.close(fd)
        record.update(perf_exit_code=process.returncode if process is not None else None,
            ended_monotonic_ns=time.monotonic_ns(),observer_thread_stopped=watch is None or not watch.is_alive(),
            raw_bytes=raw.stat().st_size if raw.is_file() else 0)
        record['record_complete']=(record.get('controlled_stop_exit') is True and record['perf_exit_code']==0
            and 0<record['raw_bytes']<=limit and not record.get('output_limit_exceeded')
            and not record.get('forced_cleanup') and record['observer_thread_stopped'] and 'perf_observer_error' not in record)


def normal_perf_preflight(a):
    """One short owned child, before any build or service; no hosted-state mutation."""
    output=a.root/'normal-perf-preflight';output.mkdir();deadline=time.monotonic()+18;child=None
    perf=shutil.which('perf',path=str(a.tools/'bin')+os.pathsep+os.environ.get('PATH',''))
    record=dict(complete=False,maximum_wall_seconds=25,coordinator_alarm_seconds=19,entry_sha256=sha(__file__),candidate=a.candidate,
        boot_sha256=sha('/proc/sys/kernel/random/boot_id'),perf_path=perf,child_reaped=False,
        perf_flags=normal_self_plan({})['perf_flags'])
    try:
        if not perf:raise ValueError('perf unavailable; preflight blocks build and six windows')
        record.update(perf_sha256=sha(perf),perf_real_path=str(Path(perf).resolve()))
        code='import sys,time,socket\na,b=socket.socketpair();a.settimeout(.1);b.settimeout(.1)\nsys.stdin.buffer.read(1)\nend=time.monotonic()+1\nwhile time.monotonic()<end: a.sendall(b"x");b.recv(1)\na.close();b.close()\nprint("done",flush=True)\nsys.stdin.buffer.read(1)\n'
        child=subprocess.Popen([sys.executable,'-c',code],stdin=subprocess.PIPE,stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL,start_new_session=True)
        def busy():
            os.write(child.stdin.fileno(),b'x');until=min(deadline,time.monotonic()+3);data=b''
            while b'\n' not in data:
                left=until-time.monotonic()
                if left<=0 or not select.select([child.stdout.fileno()],[],[],max(0,left))[0]:raise TimeoutError('owned busy child did not finish')
                chunk=os.read(child.stdout.fileno(),17-len(data));data+=chunk
                if not chunk or len(data)>16:raise ValueError('owned busy child EOF/16-byte bound')
            if data!=b'done\n':raise ValueError('owned busy child completion mismatch')
        controlled_perf(busy,child.pid,output,perf,record,deadline=deadline,limit=1<<20)
        os.write(child.stdin.fileno(),b'x');child.stdin.close();child.wait(timeout=1);record['child_reaped']=child.returncode==0
        size_on_disk=(output/'perf.data').stat().st_size
        with (output/'perf.data').open('rb') as stream:header=stream.read(56)
        offset,size=struct.unpack_from('<QQ',header,40)
        record['header_bounds_valid']=header[:8]==b'PERFILE2' and 0<offset<size_on_disk<=1<<20 and 0<size<=size_on_disk-offset
        start=record['client_started_monotonic_ns'];end=record['client_finished_monotonic_ns']
        selected=f'{start//10**9}.{start%10**9:09d},{end//10**9}.{end%10**9:09d}'
        record['exports']=[diagnostic_command([perf,kind,'-i',str(output/'perf.data'),'--time',selected,*flags],
            output/('perf-'+kind+'.log'),timeout=max(.01,min(2,deadline-time.monotonic())),limit=1<<20)
            for kind,flags in [('report',['--header','--stdio','--no-children','--show-nr-samples']),('script',['--ns','-F','pid,tid,time,event,ip'])]]
        versions=re.findall(r'^#[ \t]*perf version[ \t]*:([^\r\n]*)\r?$',(output/'perf-report.log').read_text(),re.M)
        version=versions[0].strip() if len(versions)==1 else ''
        record.update(perf_version_text=version,perf_version_header_fields=len(versions),
            perf_version_source='record header VERSION feature from the required perf report --header')
        record['perf_version_valid']=len(versions)==1 and bool(version) and version.isprintable() and len(version.encode('utf-8'))<=128
        if not record['perf_version_valid']:record['version_identity_error']='expected exactly one nonempty printable record-header version, at most128 UTF-8 bytes'
        text=(output/'perf-script.log').read_text();record['sample_count']=sum(int(m[1],16)!=0 for m in re.finditer(r'cpu-clock:\s+(?:0x)?([0-9a-fA-F]+)\b',text))
        record['kernel_chains']=kernel_chain_raw(output/'perf.data',start,end)
        record['complete']=(record.get('record_complete') is True and record['child_reaped'] and record['header_bounds_valid']
            and record['sample_count']>0 and record['kernel_chains']['kernel_multiframe_samples']>0 and record['perf_version_valid'] and all(x['complete'] for x in record['exports']) and time.monotonic()<=deadline)
    except BaseException as error:
        record['error']=repr(error)
        if not isinstance(error,Exception):raise
    finally:
        if child is not None and child.poll() is None:
            try:os.killpg(child.pid,signal.SIGKILL)
            except ProcessLookupError:pass
            record.update(child_forced_cleanup=True,complete=False)
            try:child.wait(timeout=1)
            except subprocess.TimeoutExpired:record['child_cleanup_error']='owned child did not reap within1 second after KILL'
        for stream in (child.stdin,child.stdout) if child is not None else ():
            if stream is not None:stream.close()
        record['seconds']=time.monotonic()-(deadline-18);save(output/'preflight.json',record)
        for path in output.iterdir():
            if path.is_file():path.chmod(0o644)
    return record


def require_normal_perf_preflight(a):
    path=a.root/'normal-perf-preflight/preflight.json';record=load(path) if path.is_file() else {}
    if not (record.get('complete') is True and record.get('kernel_chains',{}).get('kernel_multiframe_samples',0)>0
            and record.get('perf_flags')==normal_self_plan({})['perf_flags'] and record.get('candidate')==a.candidate and record.get('entry_sha256')==sha(__file__)
            and record.get('boot_sha256')==sha('/proc/sys/kernel/random/boot_id') and record.get('perf_path')
            and record.get('perf_sha256')==sha(record['perf_path'])):
        raise ValueError('matching successful controlled-perf preflight required before build/normal six-window capture')


def normal_self_client(client,service,output,duration,perf,*,sample):
    """Observe the existing scored client; never change its requests or deadlines."""
    output=Path(output);output.mkdir(parents=True,exist_ok=True)
    record=dict(sampled=sample,callchains='kernel-only; active kernel unwinder; no complete user-chain claim',clock_ticks_per_second=os.sysconf('SC_CLK_TCK'),
        cpu_distribution='unknown: no CPU field; instantaneous last-CPU is not a residency distribution',
        sample_scope='Controlled perf enable is acknowledged before client launch; stop follows client return. Filter mono-clock raw events by client_started/finished_monotonic_ns; '
            'that interval includes client startup/drain, not only planned arrivals. Full lifecycle CPU includes post-client perf finalization; '
            'scored-client cpu_us retains the existing h.client accounting interval.')
    def endpoint(label):
        started=time.monotonic_ns()
        snapshot=diagnostic_observe(lambda:diagnostic_context(service))
        proc=Path('/proc')/str(service.process.pid)/'task';threads=[]
        try:tasks=sorted(proc.iterdir(),key=lambda p:int(p.name))
        except OSError:tasks=[]
        for task in tasks[:128]:
            try:
                with (task/'stat').open('rb') as stream:raw=stream.read(4097)
                threads.append(dict(tid=int(task.name),stat=raw[:4096].decode(errors='replace'),truncated=len(raw)>4096))
            except OSError as error:threads.append(dict(tid=int(task.name),unavailable=repr(error)))
        value=dict(context=snapshot,thread_stat=threads,task_limit=128)
        if sample:
            value['memory']=diagnostic_observe(lambda:diagnostic_memory(service,output/(label+'-smaps.log')))
            value['cgroup']=diagnostic_observe(lambda:service.domain.snapshot())
        value.update(capture_started_monotonic_ns=started,capture_ended_monotonic_ns=time.monotonic_ns(),
            scope='Sequential endpoint reads, not a simultaneous snapshot or thread/core residency histogram.')
        return value
    record['before']=endpoint('before')
    try:
        if sample:
            if not perf:raise ValueError('perf unavailable; preflight required')
            record['symbols']=diagnostic_symbols(service,output)
            return controlled_perf(client,service.process.pid,output,perf,record)
        record['client_started_monotonic_ns']=time.monotonic_ns()
        try:return client()
        finally:record['client_finished_monotonic_ns']=time.monotonic_ns()
    finally:
        record['after']=endpoint('after');record.setdefault('record_complete',not sample)
        save(output/'normal-self-observations.json',record)




def memory_owner_plan(versions):
    return dict(schema='ku-memory-owner-plan-v1',versions=versions,resources=copy.deepcopy(RESOURCES),
        quality_policy=copy.deepcopy(POLICY),diagnostic_only=True,normal_score_eligible=False,
        formal_qualified=False,workload='json_object',rate=500,warmup_seconds=3,score_seconds=5,
        maximum_windows=1,maximum_replacements=0,timeout_seconds=3)


def prepare_memory_owner(a,state):
    import http_cost_diagnostic as hcd
    from compile_variants import Commands, clean_environment
    directory=a.root/'M';directory.mkdir();m=copy.deepcopy(load(a.root/'B/manifest.json'))
    stack=m['stacks']['ku'];build=stack['build'];native=build['native']['server'];original=copy.deepcopy(native)
    emission=hcd.prepare_allocation_owner_source(Path(native['generated_c']),directory/'owner.c',a.candidate)
    invocation=native['compiler_invocation'];libs=invocation['library_inputs']
    if len(libs)!=1 or libs[0]['role']!='libpq' or invocation['flags']!=['-std=c11','-O2','-pthread']:
        raise ValueError('owner probe requires original normal C11O2 and single libpq input')
    binary=directory/'bin/ku-owner';binary.parent.mkdir()
    command=[invocation['compiler']['path'],str(directory/'owner.c'),*invocation['flags'],
        libs[0]['selected_input']['real_path'],'-o',str(binary)]
    log=Commands(directory/'logs',clean_environment(a.tools));log.run('owner-link',command,directory,timeout=120)
    with binary.open('rb') as stream:header=stream.read(20)
    if len(header)!=20 or header[:6]!=b'\x7fELF\x02\x01' or int.from_bytes(header[18:20],'little')!=183:
        raise ValueError('owner link is not ELF64 little-endian AArch64')
    save(directory/'owner-compile.json',dict(command=command,commands=log.rows,diagnostic_only=True,normal_score_eligible=False))
    native.update(binary=str(binary),binary_sha256=sha(binary),generated_c=str(directory/'owner.c'),
        generated_c_sha256=emission['generated_c_sha256'],compiler_receipt=str(directory/'owner-compile.json'),
        compiler_receipt_sha256=sha(directory/'owner-compile.json'),
        compiler_invocation=dict(invocation,argv=command,output=str(binary),source=dict(path=str(directory/'owner.c'),
            sha256=emission['generated_c_sha256']),cwd=str(directory)))
    stack['command']=[str(binary)];build.update(instrumented='startup-allocation-owner-map-v1',normal_score_eligible=False)
    m.update(diagnostic_only=True,normal_score_eligible=False,build_receipt=str(directory/'owner-compile.json'))
    m=normalize_manifest(m,Path(m['protocol']),m['client']);save(directory/'manifest.json',m)
    fixture=copy.deepcopy(m);fixture['stacks']={};save(a.root/'fixture/manifest.json',fixture)
    versions={name:dict(root=str(a.root/name),stack='ku',source_commit=a.candidate,
        binary_sha256=load(a.root/name/'manifest.json')['stacks']['ku']['build']['native']['server']['binary_sha256'],
        diagnostic_only=name=='M',normal_score_eligible=False) for name in ('B','M')}
    plan=memory_owner_plan(versions);save(a.root/'frozen-plan.json',plan)
    save(a.root/'build-identities.json',{name:fc.version_identity(plan,name) for name in versions})
    if sha(original['binary'])!=original['binary_sha256'] or sha(original['generated_c'])!=original['generated_c_sha256']:
        raise ValueError('normal B artifact changed while preparing owner probe')
    return dict(execution_complete=True,goal_status='unknown',diagnostic_only=True,normal_score_eligible=False)


def memory_owner_probe(a,state):
    import http_cost_diagnostic as hcd
    if os.geteuid()!=0:raise ValueError('owner probe requires root cgroup control')
    plan=load(a.root/'frozen-plan.json')
    if plan!=memory_owner_plan(plan.get('versions',{})):raise ValueError('matching single-window owner plan required')
    binding=fc.version_identity(plan,'M');normal=fc.version_identity(plan,'B')
    directory=a.root/'memory-owner-probe';directory.mkdir();fixture=db.Database(a.root/'fixture',state['uid'],state['gid'])
    original=fc.Harness;private=None;captured={};result=dict(execution_complete=False,goal_status='blocked',
        diagnostic_only=True,normal_score_eligible=False,formal_qualified=False,candidate_accepted=False)
    class OwnerHarness(original):
        def __init__(self,*args,**kwargs):
            super().__init__(*args,**kwargs);self.manifest=copy.deepcopy(self.manifest)
            self.manifest['db_url']=private['db_url'];self.db_domain=ExternalDomain(private['db_cgroup'])
        def start(self,*args,**kwargs):
            service=super().start(*args,**kwargs)
            try:
                memory=diagnostic_memory(service,directory/'startup-smaps.log')
                captured['startup']=dict(memory=memory,owners=hcd.allocation_owner_ranges(
                    Path(service.cwd).parent/'service.log',service.process.pid,memory))
                return service
            except BaseException:
                service.stop();raise
        def client(self,service,workload,rate,duration,output,**kwargs):
            report=super().client(service,workload,rate,duration,output,**kwargs)
            if Path(output).name=='score':
                memory=diagnostic_memory(service,directory/'after-smaps.log')
                captured.update(memory=memory,context=diagnostic_context(service),
                    owners=hcd.allocation_owner_ranges(Path(service.cwd).parent/'service.log',service.process.pid,memory))
                with (Path('/proc')/str(service.process.pid)/'maps').open('rb') as stream:maps=stream.read((1<<20)+1)
                if len(maps)>1<<20:raise ValueError('owner maps exceeds1MiB')
                (directory/'after-maps.log').write_bytes(maps);save(directory/'live-owner-map.json',captured)
                owner_input=directory/'owner-ranges.json';save(owner_input,captured['owners'])
                ticks=(Path('/proc')/str(service.process.pid)/'stat').read_text().rsplit(')',1)[1].split()[19]
                captured['optional_pages']=diagnostic_command([sys.executable,'-B',str(Path(hcd.__file__)),
                    'owner-pages','--pid',str(service.process.pid),'--ranges',str(owner_input),'--start-ticks',ticks],
                    directory/'owner-pages.json',timeout=2,limit=65536)
                save(directory/'live-owner-map.json',captured)
            return report
    try:
        fixture.start();private=load(fixture.manifest_path);fc.Harness=OwnerHarness
        row=fc.window(plan,'M','json_object',500,3,5,directory/'window',binding=binding,job={'phase':'diagnostic'})
        for key in ('cpu_us','full_cpu_us','memory','qualification','upper_point','overload_boundary'):row.pop(key,None)
        row.update(diagnostic_only=True,normal_score_eligible=False,formal_qualified=False)
        valid=(row.get('correct')==2500 and row.get('all_correct') is True and row.get('functional_valid') is True
            and row.get('accounting_valid') is True and row.get('arrival_schedule_quality')=='pass'
            and row.get('natural_reclamation') is True and captured.get('owners',{}).get('complete') is True)
        if fc.version_identity(plan,'B')!=normal or fc.version_identity(plan,'M')!=binding:raise ValueError('owner artifact changed')
        result.update(row=row,execution_complete=valid,goal_status='unknown' if valid else 'blocked')
    except Exception as error:result['observer_error']=repr(error)
    finally:
        fc.Harness=original
        try:result['database_cleanup']=fixture.stop()
        except Exception as error:result['database_cleanup']={'error':repr(error)}
        cleanup=result['database_cleanup']
        if cleanup.get('forced') is not False or cleanup.get('cgroup_removed') is not True or cleanup.get('after',{}).get('process_count')!=0:
            result.update(execution_complete=False,goal_status='blocked')
        save(directory/'summary.json',result);save(a.root/'hosted-state.json',state)
        selected=[p for p in directory.rglob('*') if p.is_file() and p.name in ('summary.json','live-owner-map.json',
            'startup-smaps.log','after-smaps.log','after-maps.log','owner-pages.json','owner-pages.json.json','owner-pages.json.log','service.log','termination.json','window.json','client.json','driver-cleanup.json')]
        result['small_artifact']=pg_failure_export(a,directory,output_name='memory-owner-observation',
            schema='ku-memory-owner-small-v1',selected=selected)
    return result


def normal_diagnostic_quality(row):
    """Local observation gate; an unregistered environment stays unknown."""
    invalid,signature=window_quality(row);cost={};report=Path(row.get('score_report',''))
    if report.is_file() and report.stat().st_size<=2<<20:cost=load(report).get('service_cost',{})
    driver=cost.get('driver_headroom',{});fraction=driver.get('fraction_of_quota')
    headroom=(driver.get('saturated') is False and type(fraction)in(int,float) and 0<=fraction<.9)
    resources=bool(cost.get('before') and cost.get('after'))
    observed_fault=bool(cost.get('sampler_error') or cost.get('sampler_overflow') or row.get('independent_collector_fault'))
    valid=(row.get('functional_valid') is True and row.get('accounting_valid') is True
        and row.get('all_correct') is True and row.get('natural_reclamation') is True
        and row.get('environment_quality') in ('pass','unknown') and not invalid
        and headroom and resources and not observed_fault)
    row['diagnostic_quality']=dict(valid=valid,driver_headroom=driver,resource_endpoints_present=resources,
        environment_quality_preserved=row.get('environment_quality'),formal_environment_qualified=False,
        scope='Diagnostic observation only. Independent environment qualification was not run; unknown remains unknown. '
              'Functional/accounting/arrival, driver CPU headroom, available resource endpoints and observed resource faults '
              'are checked, then before/after drift and raw sampling still require review; this is not formal qualification.')
    return valid,signature


def pg_queue_screen(a,state):
    if os.geteuid()!=0:raise ValueError('PG queue screen requires root cgroup control')
    short=getattr(a,'http_short_screen',False);plan=load(a.root/'frozen-plan.json')
    prepatch=HTTP_SHORT_PREPATCH if short else PG_QUEUE_PREPATCH
    expected_plan=http_short_plan(plan.get('versions',{}),getattr(a,'http_short_goal','cpu')) if short else pg_queue_plan(plan.get('versions',{}),prepatch)
    if plan!=expected_plan:raise ValueError('matching frozen A/B diagnostic plan required')
    bindings={name:fc.version_identity(plan,name) for name in ('A','B')}
    expected=dict(formatting_mode='helpers',json_response_mode='text',json_execution_mode='worker',
        pg_execution_mode='async',pg_read_mode='int',native_flags={'batch':['-std=c11','-O2'],'server':['-std=c11','-O2','-pthread']})
    if any(x['stack']!='ku' or x['build_mode']!=expected for x in bindings.values()):raise ValueError('normal A/B Release modes required')
    if bindings['A']['source_commit']!=prepatch or bindings['B']['source_commit']==prepatch:raise ValueError('A/B source identities differ from the frozen pair')
    if short and bindings['A']['files']['expanded source']['sha256']!=bindings['B']['files']['expanded source']['sha256']:
        raise ValueError('A/B expanded business inputs differ')
    directory=a.root/('http-short-screen' if short else 'pg-queue-screen');directory.mkdir(exist_ok=False)
    fixture=db.Database(a.root/'fixture',state['uid'],state['gid']);original=fc.Harness;private=None;environment=None
    result=dict(schema='ku-http-short-screen-v1' if short else 'ku-pg-queue-screen-v1',diagnostic_only=True,normal_score_eligible=False,formal_qualified=False,
        candidate_accepted=False,goal_status='unknown',execution_complete=False,rows=[],stopped=[],purpose=plan['purpose'])
    if short:result['http_short_goal']=plan['http_short_goal']
    began=time.monotonic()
    class QueueHarness(original):
        def __init__(self,*args,**kwargs):
            super().__init__(*args,**kwargs);self.manifest=copy.deepcopy(self.manifest)
            self.manifest['db_url']=private['db_url'];self.db_domain=ExternalDomain(private['db_cgroup'])
            if environment:self.manifest['environment_qualification']=environment
    try:
        fixture.start();private=load(fixture.manifest_path);fc.Harness=QueueHarness
        save(directory/'identity.json',dict(bindings=bindings,resources=plan['resources'],quality_policy=plan['quality_policy'],
            database_limits=load(fixture.state_path)['limits'],boot_sha256=sha('/proc/sys/kernel/random/boot_id'),purpose=plan['purpose'],
            **({'http_short_goal':plan['http_short_goal'],'adoption_thresholds':plan['adoption_thresholds']} if short else {})))
        for config in plan['windows']:
            name=config['name'];phase=config['phase'];workload=config.get('workload','db')
            output=directory/(('environment-controls' if phase=='environment-control' else workload) if short else phase)/f"g{config['group']}-{name}"
            try:
                row=fc.window(plan,name,workload,config['rate'],config['warmup_seconds'],config['score_seconds'],output,
                    binding=bindings[name],job={'phase':'diagnostic','contract':config.get('contract','pg-default')})
            except Exception as error:
                row=load(output/'window.json') if (output/'window.json').is_file() else {};row['observer_error']=repr(error)
            row.update(name=name,phase=phase,workload=workload,group=config['group'],normal_score_eligible=False,formal_qualified=False,diagnostic_only=True)
            clients=[]
            for part in ['warmup',*(['score'] if row.get('score_started') else [])]:
                path=output/part/'driver-cleanup.json';client=output/part/'client.json'
                cleanup=load(path) if path.is_file() and path.stat().st_size<=2<<20 else {}
                clients.append(dict(phase=part,report_present=client.is_file(),cleanup_ok=cleanup.get('process_reaped') is True
                    and cleanup.get('cgroup_removed') is True and cleanup.get('after',{}).get('process_count')==0))
            termination=row.get('termination');cleanup=row.get('cleanup',[])
            natural=(isinstance(termination,dict) and fc.service_termination_succeeded('ku',termination)) or (
                bool(cleanup) and all(fc.service_termination_succeeded('ku',x) for x in cleanup))
            obtained=row.get('status') in ('warmup_failure','observed','functional_failure','accounting_failure') and 'observer_error' not in row
            observed=obtained and natural and all(c['report_present'] and c['cleanup_ok'] for c in clients)
            if phase=='normal' or short:
                quality,_=normal_diagnostic_quality(row);observed=observed and quality
                if short and environment:
                    row['diagnostic_quality'].update(registered_environment_evidence=environment['evidence'],
                        scope='Diagnostic A/B observation with separately registered original environment controls; '
                            'row quality is preserved. This is not formal ranking or automatic adoption.')
            row.update(observation_complete=observed,client_cleanup=clients,natural_reclamation=natural,
                interpretation='Pressure is an error-cause observation only; failed responses and driver lateness remain recorded. '
                    'No formal capacity or performance pass is inferred.' if phase=='pressure' else
                    'Normal A/B diagnostic; environment controls remain separate, no formal ranking or automatic adoption.' if short else
                    'Normal low-load diagnostic pair, not formal ranking; unregistered environment stays unknown.')
            result['rows'].append(row);save(directory/'results.json',dict(rows=result['rows'],stopped=result['stopped']))
            if not observed:
                result['stopped'].append(dict(name=name,phase=phase,group=config['group'],reason='normal quality or observation/physical cleanup failed; no retries'));break
            if short and phase=='environment-control' and config['group']==3:
                environment=environment_record(dict(rows=result['rows']),directory/'environment-qualification.json')
                environment.update(control_version='A',scope='Three independent frozen prepatch A controls; original registered qualification checks.')
                save(directory/'environment-qualification.json',environment);result['environment_qualification']=environment
                if environment['quality']!='pass':
                    result['stopped'].append(dict(phase=phase,reason='environment qualification unresolved or failed; business windows not started'));break
        normal=[row for row in result['rows'] if row['phase']=='normal']
        if not short and len(normal)==6 and all(r['observation_complete'] for r in normal):
            pairs=[{r['name']:r for r in normal if r['group']==g} for g in (1,2,3)]
            result['normal_cpu_pairs']=[dict(group=g,A=p['A']['full_cpu_us'],B=p['B']['full_cpu_us']) for g,p in enumerate(pairs,1)]
            result['normal_cpu_B_over_A']=fc.paired_interval([p['B']['full_cpu_us'] for p in pairs],[p['A']['full_cpu_us'] for p in pairs],minimum=3)
    except BaseException as error:
        result.update(observer_error=repr(error),observation_interrupted=not isinstance(error,Exception))
        if not isinstance(error,Exception):raise
    finally:
        fc.Harness=original
        try:result['database_cleanup']=fixture.stop()
        except Exception as error:result['database_cleanup']={'error':repr(error)}
        state['database_cleanup']=result['database_cleanup'];cleanup=result['database_cleanup']
        dbok=cleanup.get('forced') is False and cleanup.get('cgroup_removed') is True and cleanup.get('after',{}).get('process_count')==0
        result.update(seconds=time.monotonic()-began,execution_complete=len(result['rows'])==plan['maximum_windows']
            and not result['stopped'] and dbok and 'observer_error' not in result)
        if short:result['comparison']=http_short_comparison(result['rows'],result['execution_complete'],environment,plan['http_short_goal'])
        result['goal_status']='unknown' if result['execution_complete'] else 'blocked';save(directory/'summary.json',result)
        selected=[directory/name for name in ('summary.json','identity.json','results.json','environment-qualification.json')]
        selected.extend(directory/(('environment-controls' if c['phase']=='environment-control' else c['workload']) if short else c['phase'])/f"g{c['group']}-{c['name']}"/relative for c in plan['windows'] for relative in (
            'window.json','full-lifecycle.json','warmup/client.json','score/client.json','warmup/runtime-after-failure.json','service/service.log'))
        result['small_artifact']=pg_failure_export(a,directory,output_name='http-short-observation' if short else 'pg-queue-observation',
            schema='ku-http-short-small-artifact-v1' if short else 'ku-pg-queue-small-artifact-v1',selected=selected)
        save(a.root/'hosted-state.json',state)
    return result


def pg_driver_screen(a,state):
    """Two owned serial sender conditions; failed observations never become scores."""
    if os.geteuid()!=0:raise ValueError('PG driver screen requires root cgroup control')
    plan=load(a.root/'frozen-plan.json')
    if plan!=pg_driver_plan(plan.get('versions',{})):raise ValueError('matching frozen PG driver plan required')
    binding=fc.version_identity(plan,'B');expected=dict(formatting_mode='helpers',json_response_mode='text',
        json_execution_mode='worker',pg_execution_mode='async',pg_read_mode='int',
        native_flags={'batch':['-std=c11','-O2'],'server':['-std=c11','-O2','-pthread']})
    if binding['stack']!='ku' or binding['build_mode']!=expected:raise ValueError('normal Release B mode required')
    manifest=load(Path(plan['versions']['B']['root'])/'manifest.json')
    if manifest['client']['command'].count('--diagnostic-arrival-ledger')!=1:
        raise ValueError('same explicitly identified diagnostic driver required for both conditions')
    directory=a.root/'pg-driver-screen';directory.mkdir(exist_ok=False)
    fixture=db.Database(a.root/'fixture',state['uid'],state['gid']);result=dict(schema='ku-pg-driver-screen-v1',
        rows=[],stopped=[],execution_complete=False,diagnostic_only=True,formal_qualified=False,
        normal_score_eligible=False,candidate_accepted=False,goal_status='unknown',purpose=plan['purpose'])
    began=time.monotonic();selected=[]
    try:
        fixture.start();private=load(fixture.manifest_path)
        save(directory/'identity.json',dict(binding=binding,resources=plan['resources'],quality_policy=plan['quality_policy'],
            database_limits=load(fixture.state_path)['limits'],boot_sha256=sha('/proc/sys/kernel/random/boot_id'),
            driver_command=manifest['client']['command'],plan=plan))
        for config in plan['windows']:
            output=directory/('c%d'%config['concurrency']);output.mkdir();service=None;termination=None;quiet=False
            row=dict(concurrency=config['concurrency'],diagnostic_only=True,formal_qualified=False,
                normal_score_eligible=False,candidate_accepted=False,qualified=False,phases={},observation_complete=False)
            result['rows'].append(row)
            try:
                if fc.version_identity(plan,'B')!=binding:raise ValueError('product/driver identity changed before condition')
                harness=fc.Harness(Path(plan['versions']['B']['root']),output)
                harness.manifest=copy.deepcopy(harness.manifest);harness.manifest['db_url']=private['db_url']
                harness.db_domain=ExternalDomain(private['db_cgroup'])
                service=harness.start('ku',cores=plan['resources']['service_cores'],
                    memory=plan['resources']['service_memory_bytes'],output=output/'service')
                save(output/'service/service.json',service.startup)
                reports={}
                for phase,rate,seconds in (('warmup',config['rate'],config['warmup_seconds']),
                        ('score',config['rate'],config['score_seconds']),
                        ('recovery',plan['recovery']['rate'],plan['recovery']['seconds'])):
                    if service.process.poll() is not None:raise ValueError('service exited before '+phase)
                    database_before=harness.db_domain.snapshot()
                    report=harness.client(service,'db',rate,seconds,output/phase,concurrency=config['concurrency'],
                        timeout=plan['timeout_seconds'],runtime_metrics=True)
                    reports[phase]=report;cost=report['service_cost'];database_after=harness.db_domain.snapshot()
                    save(output/phase/'database-endpoints.json',dict(before=database_before,after=database_after))
                    cleanup=load(output/phase/'driver-cleanup.json')
                    if not (cleanup.get('process_reaped') is True and cleanup.get('exit_code')==0
                            and cleanup.get('cgroup_removed') is True and cleanup.get('after',{}).get('process_count')==0):
                        raise ValueError('owned driver failed or cleanup incomplete in '+phase)
                    if cost.get('service_exited') or service.process.poll() is not None:
                        raise ValueError('service exited during '+phase)
                    if database_after.get('process_count')==0:raise ValueError('database exited during '+phase)
                    if cost.get('sampler_error') or cost.get('sampler_overflow'):
                        raise ValueError('bounded resource observer failed during '+phase)
                    for label,before,after in (('service',cost['before'],cost['after']),
                            ('driver',{},cost['driver_after']),('database',database_before,database_after)):
                        if any(after.get('memory_events',{}).get(key,0)>before.get('memory_events',{}).get(key,0)
                                for key in ('max','oom','oom_kill','oom_group_kill')):
                            raise ValueError(label+' memory limit event in '+phase)
                    metrics=service.metrics(timeout=.5);save(output/phase/'runtime-after.json',metrics)
                    row['phases'][phase]=dict(assessment=report['assessment'],ledger=report['ledger'],
                        resources=report['resources'],first_errors=report['first_errors'],
                        configuration=report['configuration'],report=str(output/phase/'client.json'),
                        driver_cleanup_ok=True,database_endpoint_report=str(output/phase/'database-endpoints.json'))
                    save(output/'window.json',row)
                score,recovery=reports['score'],reports['recovery'];anchor=fc.measurement_clock(score['configuration'])
                cutoff=(anchor['origin_ns']+int(config['score_seconds']*1e9)) if anchor['available'] else None
                until=time.monotonic()+5;metrics={}
                while time.monotonic()<until:
                    metrics=service.metrics(timeout=.5);quiet=fc.quiet_metrics('ku',metrics)
                    if quiet:break
                    time.sleep(.1)
                recovery_elapsed=((time.monotonic_ns()-cutoff+score['configuration']['origin_monotonic_uncertainty_ns'])/1e9
                    if cutoff is not None else None)
                recovery_ok=(recovery['assessment']['functional_valid'] is True
                    and recovery['assessment']['accounting_valid'] is True
                    and recovery['ledger']['planned']==recovery['ledger']['correct_total']==200
                    and quiet and recovery_elapsed is not None and 0<=recovery_elapsed<=plan['recovery']['maximum_seconds'])
                row['recovery']=dict(passed=recovery_ok,elapsed_until_quiet_seconds=recovery_elapsed,
                    origin=plan['recovery']['origin'],maximum_seconds=plan['recovery']['maximum_seconds'])
                physical=service.domain.snapshot();termination=service.stop()
                row['termination']=termination;natural=fc.service_termination_succeeded('ku',termination)
                save(output/'full-lifecycle.json',fc.window_lifecycle_cost(score,physical,quiet,termination,metrics,recovery))
                save(output/'termination.json',termination)
                row.update(quiet=quiet,natural_reclamation=quiet and natural,observation_complete=quiet and natural,
                    warmup_qualified=False,score_qualified=False,
                    warmup_issues_preserved=reports['warmup']['assessment'],
                    interpretation='All conditions are diagnostic and unqualified. Failed warmup/arrival/requests are retained; '
                        'continued score observation is not a retry or a capacity/performance pass.')
                if not natural or not quiet:raise ValueError('physical quiet or natural reclamation failed')
                if fc.version_identity(plan,'B')!=binding:raise ValueError('product/driver identity changed during condition')
            except BaseException as error:
                row.update(observer_error=repr(error),observation_complete=False)
                result['stopped'].append(dict(concurrency=config['concurrency'],reason=repr(error)))
                if not isinstance(error,Exception):raise
            finally:
                if service is not None and termination is None:
                    try:
                        termination=service.stop();row['termination']=termination
                    except BaseException as error:
                        row['cleanup_error']=repr(error)
                        if not isinstance(error,Exception):raise
                save(output/'window.json',row);save(directory/'results.json',dict(rows=result['rows'],stopped=result['stopped']))
            if result['stopped']:break
    except BaseException as error:
        result['observer_error']=repr(error)
        if not isinstance(error,Exception):raise
    finally:
        try:result['database_cleanup']=fixture.stop()
        except Exception as error:result['database_cleanup']={'error':repr(error)}
        cleanup=result['database_cleanup'];dbok=(cleanup.get('forced') is False
            and cleanup.get('cgroup_removed') is True and cleanup.get('after',{}).get('process_count')==0)
        result.update(seconds=time.monotonic()-began,execution_complete=len(result['rows'])==2
            and all(r['observation_complete'] for r in result['rows']) and not result['stopped'] and dbok and 'observer_error' not in result)
        result['goal_status']='unknown' if result['execution_complete'] else 'blocked'
        save(directory/'summary.json',result);state['pg_driver_database_cleanup']=cleanup
        selected=[directory/name for name in ('summary.json','identity.json','results.json')]
        selected.extend(directory/('c%d'%n)/relative for n in (256,1024) for relative in (
            'window.json','full-lifecycle.json','termination.json','service/service.log'))
        selected.extend(directory/('c%d'%n)/phase/name for n in (256,1024) for phase in ('warmup','score','recovery')
            for name in ('client.json','driver-cleanup.json','runtime-after.json','runtime-metrics.json','database-endpoints.json'))
        result['small_artifact']=pg_failure_export(a,directory,output_name='pg-driver-observation',
            schema='ku-pg-driver-small-artifact-v1',selected=selected)
        save(a.root/'hosted-state.json',state)
    return result


def normal_self_probe(a,state):
    """Six windows at most, exact normal binaries, never a new performance score."""
    if os.geteuid()!=0:raise ValueError('normal SELF probe requires root cgroup control')
    require_normal_perf_preflight(a)
    plan=load(a.root/'frozen-plan.json')
    if plan!=normal_self_plan(plan.get('versions',{})):raise ValueError('matching frozen normal SELF plan required')
    bindings={name:fc.version_identity(plan,name) for name in ('B','R')}
    mode=bindings['B']['build_mode']
    expected=dict(formatting_mode='helpers',json_response_mode='text',json_execution_mode='worker',
        pg_execution_mode='async',pg_read_mode='int',native_flags={'batch':['-std=c11','-O2'],'server':['-std=c11','-O2','-pthread']})
    if bindings['B']['stack']!='ku' or mode!=expected or bindings['R']['stack']!='rust':
        raise ValueError('identified normal B/R binaries required; no D instrumentation')
    directory=a.root/'normal-self-probe';directory.mkdir(exist_ok=False)
    fixture=db.Database(a.root/'fixture',state['uid'],state['gid']);original=fc.Harness;private=None
    perf=shutil.which('perf',path=str(a.tools/'bin')+os.pathsep+os.environ.get('PATH',''))
    result=dict(schema='ku-normal-self-observation-v1',diagnostic_only=True,normal_score_eligible=False,
        formal_qualified=False,candidate_accepted=False,execution_complete=False,attribution_eligible=False,
        goal_status='unknown',rows=[],stopped=[],controls={},maximum_windows=6,perf_attempts=0,purpose=plan['purpose'])
    active={'sample':False}
    class SelfHarness(original):
        def __init__(self,*args,**kwargs):
            super().__init__(*args,**kwargs);self.manifest=copy.deepcopy(self.manifest)
            self.manifest['db_url']=private['db_url'];self.db_domain=ExternalDomain(private['db_cgroup'])
        def client(self,service,workload,rate,duration,output,**kwargs):
            parent=super().client
            if Path(output).name!='score':return parent(service,workload,rate,duration,output,**kwargs)
            return normal_self_client(lambda:parent(service,workload,rate,duration,output,**kwargs),
                service,output,duration,perf,sample=active['sample'])
    began=time.monotonic()
    try:
        fixture.start();private=load(fixture.manifest_path);fc.Harness=SelfHarness
        save(directory/'identity.json',dict(bindings=bindings,resources=plan['resources'],quality_policy=plan['quality_policy'],
            database_limits=load(fixture.state_path)['limits'],boot_sha256=sha('/proc/sys/kernel/random/boot_id'),
            kernel=platform.release(),machine=platform.machine(),perf=perf,purpose=plan['purpose']))
        save(directory/'kernel-context.json',kernel_context())
        diagnostic_command(['ps','-eo','pid,ppid,nlwp,rss,stat,comm'],directory/'processes-before.log')
        for name,turn in plan['order']:
            active['sample']=turn=='sample';output=directory/name/turn
            if active['sample']:result['perf_attempts']+=1
            try:
                row=fc.window(plan,name,plan['workload'],plan['rate'],plan['warmup_seconds'],plan['score_seconds'],
                    output,binding=bindings[name],job={'phase':'diagnostic','contract':plan['contract']})
            except Exception as error:
                row=load(output/'window.json') if (output/'window.json').is_file() else {}
                row['observer_error']=repr(error)
            row.update(name=name,turn=turn,diagnostic_only=True,normal_score_eligible=False,formal_qualified=False)
            sampled=output/'score/normal-self-observations.json'
            observation=load(sampled) if sampled.is_file() else {}
            row['observation_complete']=observation.get('record_complete') is True
            exports=[]
            if active['sample'] and row['observation_complete']:
                start=observation.get('client_started_monotonic_ns');end=observation.get('client_finished_monotonic_ns')
                if type(start) is not int or type(end) is not int or not 0<start<end:
                    row['observation_complete']=False;row['perf_export_error']='client monotonic sampling range unavailable'
            if active['sample'] and row['observation_complete']:
                raw=output/'score/perf.data';symfs=output/'score/symfs'
                sample_time=f'{start//10**9}.{start%10**9:09d},{end//10**9}.{end%10**9:09d}'
                row['perf_export_time_range']=dict(clock='mono',start_ns=start,end_ns=end,argument=sample_time,
                    scope='Full raw retained; SELF and PC exports limited to scored-client launch/return, excluding post-client perf stop/finalization')
                exports.append(diagnostic_command([perf,'report','-i',str(raw),'--symfs',str(symfs),'--time',sample_time,'--stdio','--no-children',
                    '--show-nr-samples','--call-graph','none','--sort','symbol,dso,comm','--percent-limit','0'],output/'score/perf-self.log',timeout=20,limit=4<<20))
                exports.append(diagnostic_command([perf,'script','-i',str(raw),'--symfs',str(symfs),'--time',sample_time,'--ns',
                    '--show-lost-events','--show-task-events','-F','+pid,+tid,+time,+ip,+sym,+dso,+symoff,+srcline,+misc'],
                    output/'score/perf-pcs.log',timeout=20,limit=8<<20))
                exports.append(diagnostic_command([perf,'report','-i',str(raw),'--header-only','--stdio'],output/'score/perf-header.log',timeout=2,limit=1<<20))
                exports.append(diagnostic_command([perf,'buildid-list','-i',str(raw)],output/'score/perf-buildids.log',timeout=2,limit=1<<20))
                row['kernel_chains']=kernel_chain_raw(raw,start,end)
                row['exports_complete']=all(x['complete'] for x in exports) and row['kernel_chains']['kernel_multiframe_samples']>0
            quality,signature=normal_diagnostic_quality(row)
            valid=(quality and row['observation_complete']
                and (not active['sample'] or row.get('exports_complete') is True))
            row['diagnostic_qualified']=valid;result['rows'].append(row)
            save(directory/'results.json',dict(rows=result['rows'],stopped=result['stopped']))
            if not valid:
                result['stopped'].append(dict(name=name,turn=turn,reason='functional/accounting/quality/observer/cleanup failure; no retry',quality_signature=signature));break
            if turn=='after':
                controls=[r.get('cpu_us') for r in result['rows'] if r['name']==name and r['turn']!='sample']
                ratio=max(controls)/min(controls) if len(controls)==2 and all(type(x)in(int,float) and x>0 for x in controls) else None
                sample=next(r for r in result['rows'] if r['name']==name and r['turn']=='sample')
                full_controls=[r.get('full_cpu_us') for r in result['rows'] if r['name']==name and r['turn']!='sample']
                full_sample=sample.get('full_cpu_us')
                full_ratio=(full_sample/(full_controls[0]*full_controls[1])**.5 if len(full_controls)==2
                    and all(type(x)in(int,float) and x>0 for x in [*full_controls,full_sample]) else None)
                client_sample=sample.get('cpu_us')
                client_ratio=client_sample/(controls[0]*controls[1])**.5 if ratio is not None and type(client_sample)in(int,float) and client_sample>0 else None
                result['controls'][name]=dict(unsampled_cpu_max_min=ratio,
                    drift_valid=ratio is not None and ratio<=plan['maximum_control_cpu_ratio'],
                    sampled_cpu_over_control_geomean=client_ratio,sampled_full_cpu_over_control_geomean=full_ratio,
                    cpu_contract='cpu_us: existing scored-client lifetime service CPU/correct; before/sample/after and drift use this same field',
                    full_cpu_contract='full_cpu_us: complete service lifecycle including post-client perf finalization/cleanup; descriptive separately',
                    scope='Single before/sample/after sequence, no CI or automatic adoption. Full CPU includes post-client perf finalization; '
                        'client CPU uses existing h.client lifetime. Neither ratio alone proves observer overhead.')
                if not result['controls'][name]['drift_valid']:
                    result['stopped'].append(dict(name=name,turn=turn,reason='unsampled CPU drift exceeds1.10 or missing; attribution invalid, no retry'));break
    except BaseException as error:
        result.update(observer_error=repr(error),observation_interrupted=not isinstance(error,Exception))
        if not isinstance(error,Exception):raise
    finally:
        fc.Harness=original
        try:result['database_cleanup']=fixture.stop()
        except Exception as error:result['database_cleanup']={'error':repr(error)}
        state['database_cleanup']=result['database_cleanup']
        cleanup=result['database_cleanup'];dbok=(cleanup.get('forced') is False and cleanup.get('cgroup_removed') is True and cleanup.get('after',{}).get('process_count')==0)
        complete=len(result['rows'])==6 and not result['stopped'] and dbok and 'observer_error' not in result
        result.update(seconds=time.monotonic()-began,execution_complete=complete,
            attribution_controls_passed=complete,
            attribution_review='Pending raw sample denominator/loss/sparsity/PC mapping and perturbation review; controls alone do not validate attribution.')
        result['goal_status']='unknown' if result['execution_complete'] else 'blocked'
        save(directory/'summary.json',result)
        selected=[directory/name for name in ('summary.json','identity.json','results.json','processes-before.log')]
        selected.extend(directory/name/turn/relative for name,turn in plan['order'] for relative in (
            'window.json','full-lifecycle.json','score/client.json','score/driver-cleanup.json',
            'score/normal-self-observations.json','score/perf-self.log','score/perf-pcs.log',
            'score/symbol-identities.json','score/maps.log','score/perf-record.log'))
        priority=[directory/name for name in ('summary.json','identity.json','results.json','kernel-context.json')]
        priority.extend(directory/name/turn/rel for name,turn in plan['order']
            for rel in ('window.json','full-lifecycle.json','score/client.json','score/normal-self-observations.json'))
        priority.extend(directory/name/'sample/score'/rel for name in ('B','R')
            for rel in ('perf-header.log','perf-buildids.log','perf-record.log','perf-self.log','perf-pcs.log','symbol-identities.json'))
        result['raw_artifact']=kernel_raw_export(a,directory,bindings)
        result['small_artifact']=pg_failure_export(a,directory,output_name='normal-self-observation',schema='ku-normal-self-small-artifact-v1',selected=list(dict.fromkeys(priority+selected)))
        save(a.root/'hosted-state.json',state)
    return result


def diagnostic(a,state,followup=False):
    if getattr(a,'memory_owner_probe',False):return memory_owner_probe(a,state)
    if getattr(a,'pg_driver_screen',False):return pg_driver_screen(a,state)
    if getattr(a,'pg_queue_screen',False) or getattr(a,'http_short_screen',False):return pg_queue_screen(a,state)
    if getattr(a,'normal_self_probe',False):return normal_self_probe(a,state)
    if getattr(a,'pg_failure_probe',False):return pg_failure_probe(a,state)
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
                    admission={'passed':False,'execution_complete':False,'reason':'environment controls blocked; admission not started'},
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
                row.update(turn=turn,workload=case['workload'],name=name,diagnostic_only=name in ('D','BT'),
                    normal_score_eligible=name not in ('D','BT'),formal_qualified=False,candidate_accepted=False)
                if name=='BT':row.update(json_execution_mode='task',original_worker_score_eligible=False,
                    scope='Native Task JSON local-value execution diagnostic; all ordinary quality checks remain required')
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
                            '--show-lost-events','--show-task-events','-F','+pid,+tid,+time,+ip,+sym,+dso,+symoff,+srcline,+misc'],
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
                candidate_screens={n:screen_pairs(case_rows,case,plan['latency_slo_ms'][case['workload']],n,'B' if n=='BT' else 'A')
                    for n in plan.get('candidate_versions',['B']) if any(r['name']==n for r in case_rows)} if followup else {},
                task_json_screen=task_json_screen(case_rows,case,plan['latency_slo_ms'][case['workload']]) if followup else None,
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
        admission=(required_screen_admission(plan,a.root,state,boot,directory/'admission') if not stopped else
            dict(passed=False,execution_complete=False,reason='normal functional/environment stop; supplementary gate not started'))
        expected_rows=sum(len(diagnostic_turns(plan,c,followup)) for c in plan['cases'])
        axes=diagnostic_axes(plan,rows,expected_rows,stopped,control_row,growth_rows,followup,admission)
        result=dict(**axes,axes=axes,rows=rows,stopped=stopped,
            admission=admission,
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


def required_screen_admission(plan, root, state, boot, output):
    """Keep old B receipts unchanged; explicit Task JSON adds its own real gate."""
    worker=required_admission(plan,root,state,boot,output)
    if not (plan.get('task_json_screen') or plan.get('final_task_json')):return worker
    if plan.get('task_json_admission',{}).get('name')!='BT':
        raise ValueError('explicit Task JSON mode requires its separate BT admission contract')
    if worker.get('passed') is True and worker.get('execution_complete') is True:
        task=required_admission(plan,root,state,boot,Path(output)/'BT',name='BT')
    else:
        task=dict(name='BT',passed=False,execution_complete=False,goal_status='blocked',
            reason='B admission did not pass; BT supplementary gate was not started',not_started=True)
    gates={'B':worker,'BT':task}
    complete=all(row.get('execution_complete') is True for row in gates.values())
    passed=complete and all(row.get('passed') is True for row in gates.values())
    result=dict(schema='ku-hosted-admission-series-v1',passed=passed,execution_complete=complete,
        goal_status='met' if passed else 'blocked',gates=gates,
        normal_score_eligible=False,counts_as_timed_window=False,
        scope='Separate serial B worker and BT Task admission/recovery; no throughput score or old-goal substitution')
    save(Path(output)/'admission-series.json',result);return result


def required_admission(plan, root, state, boot, output, name='B'):
    """One bounded gate; reuse only success bound to this boot/name/exact build.

    Retained failure/interruption is blocked evidence, never an implicit retry.
    This step deliberately contributes no fixed/diagnostic timed windows.
    """
    contract=plan.get('admission',ADMISSION) if name=='B' else plan.get('task_json_admission')
    if not isinstance(contract,dict) or contract.get('name')!=name or name not in plan['versions']:
        raise ValueError('missing admission contract/build for '+name)
    key='admission' if name=='B' else 'admission-'+name
    old=state.setdefault('steps',{}).get(key)
    if old:
        receipt=copy.deepcopy(old.get('result')) if isinstance(old.get('result'),dict) else {}
        try:
            matching=(receipt.get('name')==name and receipt.get('boot_sha256')==boot
                and receipt.get('binding')==fc.version_identity(plan,name)
                and receipt.get('resources')==plan['resources'] and receipt.get('contract')==contract)
        except Exception:matching=False
        if old.get('state')=='completed' and matching and receipt.get('passed') is True and receipt.get('execution_complete') is True:
            return dict(receipt,reused_same_host_build=True)
        return dict(receipt,name=name,passed=False,execution_complete=False,goal_status='blocked',no_retry=True,
            reason='retained admission is failed, incomplete, or not bound to the current boot/build/resources; no implicit rerun')
    def execute():
        receipt=admission_window(plan,output,name=name)
        receipt.update(name=name,boot_sha256=boot,resources=copy.deepcopy(plan['resources']),contract=copy.deepcopy(contract))
        save(Path(output)/'admission-window.json',receipt)
        return receipt
    try:
        return step(state,root,key,execute)
    except Exception as error:
        # step retains the failed attempt. Do not turn a driver/setup failure
        # into a retry or let downstream successful cleanup erase this axis.
        return dict(name=name,passed=False,execution_complete=False,goal_status='blocked',no_retry=True,
            error=repr(error),reason='admission attempt failed; retained without retry',evidence=str(Path(output)/'admission-window.json'))


def admission_window(plan, output, name='B'):
    """Finite real C/A/P gate, separate from throughput and normal CPU scores."""
    output=Path(output);spec=plan['versions'][name];binding=fc.version_identity(plan,name)
    harness=fc.Harness(Path(spec['root']),output);service=None;interrupted=None
    row=dict(schema='ku-hosted-admission-v1',name=name,passed=False,execution_complete=False,
        counts_as_timed_window=False,binding=binding,source_commit=binding['source_commit'],normal_score_eligible=False,
        scope='Finite actual admission/recovery; no maximum RPS or Rust comparison')
    try:
        service=harness.start(spec.get('stack','ku'),cores=plan['resources']['service_cores'],
            memory=plan['resources']['service_memory_bytes'],output=output/'service',extra_env=spec.get('env',{}))
        row['gate']=harness.admission_pressure(service,output/'admission')
        row['execution_complete']=True
        row['identity_unchanged']=fc.version_identity(plan,name)==binding
    except BaseException as error:
        row['error']=repr(error)
        if isinstance(error,(KeyboardInterrupt,SystemExit)):raise
    finally:
        if service is not None:
            try:row['termination']=service.stop()
            except BaseException as error:
                row['termination_error']=repr(error)
                if isinstance(error,(KeyboardInterrupt,SystemExit)):interrupted=error
        try:row['cleanup']=harness.close()
        except BaseException as error:
            row['cleanup_error']=repr(error)
            if isinstance(error,(KeyboardInterrupt,SystemExit)):interrupted=error
        row['natural_reclamation']=service is not None and fc.service_termination_succeeded('ku',row.get('termination',{}))
        row['passed']=bool(row.get('gate',{}).get('passed') is True and row.get('identity_unchanged') is True
            and row['natural_reclamation'] and 'cleanup_error' not in row and 'error' not in row)
        row['goal_status']='met' if row['passed'] else 'blocked'
        save(output/'admission-window.json',row)
        if interrupted is not None:raise interrupted
    return row


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


def acceptance_result(final, capacity, stability, comparison_goal, admission=None):
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
    admission_ok=isinstance(admission,dict) and admission.get('passed') is True and admission.get('execution_complete') is True
    functional=bool(final.get('rows')) and all(r.get('functional_valid') is True and r.get('accounting_valid') is True for r in final['rows']) and cap_function and stable_function and admission_ok
    cap_status='not_met' if any(x['goal_status']=='not_met' for x in scenes) else 'met' if all(x['goal_status']=='met' for x in scenes) else 'unknown'
    statuses=[comparison_goal,cap_status,'met' if stability.get('formal_qualified') is True else 'unknown']
    known_failure=any(r.get('functional_valid') is False or r.get('accounting_valid') is False for r in final.get('rows',[])+rows)
    goal='not_met' if known_failure or 'not_met' in statuses else 'met' if all(x=='met' for x in statuses) and functional else 'unknown'
    if not admission_ok:goal='blocked'
    return {'execution_complete':final.get('execution_complete') is True and capacity.get('execution_complete') is True and stability.get('execution_complete') is True and admission_ok,
        'goal_status':goal,'functional_acceptance':functional,'functional_axes':{'capacity_recovery':cap_function,'continuous':stable_function,'actual_admission':admission_ok},
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
        (parts[0] in ('diagnostic','normal-self-probe','normal-perf-preflight') and (path.name=='perf.data' or 'symfs' in parts)) or 'bin' in parts or path.name=='libpq.so')


def immutable_benchmark_json(path,root):
    parts=path.relative_to(root).parts
    if len(parts)<6 or parts[1:5]!=('src','tests','benchmarks','cross_stack'):return False
    relative=parts[5:]
    return relative in [('protocol.json',),('task_mix_protocol.json',),
        ('corpora','object.json'),('corpora','array.json'),('corpora','escape.json')]


def full_archive(a,state):
    destination=a.root/'ku-performance-v2-evidence.tar.gz'
    if destination.exists():raise ValueError('archive already exists; never overwrite retained evidence')
    entries=[];limit=ARCHIVE_LIMIT
    with tarfile.open(destination,'x:gz',compresslevel=6) as tar:
        for path in sorted(a.root.rglob('*')):
            if not path.is_file() or path.is_symlink() or not evidence_file(path,a.root):continue
            raw=path.read_bytes();original=hashlib.sha256(raw).hexdigest();data=raw
            if path.suffix=='.json' and not immutable_benchmark_json(path,a.root):
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
        redaction_scope='Receipts/text are redacted; frozen synthetic protocol/corpora JSON and raw perf/ELF remain byte-exact. Raw binaries may retain ephemeral private-fixture values.',
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
    p.add_argument('--phase',required=True,choices=['build','run','tail','lifecycle','compiler','diagnostic','followup-screen','perf-preflight','archive']);p.add_argument('--root',required=True,type=Path)
    p.add_argument('--source',required=True,type=Path);p.add_argument('--tools',required=True,type=Path)
    p.add_argument('--baseline',default=BASELINE);p.add_argument('--candidate',required=True)
    p.add_argument('--prepatch',default=FOLLOWUP_PREPATCH)
    p.add_argument('--screen-variants',default='',help='Up to seven explicit full commits, comma separated; normal preregistered short ablations only')
    p.add_argument('--task-json-screen',action='store_true',help='Explicit separate native Task JSON comparison; followup-screen only, never an implicit product-capability switch')
    p.add_argument('--common-http-screen',action='store_true',help='Explicit shared HTTP candidate screen; requires followup-screen and --task-json-screen; omits unchanged worker/HC windows')
    p.add_argument('--final-task-json',action='store_true',help='Separate formal Task JSON15-window supplement; full/performance-v2 only, never replaces original80/16')
    p.add_argument('--pg-failure-probe',action='store_true',help='One normal B PG12800 failure observation; diagnostic build-set only, no retries or performance score')
    p.add_argument('--normal-self-probe',action='store_true',help='Fixed archived normal B/R kernel chains; diagnostic only, no fresh builds or acceptance')
    p.add_argument('--http-short-goal',choices=['cpu','memory'],default='cpu',help='Preregistered target for explicit HTTP short screen only; frozen into identity/plan')
    p.add_argument('--http-short-screen',action='store_true',help='Explicit23-window control+normal A/B single HTTP candidate diagnostic; frozen b765f9b prepatch, no capacity or adoption claim')
    p.add_argument('--memory-owner-probe',action='store_true',help='Single diagnostic-only owner-map window; fixed archive reuse required, no scoring')
    p.add_argument('--pg-driver-screen',action='store_true',help='Two fresh normal B PG diagnostic arms,256/1024 sender concurrency; no capacity ranking')
    p.add_argument('--pg-queue-screen',action='store_true',help='Eight normal A/B PG fix diagnostic windows; frozen f39c673 prepatch required, no matrix or formal capacity claim')
    p.add_argument('--build-set',choices=['full','diagnostic','followup-screen'],default='full')
    a=p.parse_args(argv)
    if a.memory_owner_probe and a.phase not in ('diagnostic','archive'):p.error('--memory-owner-probe requires fixed archive reuse before diagnostic')
    if a.final_task_json and a.phase not in ('build','run','archive'):p.error('--final-task-json only supports build/run/archive in the complete performance-v2 flow')
    if a.pg_failure_probe and a.phase not in ('build','diagnostic','archive'):p.error('--pg-failure-probe only supports fresh build/diagnostic/archive')
    if a.normal_self_probe and a.phase not in ('build','diagnostic','perf-preflight','archive'):p.error('--normal-self-probe only supports fixed build-registration/diagnostic/preflight/archive')
    if a.phase=='perf-preflight' and not a.normal_self_probe:p.error('perf-preflight requires explicit normal-self mode')
    if a.http_short_screen and a.phase not in ('build','diagnostic','archive'):p.error('--http-short-screen only supports fresh build/diagnostic/archive')
    if a.pg_driver_screen and a.phase not in ('build','diagnostic','archive'):p.error('--pg-driver-screen only supports fresh build/diagnostic/archive')
    if a.pg_queue_screen and a.phase not in ('build','diagnostic','archive'):p.error('--pg-queue-screen only supports fresh build/diagnostic/archive')
    try:validate_screen_selection(a,a.build_set)
    except ValueError as error:p.error(str(error))
    a.root=a.root.resolve();a.source=a.source.resolve(strict=a.phase!='archive');a.tools=a.tools.resolve(strict=a.phase!='archive')
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
    a.root.mkdir(parents=True,exist_ok=True)
    if a.phase=='perf-preflight':
        def preflight_stop(*unused):raise KeyboardInterrupt('perf preflight wall bound')
        signal.signal(signal.SIGALRM,preflight_stop)
        signal.alarm(19)
        try:result=normal_perf_preflight(a)
        finally:signal.alarm(0)
        print(json.dumps(result));return 0 if result.get('complete') is True else 2
    identity=hosted_identity(a,a.build_set)
    state=load(a.root/'hosted-state.json') if (a.root/'hosted-state.json').exists() else {'identity':identity,'steps':{},'goal_status':'unknown'}
    if state['identity']!=identity and a.phase!='archive':p.error('identity changed; new root required')
    if a.phase=='archive':result=archive(a,state)
    else:
        def stop(*unused):raise KeyboardInterrupt('hosted coordinator interrupted')
        signal.signal(signal.SIGTERM,stop);signal.signal(signal.SIGALRM,stop)
        if a.phase in ('run','tail','lifecycle','compiler'):signal.alarm((150 if a.final_task_json else 130)*60-30)
        if a.phase=='diagnostic':signal.alarm(120 if a.memory_owner_probe else 690 if a.http_short_screen else 180 if a.pg_driver_screen else 120 if a.pg_failure_probe else 330 if a.normal_self_probe else 420 if a.pg_queue_screen else 20*60-30)
        if a.phase=='followup-screen':signal.alarm((25+3*len(a.screen_variants))*60-30)
        operation={'build':build,'run':run,'tail':lambda a,s:run(a,s,tail_only=True),
            'lifecycle':lambda a,s:run(a,s,tail_only=True,stability_only=True),'compiler':compiler,'diagnostic':diagnostic,
            'followup-screen':lambda a,s:diagnostic(a,s,followup=True)}[a.phase]
        result=step(state,a.root,a.phase,lambda:operation(a,state),resume=a.phase in ('run','tail','lifecycle','compiler'))
    if a.phase!='archive':state['goal_status']=result.get('goal_status','unknown');save(a.root/'hosted-state.json',state)
    print(json.dumps(result));return 2 if a.phase in ('run','tail','lifecycle','diagnostic','followup-screen') and result.get('execution_complete') is not True else 0

if __name__=='__main__':raise SystemExit(main())
