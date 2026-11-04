#!/usr/bin/env python3
"""One bounded normal PG read-credit screen; reuse existing build/driver/harness."""
import argparse
import copy
import json
import os
from pathlib import Path
import platform
import re
import signal
import shutil
import sys
import time


BASELINE = 'd05ca1551d4585011499d81d0ae828c4173306ff'
PROBE_MARKERS = ('ku_rs_', 'ku_mp_', 'ku_net_probe_', 'KU_MECHANISM ')


def helpers(source):
    sys.path.insert(0,str(source/'tests/benchmarks/cross_stack'))
    global h, fc, reuse
    import hosted_cycle as h
    import focus_cycle as fc
    import reuse_build as reuse
    if h.bl.RUST_TOOLCHAIN!='1.99.1':raise ValueError('frozen Rust1.99.1 tools required')


def mode(typed=False):
    return dict(formatting_mode='helpers',json_response_mode='text',json_execution_mode='worker',
        pg_execution_mode='async',pg_read_mode='typed' if typed else 'int',
        native_flags={'batch':['-std=c11','-O2'],'server':['-std=c11','-O2','-pthread']})


def make_plan(versions,candidate,resources,policy,workload='db'):
    expected={'A','B'} if workload=='ok' else {'A','B','AP','BP'}
    if workload not in ('db','ok') or set(versions)!=expected or not re.fullmatch('[0-9a-f]{40}',candidate) or candidate==BASELINE:
        raise ValueError('exact normal/typed d05 and distinct full candidate required')
    if any(versions[n]['source_commit']!=(BASELINE if n in ('A','AP') else candidate) for n in versions):
        raise ValueError('normal/typed source roles differ')
    windows=[]
    paths=(('ok',('A','B'),'text'),) if workload=='ok' else (('ordinary',('A','B'),'pg-strict'),('typed',('AP','BP'),'typed-pg-strict'))
    for path,names,contract in paths:
        for group in range(1,4):
            order=names if group%2 else names[::-1]
            windows.extend(dict(path=path,name=n,group=group,phase='normal',workload=workload,rate=2000 if workload=='ok' else 200,
                warmup_seconds=3,score_seconds=15,contract=contract) for n in order)
    plan=dict(schema='ku-pg-read-credit-coarse-v1',candidate=candidate,baseline=BASELINE,versions=versions,
        resources=copy.deepcopy(resources),quality_policy=copy.deepcopy(policy),windows=windows,
        maximum_windows=12,maximum_replacements=0,observation_alarm_seconds=480,
        bootstrap_seed=20261023,bootstrap_resamples=20000,minimum_pairs=3,latency_p99_SLO_ms=250,
        expansion_rule={'at_least_one_path_CPU_point_maximum':.98,'at_least_one_path_CPU_upper95_strictly_below':1,
            'any_path_clear_CPU_or_tail_regression_blocks':'lower95>1 for CPU/p99/p999',
            'all_windows':'original correctness/accounting/arrival/resource/cleanup checks and p99<=250ms',
            'memory':'retain original current metric; not a3% guard or adoption result'},
        diagnostic_only=True,instrumented=False,normal_score_eligible=False,formal_qualified=False,
        candidate_accepted=False,purpose='Exactly ordinary3pairs then typed3pairs AB/BA/AB; original200rps '
            'business/SQL/driver/resources/deadline/cancel/quantum/strict reset and natural cleanup. '
            'Net-benefit-space screen only; no inherited scores, replacement, appended windows, formal3% guard or automatic adoption.')
    if workload=='ok':
        plan.update(schema='ku-http-normal-coarse-v1',screen_workload='ok',maximum_windows=6,
            purpose='Exactly ordinary /ok3pairs AB/BA/AB; original2000rps HTTP/driver/resources/deadline/cancel/quantum '
                'and natural cleanup. No typed preparation or PG observation. Net-benefit-space screen only; '
                'no inherited scores, replacement, appended windows, formal3% guard or automatic adoption.')
    return plan


