#!/usr/bin/env python3
"""Cached PG software-SELF windows, or explicit current ordinary /ok three; never a score."""
import argparse, copy, json, os, platform, re, shutil, signal
from pathlib import Path
import hosted_cycle as h
import focus_cycle as fc

FROZEN_BASE='80aaf7bccd4a436ebdf83e2283a83fcc6deec4fd'
FROZEN_DEFAULT='166453a1d2768bbce19796d9c7823be73cc84ccb'
INPUTS={'B':'59ede7cdba403d4d038c3720ab65f1cd940efd1038d07df782743a35eb9887ae',
        'BP':'d9b5ea46dd2b401ae368471a822efa8467efa64c50e4b5966681c0d2a21bc4a0'}


def frozen_plan(versions, *, callchains=False, current_product=None, only=None, workload='db'):
    if workload not in ('db','ok'):raise ValueError('SELF workload must be db or ok')
    if workload=='db' and only not in (None,'BP'):raise ValueError('only the explicit BP-only three-window PG mode is supported')
    if workload=='ok' and (only!='B' or current_product!=h.PG_CURRENT_BASELINE or not callchains):
        raise ValueError('/ok requires explicit current d05 B-only fp capture')
    plan=dict(schema='ku-typed-pg-minimal-SELF-v1',versions=versions,entry_sha256=h.sha(__file__),
        observer_helper_sha256=h.sha(h.__file__),resources=copy.deepcopy(h.RESOURCES),
        quality_policy=copy.deepcopy(h.POLICY),workload='db',contract='pg-strict',rate=800,
        warmup_seconds=3,score_seconds=15,order=[[n,t] for n in ('B','BP') for t in ('before','sample','after')],
        event='cpu-clock',perf_frequency=499,callchains=callchains,current_product=current_product,maximum_windows=6,maximum_replacements=0,
        maximum_control_cpu_ratio=1.10,observation_alarm_seconds=330,diagnostic_only=True,
        formal_qualified=False,normal_score_eligible=False,
        purpose='Current ordinary versus typed public PG path, same fixed work and mandatory reset. '
            'Fixed800rps improves sample opportunity, not guaranteed density; typed regions may remain unknown. '
            'Running SELF/PC and optional fp chains; missing or incomplete user frames remain unknown. No source-call-count timing, PMU/cycles, offCPU attribution, retries or score. '
            'Historical200rps measurements are not a before/after gain comparison.')
    if workload=='ok':
        plan.update(schema='ku-current-ok-minimal-SELF-v1',workload='ok',contract='text',rate=2000,
            only='B',order=[['B',turn] for turn in ('before','sample','after')],maximum_windows=3,
            purpose='Current d05 ordinary helpers/text/async/int /ok only: request materialization, string projection, '
                'Task driver and HTTP event running SELF/PC/fp chains. BP remains identity-checked, never run. '
                'Fixed2000rps warm3/score15 before/sample/after; no replacement, gain score, PMU or waiting attribution. '
                'Incomplete user frames, sparse samples and observer perturbation remain unknown.')
    elif only:
        plan.update(only=only,order=[[only,turn] for turn in ('before','sample','after')],maximum_windows=3,
            purpose=plan['purpose']+' Explicit BP-only capture; ordinary B is identity-checked and retained, never measured or compared in this sequence.')
    return plan


def identify(normal,typed, *, current_product=None, callchains=False, only=None, workload='db'):
    manifests={};versions={}
    for name,path in (('B',normal),('BP',typed)):
        if path.name!='manifest.json' or path.is_symlink():raise ValueError('plain manifest.json inputs required')
        m=h.load(path);manifests[name]=m;target=m['stacks']['ku'];build=target['build']
        declared=build['native']['server']['binary_sha256']
        if m['candidate_base'] not in ((current_product,) if current_product else (FROZEN_BASE,FROZEN_DEFAULT)) or not re.fullmatch('[0-9a-f]{64}',declared):raise ValueError('fixed archived source and declared ELF SHA required')
        protocol=Path(m['protocol'])
        versions[name]=dict(root=str(path.parent),stack='ku',source_commit=m['candidate_base'],binary_sha256=declared,
            artifacts={'protocol':dict(path=str(protocol),sha256=h.sha(protocol))})
    if versions['B']['source_commit']!=versions['BP']['source_commit']:raise ValueError('both PG arms must use the same fixed source')
    plan=frozen_plan(versions,callchains=callchains,current_product=current_product,only=only,workload=workload);bindings={n:fc.version_identity(plan,n) for n in versions}
    expected=dict(formatting_mode='helpers',json_response_mode='text',json_execution_mode='worker',
        pg_execution_mode='async',native_flags={'batch':['-std=c11','-O2'],'server':['-std=c11','-O2','-pthread']})
    for n,b in bindings.items():
        if b['build_mode']!={**expected,'pg_read_mode':'int' if n=='B' else 'typed'}:raise ValueError('normal O2/helpers/public async PG modes required')
        if b['files']['expanded source']['sha256']!=INPUTS[n]:raise ValueError('frozen PG business input changed')
    for key in ('Ku CLI','driver ELF','libpq'):
        if bindings['B']['files'][key]['sha256']!=bindings['BP']['files'][key]['sha256']:raise ValueError('same frozen '+key+' required')
    if not current_product and versions['B']['binary_sha256']!='234300bba9b6fce603a178701ac289a82108398d5f35de3c0a7ad585f082367b':raise ValueError('frozen default ordinary ELF234 required')
    if not current_product and versions['BP']['binary_sha256']!='faf34cec074f10b225ad880f149fb60f2fda9068fd192872ee00222344bd5b93':raise ValueError('frozen typed ELFfaf required')
    if current_product:
        for name,manifest in manifests.items():
            h.bl.require_rust_manifest(manifest)
            receipt=h.load(manifest['build_receipt'])
            cli_receipt=manifest['stacks']['ku']['build']['cli_build_receipt']
            if manifest['stacks']['ku']['build'].get('rust_toolchain')!='1.99.1':
                raise ValueError('new PG paths require actual Rust1.99.1 CLI provenance')
            if receipt.get('failure') is not None or not cli_receipt:raise ValueError('incomplete current build provenance')
    if workload=='ok':
        import reuse_build
        for name,expected_files in (('B',reuse_build.CURRENT_PG_FILES),('BP',reuse_build.CURRENT_TYPED_PG_FILES)):
            if {key:value.get('sha256') for key,value in bindings[name]['files'].items()}!=expected_files:
                raise ValueError('/ok requires the exact cached current CLI/ELF/Driver/libpq/input identity: '+name)
    if versions['B']['artifacts']['protocol']['sha256']!=versions['BP']['artifacts']['protocol']['sha256']:raise ValueError('actual protocol bytes differ')
    if manifests['B']['client']['command'][1:]!=manifests['BP']['client']['command'][1:]:raise ValueError('driver base arguments differ')
    for key in ('protocol_sha256','quality_policy','driver_cores','driver_cpuset','service_memory_bytes','service_env','uid','gid'):
        if manifests['B'].get(key)!=manifests['BP'].get(key):raise ValueError('manifest protocol/resources differ: '+key)
    return plan,bindings,manifests['B']


def drift(rows):
    costs={r['turn']:r.get('cpu_us') for r in rows};full={r['turn']:r.get('full_cpu_us') for r in rows}
    good=set(costs)=={'before','sample','after'} and all(type(x) in (int,float) and x>0 for x in costs.values())
    spread=max(costs['before'],costs['after'])/min(costs['before'],costs['after']) if good else None
    return dict(unsampled_cpu_max_min=spread,drift_valid=spread is not None and spread<=1.10,
        sampled_cpu_over_control_geomean=costs['sample']/(costs['before']*costs['after'])**.5 if good else None,
        full_lifecycle_cpu_us=full,cpu_contract='cpu_us is the original scored-client service CPU/correct; full_cpu_us remains separate',
        sample_ratio_is_observation_not_proven_overhead=True)


def exports(perf,output,observation,kallsyms=None,*,callchains=False):
    start,end=(observation.get(k) for k in ('client_started_monotonic_ns','client_finished_monotonic_ns'))
    if type(start)is not int or type(end)is not int or not 0<start<end:return False
    selected=f'{start//10**9}.{start%10**9:09d},{end//10**9}.{end%10**9:09d}'
    base=[perf,'-i',str(output/'perf.data'),'--symfs',str(output/'symfs'),'--time',selected]
    if kallsyms:base+=['--kallsyms',str(kallsyms)]
    report=h.diagnostic_command([base[0],'report',*base[1:],'--stdio','--no-children','--show-nr-samples',
        '--sort','symbol,dso,comm','--percent-limit','0'],output/'perf-self.log',timeout=20,limit=4<<20)
    pcs=h.diagnostic_command([base[0],'script',*base[1:],'--ns','--show-lost-events','--show-task-events',
        '-F','+pid,+tid,+time,+ip,+sym,+dso,+symoff,+srcline,+misc'],output/'perf-pcs.log',timeout=20,limit=8<<20)
    h.save(output/'export-receipt.json',dict(client_time_range_ns=[start,end],exports=[report,pcs],
        SELF_counted_once=True,inclusive_chains_unavailable=not callchains,missing_source_lines_remain_unknown=True,
        **({'fp_callchains_requested':True,'complete_user_chains_proven':False,'inclusive_chains_overlap_not_additive':True} if callchains else {})))
    return all(x['complete'] for x in (report,pcs))