def check_exact_hashes(binding,expected,protocol_sha):
    # Existing pg-input-guard bindings have six files; cache bindings may
    # additionally declare protocol. Verify it without mutating either form.
    actual={k:v['sha256'] for k,v in binding['files'].items()}
    wanted=copy.deepcopy(expected)
    if protocol_sha!=wanted['declared:protocol']:
        raise ValueError('actual manifest protocol differs from frozen cached protocol')
    if 'declared:protocol' not in actual:wanted.pop('declared:protocol')
    if actual!=wanted:raise ValueError('cached original byte identities differ')


def check_binding(plan,name,*,typed=False,exact=None):
    binding=fc.version_identity(plan,name)
    manifest=h.load(Path(plan['versions'][name]['root'])/'manifest.json')
    h.bl.require_rust_manifest(manifest)
    if binding['stack']!='ku' or binding['build_mode']!=mode(typed):raise ValueError('normal helpers/O2 mode differs: '+name)
    build=manifest['stacks']['ku']['build']
    if build.get('instrumented') or build.get('diagnostic'):raise ValueError('instrumented product rejected: '+name)
    source=Path(binding['files']['generated C']['path']).read_text()
    if any(marker in source for marker in PROBE_MARKERS):raise ValueError('temporary probe in normal source: '+name)
    if exact is not None:check_exact_hashes(binding,exact,h.sha(manifest['protocol']))
    return binding