def observe(a,plan,bindings,normal_manifest):
    result=dict(schema=plan['schema'],diagnostic_only=True,formal_qualified=False,normal_score_eligible=False,
        candidate_accepted=False,execution_complete=False,attribution_eligible=False,rows=[],controls={},stopped=[])
    directory=a.root/'normal-self-probe';directory.mkdir()
    fixture_root=a.root/'fixture';fixture_root.mkdir();manifest=copy.deepcopy(normal_manifest);manifest['stacks']={}
    h.save(fixture_root/'manifest.json',manifest);fixture=h.db.Database(fixture_root,manifest['uid'],manifest['gid'])
    original=fc.Harness;private=None;active=False;perf=shutil.which('perf',path=str(a.tools/'bin')+os.pathsep+os.environ.get('PATH',''))
    class SelfHarness(original):
        def __init__(self,*args,**kwargs):
            super().__init__(*args,**kwargs);self.manifest=copy.deepcopy(self.manifest)
            self.manifest['db_url']=private['db_url'];self.db_domain=h.ExternalDomain(private['db_cgroup'])
        def client(self,service,workload,rate,duration,output,**kwargs):
            parent=super().client
            if Path(output).name!='score':return parent(service,workload,rate,duration,output,**kwargs)
            return h.normal_self_client(lambda:parent(service,workload,rate,duration,output,**kwargs),
                service,output,duration,perf,sample=active,callchains=plan.get('callchains',False))
    kernel_symbols=None
    if plan.get('callchains'):
        with Path('/proc/kallsyms').open('rb') as stream:raw=stream.read((32<<20)+1)
        if len(raw)>32<<20:raise ValueError('bounded kernel symbol snapshot exceeded32MiB')
        kernel_symbols=a.root/'kernel.kallsyms';kernel_symbols.write_bytes(raw)
        with Path('/sys/kernel/notes').open('rb') as stream:notes=stream.read((1<<20)+1)
        if len(notes)>1<<20:raise ValueError('kernel notes exceed1MiB')
        (a.root/'kernel.notes').write_bytes(notes)
        h.save(a.root/'kernel-identity.json',dict(kernel=platform.release(),kallsyms_sha256=h.sha(kernel_symbols),notes_sha256=h.sha(a.root/'kernel.notes'),boot_sha256=h.sha('/proc/sys/kernel/random/boot_id'),boundary='Kernel fp chains use the active kernel unwinder. Missing, zero or truncated frames stay unknown; inclusive paths overlap and are not summed.'))
    try:
        fixture.start();private=h.load(fixture.manifest_path);fc.Harness=SelfHarness
        h.save(a.root/'identity.json',dict(bindings=bindings,private_database_limits=h.load(fixture.state_path)['limits'],
            kernel=platform.release(),machine=platform.machine(),boot_sha256=h.sha('/proc/sys/kernel/random/boot_id'),
            event='software cpu-clock499Hz; optional fp chains, no GPR/CPU field',source='Rust1.99.1 normal/typed build artifacts, verified before fresh observation' if plan.get('current_product') else 'archived fixed80/F166 arms; historical binaries only'))
        for name,turn in plan['order']:
            active=turn=='sample';output=directory/name/turn
            try:
                binding=fc.version_identity(plan,name)
                if binding!=bindings[name]:raise ValueError('cached identity changed during sequence')
                row=fc.window(plan,name,plan['workload'],plan['rate'],plan['warmup_seconds'],plan['score_seconds'],output,
                    binding=binding,job={'phase':'diagnostic','contract':plan['contract']})
                observation=h.load(output/'score/normal-self-observations.json')
                row.update(name=name,turn=turn,observation_complete=observation.get('record_complete') is True)
                if active:row['exports_complete']=observation.get('symbols',{}).get('complete') is True and exports(perf,output/'score',observation,kernel_symbols,
                    **({'callchains':True} if plan['workload']=='ok' else {}))
                report=h.load(output/'score/client.json');row['driver_send_delay']=report.get('ledger',{}).get('scheduled_to_attempt')
                row['correct_response_denominator']=report.get('ledger',{}).get('correct_total')
                quality,signature=h.normal_diagnostic_quality(row)
                valid=quality and row['observation_complete'] and (not active or row.get('exports_complete') is True)
            except Exception as error:
                row=h.load(output/'window.json') if (output/'window.json').is_file() else {}
                row.update(name=name,turn=turn,observer_error=repr(error));valid=False
            row.update(diagnostic_only=True,formal_qualified=False,normal_score_eligible=False,diagnostic_qualified=valid)
            result['rows'].append(row);h.save(a.root/'results.json',result)
            if not valid:result['stopped'].append(dict(name=name,turn=turn,reason='quality/observer/correctness/cleanup failure; no retry'));break
            if turn=='after':
                result['controls'][name]=control=drift([r for r in result['rows'] if r['name']==name])
                if not control['drift_valid']:
                    result['stopped'].append(dict(name=name,turn=turn,reason='unsampled drift>1.10 or unknown; no replacement'));break
    except BaseException as error:
        result['observer_error']=repr(error)
        if not isinstance(error,Exception):raise
    finally:
        fc.Harness=original
        try:cleanup=fixture.stop()
        except Exception as error:cleanup=dict(error=repr(error))
        result['database_cleanup']=cleanup
        dbok=cleanup.get('forced') is False and cleanup.get('cgroup_removed') is True and cleanup.get('after',{}).get('process_count')==0
        result['execution_complete']=len(result['rows'])==len(plan['order']) and not result['stopped'] and dbok and 'observer_error' not in result
        result['attribution_controls_passed']=result['execution_complete']
        result['boundary']='Running software SELF/PC exploration only; raw denominator/loss/sparsity/PC and sampling perturbation require review. Kernel fp callers may be available; incomplete user frames and waiting attribution remain unknown. No comparative score or automatic product change.'
        h.save(a.root/'summary.json',result)
        for name in ('B','BP'):
            raw=directory/name/'sample/score/perf.data'
            if raw.is_file() and not raw.is_symlink():raw.chmod(0o644)
    return result


def prepare_current(a):
    """Use the shared versioned builder/normalizer once; never import old builds or scores."""
    if os.geteuid()==0:raise ValueError('prepare fresh builds as the ordinary runner')
    h.require_normal_perf_preflight(a)
    Builder=h.shared_cli_builder(a.root)
    for name,mode in (('B',{}),('Driver',{}),('BP',{'ku_pg_read':'typed'})):
        directory=a.root/name;h.snapshot(a.source,a.current_product,directory/'src')
        (directory/'tools').symlink_to(a.tools,target_is_directory=True)
        builder=Builder(directory,directory/'src',uid=os.getuid(),gid=os.getgid(),candidate=a.current_product,**mode)
        try:
            if name=='B':builder.ku()
            elif name=='Driver':builder.go_build(client=True)
            else:builder.ku_native(Path(h.load(a.root/'B/manifest.json')['stacks']['ku']['build']['cli']))
            builder.finish()
        except BaseException as error:builder.finish(failed=repr(error));raise
    driver=h.load(a.root/'Driver/manifest.json');protocol=Path(driver['protocol'])
    for name in ('B','BP'):
        manifest=h.normalize_manifest(h.load(a.root/name/'manifest.json'),protocol,driver['client'])
        h.save(a.root/name/'manifest.json',manifest)
    plan,bindings,_=identify(a.root/'B/manifest.json',a.root/'BP/manifest.json',current_product=a.current_product,callchains=a.callchains)
    h.save(a.root/'prepared-current.json',dict(product=a.current_product,rust_toolchain='1.99.1',bindings=bindings,
        helper_sha256={name:h.sha(path)for name,path in [('entry',__file__),('builder',h.bl.__file__),('harness',h.__file__)]},
        new_builds=True,imported_measurement_windows=0,plan=plan))
    return 0