def prepare(a):
    if os.geteuid()==0:raise ValueError('native-only preparation must run as the original build user')
    original=h.load(a.root/'frozen-plan.json');declared=h.load(a.root/'build-identities.json');state=h.load(a.root/'hosted-state.json')
    candidate=original.get('candidate')
    workload=getattr(a,'workload','db')
    if original.get('prepatch')!=BASELINE or set(original.get('versions',{}))!={'A','B'}:
        raise ValueError('existing ordinary d05 A/B build-only root required')
    if original['resources']!=h.RESOURCES or original['quality_policy']!=h.POLICY:
        raise ValueError('original resource/quality contract differs')
    if original.get('pg_input_guard_workload','db')!=workload:
        raise ValueError('screen workload differs from the original normal build plan')
    identity=state['identity']
    if identity.get('candidate')!=candidate or identity.get('baseline')!=BASELINE or identity.get('rust_toolchain')!='1.99.1':
        raise ValueError('hosted-state build identity differs')
    bindings={n:check_binding(original,n,exact=reuse.CURRENT_PG_FILES if n=='A' else None) for n in ('A','B')}
    if bindings!=declared:raise ValueError('normal frozen build identities changed')
    if any(bindings['A']['files'][k]['sha256']!=bindings['B']['files'][k]['sha256'] for k in ('expanded source','driver ELF','libpq')):
        raise ValueError('normal input/protocol/driver/library differs')
    driver=h.load(a.root/'Driver/manifest.json');client=copy.deepcopy(driver['client']);protocol=Path(driver['protocol'])
    protocol_sha=h.sha(protocol)
    if protocol_sha!=reuse.CURRENT_PG_FILES['declared:protocol'] or any(
            h.sha(h.load(Path(original['versions'][n]['root'])/'manifest.json')['protocol'])!=protocol_sha for n in ('A','B')):
        raise ValueError('normal actual protocol differs from fixed Driver protocol')
    directory=a.root/'pg-read-screen';directory.mkdir(exist_ok=False)
    cached_binding=None;typed_pairs=(('AP','A'),('BP','B')) if workload=='db' else ()
    if typed_pairs:
        cached=a.root/'verified-current-cache/BP';cm=h.load(cached/'manifest.json')
        cached_spec=dict(root=str(cached),stack='ku',source_commit=BASELINE,
            binary_sha256=reuse.CURRENT_TYPED_PG_FILES['ELF'],
            artifacts={'protocol':dict(path=cm['protocol'],sha256=h.sha(cm['protocol']))})
        cached_binding=check_binding({'versions':{'AP':cached_spec}},'AP',typed=True,exact=reuse.CURRENT_TYPED_PG_FILES)
        ap=a.root/'AP'
        if ap.exists() or state['steps'].get('build-AP') or state['steps'].get('build-BP'):
            raise ValueError('fresh native typed preparation required; retained attempts are not overwritten')
        ap.mkdir();h.save(ap/'manifest.json',copy.deepcopy(cm))
        state['steps']['build-AP']=dict(state='completed',result=copy.deepcopy(cm),build_repeated=False,
            provenance='Exact verified-current-cache/BP byte identities; no old windows imported')
        h.save(a.root/'hosted-state.json',state)
        h.prepare_common_typed_guards(a,state,original,bindings,client,protocol,kinds=('pg',))
    versions=copy.deepcopy(original['versions'])
    for name in ('A','B'):
        versions[name]['artifacts']={**versions[name].get('artifacts',{}),
            'protocol':dict(path=str(protocol),sha256=protocol_sha)}
    for name,parent in typed_pairs:
        m=h.load(a.root/name/'manifest.json');binary=m['stacks']['ku']['command'][0]
        versions[name]=dict(root=str(a.root/name),stack='ku',source_commit=versions[parent]['source_commit'],
            binary_sha256=h.sha(binary),rust_toolchain='1.99.1',
            artifacts={'protocol':dict(path=str(protocol),sha256=h.sha(protocol))})
    plan=make_plan(versions,candidate,h.RESOURCES,h.POLICY,workload)
    all_bindings={n:check_binding(plan,n,typed=n.endswith('P'),exact=reuse.CURRENT_PG_FILES if n=='A' else reuse.CURRENT_TYPED_PG_FILES if n=='AP' else None) for n in versions}
    for n,parent in typed_pairs:
        if all_bindings[n]['files']['Ku CLI']['sha256']!=all_bindings[parent]['files']['Ku CLI']['sha256']:
            raise ValueError('typed arm not using its original normal parent CLI: '+n)
    if typed_pairs:
        for field in ('expanded source','driver ELF','libpq','declared:protocol'):
            if all_bindings['AP']['files'][field]['sha256']!=all_bindings['BP']['files'][field]['sha256']:
                raise ValueError('typed paired input differs: '+field)
    if {n:fc.version_identity(original,n) for n in ('A','B')}!=bindings:
        raise ValueError('normal artifacts changed during typed preparation')
    for name in (('B','BP') if typed_pairs else ('B',)):
        exported=directory/'inputs'/name;exported.mkdir(parents=True)
        shutil.copyfile(all_bindings[name]['files']['generated C']['path'],exported/'server.c')
        native=h.load(a.root/name/'manifest.json')['stacks']['ku']['build']['native']['server']
        h.save(exported/'native-compile.json',dict(native=native,scored_generated_C_sha256=h.sha(exported/'server.c'),
            provenance='Direct copy from the actual normal native build; no re-emission or instrumentation.'))
    h.save(directory/'frozen-plan.json',plan);h.save(directory/'build-identities.json',all_bindings)
    h.save(directory/'prepare.json',dict(entry_sha256=h.sha(__file__),normal_plan_sha256=h.sha(a.root/'frozen-plan.json'),
        helper_sha256={n:h.sha(Path(h.__file__).parent/n) for n in ('hosted_cycle.py','focus_cycle.py','build_linux.py','reuse_build.py')},
        cached_typed_binding=cached_binding,baseline_native_rebuilt=False,new_Rust_CLI_builds=0,
        candidate_typed_native_only=bool(typed_pairs),imported_measurement_windows=0,uid=state['uid'],gid=state['gid']))
    h.save(a.root/'hosted-state.json',state)
    return dict(prepared=True,window_count=plan['maximum_windows'],CLI_rebuilt=False,plan=str(directory/'frozen-plan.json'))