def main(argv=None):
    p=argparse.ArgumentParser(description=__doc__);p.add_argument('--root',required=True,type=Path)
    p.add_argument('--normal-manifest',type=Path);p.add_argument('--typed-manifest',type=Path)
    p.add_argument('--current-product');p.add_argument('--source',type=Path);p.add_argument('--prepare',action='store_true')
    p.add_argument('--preflight',action='store_true');p.add_argument('--preflight-root',type=Path);p.add_argument('--callchains',action='store_true')
    p.add_argument('--workload',choices=['db','ok'],default='db',help='PG by default; /ok requires exact current d05 B-only cached fp capture')
    p.add_argument('--only',choices=['B','BP'],help='BP-only PG, or B-only /ok; the other arm remains identity-checked and retained')
    p.add_argument('--tools',type=Path);a=p.parse_args(argv)
    a.root=a.root.resolve();a.tools=a.tools.resolve() if a.tools else None
    if a.current_product and not re.fullmatch('[0-9a-f]{40}',a.current_product):p.error('explicit full product SHA required')
    if a.workload=='ok' and (a.prepare or a.preflight or a.current_product!=h.PG_CURRENT_BASELINE or a.only!='B' or not a.callchains):
        p.error('/ok requires cached current d05, --only B and --callchains; no prepare/preflight workload switch')
    if a.workload=='db' and a.only=='B':p.error('B-only is reserved for explicit /ok; PG default and BP-only stay unchanged')
    a.candidate=a.current_product;a.perf_callchains=a.callchains
    if a.preflight:
        if not a.current_product or not a.tools:p.error('preflight requires current product/tools')
        a.root.mkdir(parents=True,exist_ok=True)
        return 0 if h.normal_perf_preflight(a).get('complete') is True else 2
    if a.prepare:
        if not a.source or not a.current_product or not a.tools:p.error('prepare requires current product/source/tools')
        return prepare_current(a)
    if not a.normal_manifest or not a.typed_manifest:p.error('two identified manifests required')
    if os.geteuid()!=0 or platform.system()!='Linux' or platform.machine() not in ('aarch64','arm64'):p.error('owned Linux ARM64/cgroup root required')
    a.normal_manifest=a.normal_manifest.resolve(strict=True);a.typed_manifest=a.typed_manifest.resolve(strict=True)
    plan,bindings,manifest=identify(a.normal_manifest,a.typed_manifest,current_product=a.current_product,callchains=a.callchains,only=a.only,workload=a.workload)
    a.root=a.root.resolve()
    if any(root==a.root or root in a.root.parents or a.root in root.parents for root in (a.normal_manifest.parent,a.typed_manifest.parent)):p.error('fresh output must be independent of cached inputs')
    if any(os.environ.get(k) for k in ('LD_PRELOAD','LD_AUDIT','LD_LIBRARY_PATH')):p.error('library overrides are not normal default measurements')
    a.root.mkdir(parents=True,exist_ok=False)
    a.tools=(a.tools or a.normal_manifest.parent/'tools').resolve();a.candidate=bindings['B']['source_commit']
    def stop(*unused):raise KeyboardInterrupt('typed PG SELF fixed coordinator bound')
    signal.signal(signal.SIGTERM,stop);signal.signal(signal.SIGALRM,stop);signal.alarm(330)
    h.save(a.root/'frozen-plan.json',plan)
    retained={}
    for name,binding in bindings.items():
        directory=a.root/'inputs'/name;directory.mkdir(parents=True);retained[name]=[]
        paths=[('manifest.json',Path(plan['versions'][name]['root'])/'manifest.json')]+[(label,binding['files'][key]['path']) for label,key in (('server.ku','expanded source'),('server.c','generated C'),('bin/ku-server','ELF'),('protocol.json','declared:protocol'))]
        for label,source in paths:
            source=Path(source);destination=directory/label;destination.parent.mkdir(parents=True,exist_ok=True);shutil.copyfile(source,destination)
            if h.sha(source)!=h.sha(destination):raise ValueError('retained input copy differs')
            retained[name].append(dict(path=str(destination.relative_to(a.root)),source=str(source),bytes=destination.stat().st_size,sha256=h.sha(destination)))
    h.save(a.root/'retained-inputs.json',retained)
    if a.preflight_root:
        shutil.copytree(a.preflight_root/'normal-perf-preflight',a.root/'normal-perf-preflight')
        preflight=h.load(a.root/'normal-perf-preflight/preflight.json')
        h.save(a.root/'preflight-reuse.json',dict(source_root=str(a.preflight_root),same_boot_and_mode_required=True,sha256=h.sha(a.root/'normal-perf-preflight/preflight.json')))
    else:preflight=h.normal_perf_preflight(a)
    if preflight.get('complete') is not True:
        h.save(a.root/'summary.json',dict(execution_complete=False,diagnostic_only=True,formal_qualified=False,
            attribution_eligible=False,stopped=['preflight failed; no services or windows started'],preflight=preflight));return 2
    h.require_normal_perf_preflight(a)
    result=observe(a,plan,bindings,manifest);print(json.dumps(result))
    return 0 if result['execution_complete'] else 2


if __name__=='__main__':raise SystemExit(main())