def comparison(plan,rows,complete):
    out=dict(execution_complete=complete,paths={},reasons=[],worth_expanded_validation=False,
        original_3pct_guard_passed=False,candidate_accepted=False,formal_qualified=False)
    if not complete or len(rows)!=plan['maximum_windows']:
        out['reasons'].append('incomplete fixed'+str(plan['maximum_windows'])+' windows; no replacement or expansion');return out
    paths=(('ok',('A','B')),) if plan.get('screen_workload')=='ok' else (('ordinary',('A','B')),('typed',('AP','BP')))
    for path,names in paths:
        selected=[r for r in rows if r['path']==path];pairs=[]
        for group in range(1,4):
            pair={r['name']:r for r in selected if r['group']==group}
            if set(pair)!=set(names):raise ValueError('missing/duplicate coarse pair')
            if pair[names[0]].get('comparison_contract')!=pair[names[1]].get('comparison_contract'):
                out['reasons'].append(path+' paired comparison contract differs')
            pairs.append(pair)
        metrics={}
        for key in ('full_cpu_us','memory.steady_median_bytes','latency.p99_ms','latency.p999_ms','attempt_delay.p99_ms','attempt_delay.p999_ms','send_delay.p99_ms','send_delay.p999_ms'):
            metrics[key]=fc.paired_interval([fc.metric(p[names[1]],key) for p in pairs],
                [fc.metric(p[names[0]],key) for p in pairs],minimum=3,
                seed=plan['bootstrap_seed'],resamples=plan['bootstrap_resamples'])
        for key in ('full_cpu_us','latency.p99_ms','latency.p999_ms'):
            interval=metrics[key]
            if interval.get('status')!='estimated':out['reasons'].append(path+' '+key+' unavailable')
            elif interval['lower95']>1:out['reasons'].append(path+' clear '+key+' regression in coarse pairs')
        cpu=metrics['full_cpu_us'];space=cpu.get('status')=='estimated' and cpu['estimate']<=.98 and cpu['upper95']<1
        out['paths'][path]=dict(metrics=metrics,CPU_net_gain_space=space,
            row_latencies=[dict(name=r['name'],group=r['group'],p99=r.get('latency',{}).get('p99_ms'),p999=r.get('latency',{}).get('p999_ms')) for r in selected])
    if not any(p['CPU_net_gain_space'] for p in out['paths'].values()):
        out['reasons'].append('neither normal path demonstrates preregistered>=2% CPU space with upper95<1')
    out['worth_expanded_validation']=not out['reasons']
    out['boundary']='Descriptive three-pair screen only; no normal3% memory/CPU guard, per-kernel attribution, original-goal completion or adoption. Raw order/host/tail review and separate mechanism comparisons remain required.'
    return out


def run(a):
    if os.geteuid()!=0:raise ValueError('run phase requires root cgroup control')
    directory=a.root/'pg-read-screen';plan=h.load(directory/'frozen-plan.json');prepared=h.load(directory/'prepare.json')
    if plan!=make_plan(plan['versions'],plan['candidate'],h.RESOURCES,h.POLICY,getattr(a,'workload','db')):raise ValueError('frozen screen plan differs')
    if prepared['entry_sha256']!=h.sha(__file__) or prepared['normal_plan_sha256']!=h.sha(a.root/'frozen-plan.json'):
        raise ValueError('entry or original normal plan changed')
    if any(h.sha(Path(h.__file__).parent/n)!=v for n,v in prepared['helper_sha256'].items()):raise ValueError('helper changed after preparation')
    bindings=h.load(directory/'build-identities.json')
    if {n:check_binding(plan,n,typed=n.endswith('P')) for n in plan['versions']}!=bindings:raise ValueError('normal binaries changed')
    output=directory/'windows';output.mkdir(exist_ok=False)
    result=dict(schema=plan['schema'],execution_complete=False,rows=[],stopped=[],candidate_accepted=False,
        diagnostic_only=True,instrumented=False,formal_qualified=False,normal_score_eligible=False)
    fixture=h.db.Database(a.root/'fixture',prepared['uid'],prepared['gid']);original=fc.Harness;private=None;began=time.monotonic()
    class PrivateHarness(original):
        def __init__(self,*args,**kwargs):
            super().__init__(*args,**kwargs);self.manifest=copy.deepcopy(self.manifest)
            self.manifest['db_url']=private['db_url'];self.db_domain=h.ExternalDomain(private['db_cgroup'])
    def deadline(*unused):raise TimeoutError('fixed coarse observer bound reached; no retry')
    def interrupted(*unused):raise KeyboardInterrupt('coarse observer interrupted; preserve rows and cleanup')
    previous={s:signal.getsignal(s) for s in (signal.SIGALRM,signal.SIGTERM)}
    signal.signal(signal.SIGALRM,deadline);signal.signal(signal.SIGTERM,interrupted);signal.alarm(plan['observation_alarm_seconds'])
    try:
        fixture.start();private=h.load(fixture.manifest_path);fc.Harness=PrivateHarness
        h.save(directory/'identity.json',dict(bindings=bindings,kernel=platform.release(),boot_sha256=h.sha('/proc/sys/kernel/random/boot_id'),
            private_database_limits=h.load(fixture.state_path)['limits'],resources=plan['resources'],quality_policy=plan['quality_policy'],
            scope='One live fixture shared serially by all'+str(plan['maximum_windows'])+' windows; no independent environment qualification or formal acceptance.'))
        for config in plan['windows']:
            name=config['name'];dest=output/config['path']/f"g{config['group']}-{name}"
            if fc.version_identity(plan,name)!=bindings[name]:raise ValueError('identity changed before window')
            row=fc.window(plan,name,config['workload'],config['rate'],config['warmup_seconds'],config['score_seconds'],dest,
                binding=bindings[name],job={'phase':'diagnostic','contract':config['contract']})
            row.update(path=config['path'],group=config['group'],phase='normal')
            valid,_=h.normal_diagnostic_quality(row);p99=row.get('latency',{}).get('p99_ms')
            valid=valid and type(p99) in (int,float) and 0<=p99<=plan['latency_p99_SLO_ms']
            row.update(coarse_window_valid=valid,latency_SLO_ms=plan['latency_p99_SLO_ms'])
            result['rows'].append(row);h.save(directory/'results.json',result)
            if not valid:
                result['stopped'].append(dict(path=config['path'],name=name,group=config['group'],reason='original quality/correctness/resource/latency/cleanup failure; zero retry'));break
    except BaseException as error:
        result.update(observer_error=repr(error),observer_interrupted=not isinstance(error,Exception))
        if not isinstance(error,Exception):raise
    finally:
        signal.alarm(0);fc.Harness=original
        try:result['database_cleanup']=fixture.stop()
        except Exception as error:result['database_cleanup']={'error':repr(error)}
        for s,handler in previous.items():signal.signal(s,handler)
        cleanup=result['database_cleanup'];dbok=cleanup.get('forced') is False and cleanup.get('cgroup_removed') is True and cleanup.get('after',{}).get('process_count')==0
        result.update(seconds=time.monotonic()-began,execution_complete=len(result['rows'])==plan['maximum_windows'] and not result['stopped'] and dbok and 'observer_error' not in result)
        result['comparison']=comparison(plan,result['rows'],result['execution_complete']);h.save(directory/'summary.json',result)
    return dict(execution_complete=result['execution_complete'],comparison=result['comparison'],summary=str(directory/'summary.json'))


def main():
    p=argparse.ArgumentParser();p.add_argument('--root',required=True,type=Path);p.add_argument('--source',required=True,type=Path)
    p.add_argument('--tools',required=True,type=Path);p.add_argument('--phase',choices=('prepare','run'),required=True)
    p.add_argument('--workload',choices=('db','ok'),default='db');a=p.parse_args()
    a.root=a.root.resolve();a.source=a.source.resolve();a.tools=a.tools.resolve();helpers(a.source)
    if not a.tools.is_dir():raise ValueError('existing fixed tools directory required')
    result=prepare(a) if a.phase=='prepare' else run(a);print(json.dumps(result),flush=True)
    return 0 if a.phase=='prepare' or result['execution_complete'] else 2


if __name__=='__main__':raise SystemExit(main())
