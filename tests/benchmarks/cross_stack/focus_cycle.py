#!/usr/bin/env python3
"""Serial, resumable frozen-plan execution using the existing V5 Harness.

No build, installation, source repair or implicit service retry occurs here.
A new implementation/build needs a new frozen plan/output. Explore is allowed
without formal environmental qualification; that never turns it into a claim.
"""
import argparse
import hashlib
import json
import math
from pathlib import Path
import re
import signal
import time
from linux_harness import (Harness, full_lifecycle_cost, quiet_metrics, write_json,
                           qualification, upper_point, overload_boundary, paired_interval,
                           service_termination_succeeded, measurement_clock, result_assessment)

PHASES = ('control', 'screen', 'ablation', 'explore', 'final', 'capacity')


def digest(path):
    return hashlib.sha256(Path(path).read_bytes()).hexdigest()


def version_identity(plan, name):
    """Verify declared executable/source/dependency receipts before EVERY turn.

    Ku expanded source/C/CLI bind the actual generated program. Non-Ku source
    maps/lock files bind the reference. This does not pretend an undeclared
    system library was hashed: plans may add those exact files via artifacts.
    """
    spec = plan['versions'][name]
    root = Path(spec['root']).resolve()
    manifest_path = root/'manifest.json'
    manifest = json.loads(manifest_path.read_text())
    stack = spec.get('stack', 'ku')
    target = manifest['stacks'][stack]
    build = target.get('build', {})
    commit = spec.get('source_commit', manifest.get('candidate_base'))
    if not isinstance(commit, str) or not re.fullmatch('[0-9a-f]{40}', commit):
        raise ValueError('full source_commit required for '+name)
    files = {}

    def check(label, path, expected):
        path = Path(path).resolve(strict=True)
        if not isinstance(expected, str) or not re.fullmatch('[0-9a-f]{64}', expected):
            raise ValueError('missing frozen SHA256: '+label)
        actual = digest(path)
        if actual != expected:
            raise ValueError('artifact identity changed: '+label)
        files[label] = {'path': str(path), 'sha256': actual}

    binary = str(target['command'][0]).replace('{root}', str(root))
    check('ELF', binary, spec['binary_sha256'])
    client = manifest['client']
    check('driver ELF', str(client['command'][0]).replace('{root}',str(root)), client['sha256'])
    if stack == 'ku':
        native = build.get('native', {}).get('server', {})
        check('Ku CLI', build['cli'], build['cli_sha256'])
        check('expanded source', native['source'], native['source_sha256'])
        check('generated C', native['generated_c'], native['generated_c_sha256'])
        check('libpq', build['libpq_source'], build['libpq_sha256'])
    else:
        sources = build.get('source_files_sha256', {})
        if not sources:
            raise ValueError('reference source receipt required: '+name)
        directory = Path(manifest['source_root'])/'tests/benchmarks/cross_stack'/stack
        for path, expected in sources.items():
            check('source:'+path, directory/path, expected)
        if not any('lock' in p.lower() or p.endswith(('go.mod', 'go.sum')) for p in sources):
            raise ValueError('reference dependency lock receipt required: '+name)
    for label, artifact in spec.get('artifacts', {}).items():
        check('declared:'+label, artifact['path'], artifact['sha256'])
    for path, expected in spec.get('source_files', {}).items():
        check('frozen-source:'+path, Path(manifest['source_root'])/path, expected)
    return {'source_commit': commit, 'stack': stack, 'manifest_sha256': digest(manifest_path),
            'source_inventory_sha256': manifest.get('source_inventory_sha256'),
            'build_mode': {key: build[key] for key in ('native_flags', 'flags', 'formatting_mode',
                'json_response_mode', 'pg_execution_mode', 'pg_read_mode') if key in build},
            'files': files, 'dependency_scope': 'declared library/source-lock receipts; extra system libraries require plan artifacts'}


def bounded(value, low, high, label):
    if type(value) not in (int, float) or not math.isfinite(value) or not low <= value <= high:
        raise ValueError('out-of-range '+label)
    return value


def integer(value, low, high, label):
    if type(value) is not int:
        raise ValueError('integer required: '+label)
    return bounded(value,low,high,label)


def jobs_for(plan, phase, workload='json_object', names=('A', 'B', 'R')):
    """Expand one frozen schedule, with old control/screen/final flags intact."""
    jobs = []
    entries = plan.get('schedule')
    if entries is None:
        selected = ('control', 'screen', 'final') if phase == 'all' else (phase,)
        entries = []
        for current in selected:
            if current == 'capacity':
                continue
            if current == 'control':
                orders = [[names[0]]]*3
                config = plan.get('control', {'warmup_seconds': 5, 'score_seconds': 10})
            elif current in ('screen', 'ablation', 'explore'):
                orders = [list(names)]
                config = plan.get(current, plan.get('screen', {}))
            else:
                orders = []
                for order in plan['final_orders']:
                    if isinstance(order,str):
                        if len(names)==3:
                            orders.append([names['ABR'.index(key)] for key in order])
                        elif all(key in names for key in order):
                            orders.append(list(order))
                        else:
                            raise ValueError('non-ABR final names require explicit name arrays')
                    else:
                        orders.append(list(order))
                config = {'warmup_seconds': 10, 'score_seconds': 30}
            entries.append(dict(phase=current, workload=workload, orders=orders, **config))
    for entry_index, entry in enumerate(entries):
        current = entry['phase']
        if current not in PHASES or current == 'capacity':
            raise ValueError('fixed schedule phase must be control/screen/ablation/explore/final')
        if phase != 'all' and current != phase:
            continue
        kind = entry.get('workload', workload)
        rate = integer(entry['rate'] if 'rate' in entry else plan['rates'][kind], 1, 1_000_000, 'rate')
        warmup = bounded(entry.get('warmup_seconds', 10), 0, 60, 'warmup')
        duration = bounded(entry.get('score_seconds', 30), 1, 300, 'duration')
        if current == 'final' and (warmup < 10 or duration < 30):
            raise ValueError('final requires >=10s warmup and >=30s score; short windows are explore/smoke')
        orders = entry.get('orders', [entry.get('names', list(names))])
        for group, order in enumerate(orders, 1):
            if len(order) != len(set(order)):
                raise ValueError('duplicate name in one paired group')
            for name in order:
                if name not in plan['versions']:
                    raise ValueError('unknown version '+name)
                contract = entry.get('contract', plan['versions'][name].get('contract', 'pg-default' if kind == 'db' else 'dom'))
                job_id = '%s-%s-e%02d-g%02d-%s' % (current, kind, entry_index, group, name)
                if not re.fullmatch('[A-Za-z0-9_.-]{1,128}', job_id):
                    raise ValueError('unsafe window identity')
                jobs.append(dict(id=job_id, phase=current, name=name, workload=kind, rate=rate,
                                 warmup=warmup, duration=duration, group=group, entry=entry_index,
                                 contract=contract))
    return jobs


def window(plan, name, workload, rate, warmup, duration, output, *, job=None, binding=None, recovery=None):
    """One fresh service, original driver, bounded physical cleanup in finally."""
    spec = plan['versions'][name]
    output = Path(output)
    h = Harness(Path(spec['root']), output)
    stack = spec.get('stack', 'ku')
    binding = binding or version_identity(plan, name)
    if (h.manifest.get('driver_cores') != plan['resources']['driver_cores'] or
            h.manifest.get('driver_cpuset') != plan['resources']['driver_cpuset']):
        raise RuntimeError('driver resource contract differs from frozen plan')
    if h.manifest.get('quality_policy') != plan['quality_policy']:
        raise RuntimeError('arrival policy differs from frozen plan')
    row = {'name': name, 'workload': workload, 'rate': rate, 'started_unix_ns': time.time_ns(),
           'binary_sha256': spec['binary_sha256'], 'binding': binding,
           'source_commit': binding['source_commit'], 'build_mode': binding['build_mode'],
           'contract': (job or {}).get('contract', spec.get('contract', 'dom')),
           'functional_valid': None, 'accounting_valid': None, 'arrival_schedule_quality': 'unknown',
           'environment_quality': 'unknown', 'measurement_invalid': False, 'goal_status': 'unknown',
           'candidate_accepted': False, 'score_started': False, 'status': 'starting'}
    row.update(window_seconds=duration,warmup_seconds=warmup,
               formal_duration=(job or {}).get('phase')=='final' and duration>=30 and warmup>=10)
    s = None
    try:
        s = h.start(stack, cores=plan['resources']['service_cores'],
                    memory=plan['resources']['service_memory_bytes'],
                    output=output/'service', extra_env=spec.get('env', {}))
        s.startup['reuse'] = 'fresh serial instance; predecessor exited; never frozen'
        write_json(output/'service/service.json', s.startup)
        if warmup:
            warm = h.client(s, workload, rate, warmup, output/'warmup', runtime_metrics=False)
            row['warmup_assessment'] = warm['assessment']
            if not warm['assessment']['functional_valid'] or not warm['assessment']['accounting_valid']:
                row.update(status='warmup_failure', goal_status='not_met', **{
                    key: warm['assessment'][key] for key in ('functional_valid', 'accounting_valid',
                        'arrival_schedule_quality', 'environment_quality', 'measurement_invalid')})
                row['warmup_ledger'] = warm['ledger']
                return row
        row['score_started'] = True
        write_json(output/'progress.json', row)
        result = h.client(s, workload, rate, duration, output/'score', runtime_metrics=True)
        a = result['assessment']
        row.update({key: a[key] for key in ('functional_valid', 'accounting_valid',
                   'arrival_schedule_quality', 'environment_quality', 'measurement_invalid')})
        row['qualification'] = qualification(result, workload)
        row['upper_point'] = upper_point(rate, result, row['qualification'], output/'score/client.json')
        row['overload_boundary'] = overload_boundary(result, workload)
        row['results'] = result['ledger'].get('results', {})
        c = result.get('configuration',{})
        row['comparison_contract'] = {
            'protocol_sha256':h.identity['protocol_sha256'], 'driver_sha256':binding['files']['driver ELF']['sha256'],
            'corpora_sha256':c.get('corpora_sha256'), 'cores':plan['resources']['service_cores'],
            'memory':plan['resources']['service_memory_bytes'], 'warmup':warmup,'duration':duration,
            **{key:c.get(key) for key in ('workload','rate','concurrency','queue','timeout_seconds','keepalive_idle_ms','http')}}
        row['failed_request_outcomes'] = a.get('failed_request_outcomes', {})
        row['independent_collector_fault'] = bool(result.get('service_cost', {}).get('sampler_error')
                                                 or result.get('service_cost', {}).get('sampler_overflow'))
        if recovery:
            began = time.monotonic()
            anchor = measurement_clock(result.get('configuration',{}))
            recovery_cutoff_ns = anchor['origin_ns']+int(duration*1e9) if anchor['available'] else None
            recovered = h.client(s, workload, recovery['rate'], recovery['seconds'], output/'recovery', runtime_metrics=False)
            row['recovery'] = {'assessment': recovered['assessment'], 'ledger': recovered['ledger'],
                               'elapsed_client_seconds': time.monotonic()-began,
                               'origin':'original pressure planned-arrival cutoff, not driver return',
                               'origin_available':anchor['available']}
        deadline, quiet = time.monotonic()+5, False
        metrics = {}
        while time.monotonic() < deadline:
            metrics = s.metrics()
            quiet = quiet_metrics(stack, metrics)
            if quiet:
                break
            time.sleep(.1)
        physical = s.domain.snapshot()
        if recovery:
            row['recovery']['elapsed_until_quiet_seconds'] = ((time.monotonic_ns()-recovery_cutoff_ns
                +result['configuration']['origin_monotonic_uncertainty_ns'])/1e9 if recovery_cutoff_ns is not None else None)
        termination = s.stop()
        full = full_lifecycle_cost(result, physical, quiet, termination, metrics)
        write_json(output/'full-lifecycle.json', full)
        write_json(output/'termination.json', termination)
        natural = service_termination_succeeded(stack, termination)
        complete = quiet and natural
        row.update(status='functional_failure' if not a['functional_valid'] or not complete else
                   ('accounting_failure' if not a['accounting_valid'] else 'observed'),
                   correct=result['ledger']['correct_total'], planned=result['ledger']['planned'],
                   all_correct=result['ledger']['correct_total'] == result['ledger']['planned'],
                   cpu_us=result['service_cost']['cpu_seconds_per_correct']*1e6 if result['service_cost']['cpu_seconds_per_correct'] is not None else None,
                   full_cpu_us=full['cpu_seconds_per_correct']*1e6 if full['cpu_seconds_per_correct'] is not None else None,
                   memory=result['service_cost']['arrival_window_memory'], quiet=quiet, termination=termination,
                   send_delay=result['ledger']['scheduled_to_sent'], attempt_delay=result['ledger']['scheduled_to_attempt'],
                   latency=result['ledger']['correct_scheduled_to_complete'],
                   natural_reclamation=complete, score_report=str(output/'score/client.json'))
        if recovery:
            r = row['recovery'];l = r['ledger']
            r['passed'] = (r['assessment']['functional_valid'] and r['assessment']['accounting_valid']
                           and l['correct_total'] == l['planned'] and quiet
                           and r['elapsed_until_quiet_seconds'] is not None
                           and 0 <= r['elapsed_until_quiet_seconds'] <= recovery['maximum_seconds'])
        if row['status'] != 'observed':
            row['goal_status'] = 'not_met'
        if version_identity(plan, name) != binding:
            raise RuntimeError('artifact identity changed during window: '+name)
        return row
    except BaseException as error:
        row.update(error=repr(error), status='coordinator_interrupted' if isinstance(error, (KeyboardInterrupt, SystemExit)) else 'execution_error',
                   coordinator_interrupted=isinstance(error, (KeyboardInterrupt, SystemExit)), goal_status='blocked')
        # Preserve an already completed driver result even if tail/observer work
        # failed. A confirmed coordinator interruption never erases HTTP500.
        score = output/'score/client.json'
        if score.is_file():
            try:
                partial = json.loads(score.read_text())
                if 'ledger' in partial:
                    assessment = partial.get('assessment') or result_assessment(partial,plan['quality_policy'],
                        h.manifest.get('environment_qualification'))
                    row.update({key: assessment[key] for key in ('functional_valid', 'accounting_valid',
                        'arrival_schedule_quality', 'environment_quality', 'measurement_invalid')})
                    row['failed_request_outcomes'] = assessment.get('failed_request_outcomes', {})
            except (OSError, ValueError, KeyError):
                row['partial_driver_receipt'] = 'unreadable; original file retained'
        if s is not None:
            row['service_exited_before_cleanup'] = s.process.poll() is not None
        row['service_started'] = bool(h.services)
        row['not_executed'] = not h.services
        raise
    finally:
        row['cleanup'] = h.close()
        row['ended_unix_ns'] = time.time_ns()
        write_json(output/'window.json', row)


def replacement_allowed(attempt, evidence, maximum, used):
    """Bad scores/lateness are never replacement evidence, even with resume."""
    if used >= maximum:
        return False
    row = attempt.get('row', {})
    if row.get('functional_valid') is False or row.get('failed_request_outcomes') or row.get('service_exited_before_cleanup'):
        return False
    if row.get('termination',{}).get('forced') or any(x.get('forced_owned_cleanup') for x in row.get('cleanup',[])):
        return False
    if row.get('not_executed') is True:
        return True
    if row.get('independent_collector_fault') or row.get('coordinator_interrupted'):
        return True
    if not evidence or evidence.get('measurement_fault_confirmed') is not True:
        return False
    key = 'result_sha256' if attempt.get('result_sha256') else 'attempt_sha256'
    expected = attempt.get(key)
    if not isinstance(expected,str) or not re.fullmatch('[0-9a-f]{64}',expected) or evidence.get(key) != expected:
        return False
    if evidence.get('cause') not in ('driver_failure', 'collector_failure', 'external_host_failure', 'coordinator_interrupted'):
        return False
    files = evidence.get('evidence_files', [])
    return bool(files) and all(Path(x['path']).is_file() and digest(x['path']) == x['sha256'] for x in files)


def run_one(plan, job, output, state, identity, *, resume=False, evidence=None):
    """The state ledger owns unique attempts; never ask Harness to overwrite."""
    directory = Path(output)/job['id']
    attempts = state['windows'].setdefault(job['id'], [])
    binding = version_identity(plan, job['name'])
    if attempts:
        previous = attempts[-1]
        if previous.get('binding') != binding:
            write_json(Path(output)/('invalidation-'+str(time.time_ns())+'.json'),
                       {'window_id': job['id'], 'reason': 'artifact binding changed', 'old': previous.get('binding'), 'new': binding})
            raise ValueError('artifact drift; use new frozen plan/output')
        if previous.get('result_path') and digest(previous['result_path']) != previous['result_sha256']:
            raise ValueError('retained result changed')
        if previous['state'] == 'completed' and (not previous['row'].get('measurement_invalid')
                or previous['row'].get('functional_valid') is False):
            return previous['row']
        maximum = integer(plan.get('resume', {}).get('maximum_replacements', 2),0,10,'replacement maximum')
        used = sum(max(0, len(v)-1) for v in state['windows'].values())
        if not resume or not replacement_allowed(previous, evidence, maximum, used):
            if previous['state']=='completed':
                return dict(previous['row'],resume_reason='retained invalid window; no approved replacement')
            return dict(previous.get('row', {}), id=job['id'], name=job['name'], workload=job['workload'],
                        phase=job['phase'],group=job['group'],entry=job['entry'],contract=job['contract'],
                        status='blocked_resume', goal_status='blocked',
                        resume_reason='executed or cause unknown; no authorized independent-fault replacement')
    attempt_dir = directory/('attempt-%02d' % len(attempts))
    attempt_dir.mkdir(parents=True, exist_ok=False)
    record = {'state': 'running', 'binding': binding, 'directory': str(attempt_dir)}
    record['attempt_sha256'] = hashlib.sha256(json.dumps({'job':job,'binding':binding,
        'directory':str(attempt_dir),'plan_sha256':identity['plan_sha256']},sort_keys=True).encode()).hexdigest()
    attempts.append(record)
    write_json(Path(output)/'state.json', state)
    try:
        row = window(plan, job['name'], job['workload'], job['rate'], job['warmup'], job['duration'],
                     attempt_dir, job=job, binding=binding, recovery=job.get('recovery'))
        record['state'] = 'completed'
    except BaseException as error:
        path = attempt_dir/'window.json'
        row = json.loads(path.read_text()) if path.is_file() else {
            'status': 'execution_error_before_measurement', 'error': repr(error),
            'goal_status': 'blocked', 'functional_valid': None, 'accounting_valid': None,
            'arrival_schedule_quality': 'unknown', 'environment_quality': 'unknown',
            'measurement_invalid': False, 'candidate_accepted': False,
            'not_executed':not (attempt_dir/'service').exists()}
        record['state'] = 'interrupted' if row.get('coordinator_interrupted') else 'failed'
        if isinstance(error, (KeyboardInterrupt, SystemExit)):
            record['row'] = row
            raise
    finally:
        if 'row' in locals():
            row.update({key: job[key] for key in ('id', 'phase', 'group', 'entry', 'contract')})
            row.update(plan_sha256=identity['plan_sha256'], source_commit=binding['source_commit'], build_mode=binding['build_mode'])
            result = attempt_dir/'result.json'
            write_json(result, row)
            record.update(row=row, result_path=str(result), result_sha256=digest(result))
            write_json(Path(output)/'state.json', state)
    return row


def capacity_jobs(plan, output, state, identity, *, resume=False, evidence=None):
    config = plan.get('capacity')
    if not config or config.get('final_frozen') is not True:
        raise ValueError('capacity requires predeclared final_frozen configuration')
    maximum = integer(config['maximum_points'], 1, 12, 'capacity points')
    refinements = integer(config.get('refinements', min(2,maximum-1)), 0, maximum-1, 'capacity refinements')
    factor = bounded(config.get('factor', 2), 1.1, 4, 'capacity factor')
    results = []
    for kind in config['workloads']:
        for name in config.get('names', ['B', 'R']):
            contract=config.get('contracts', {}).get(kind,plan['versions'][name].get('contract','pg-default' if kind=='db' else 'dom'))
            known=state.get('blocked_candidates',{}).get(json.dumps([name,kind,contract]))
            if known:
                results.append({'name':name,'workload':kind,'contract':contract,'point_ids':[],
                    'goal_status':'blocked','candidate_accepted':False,'known_failure':known,
                    'claim':'unchanged candidate already failed; no capacity replay'})
                write_json(Path(output)/'capacity.json',{'results':results})
                continue
            start = integer(config['start_rates'][kind] if kind in config.get('start_rates',{}) else plan['rates'][kind],1,1_000_000,'capacity start')
            ceiling = integer(config['maximum_rate'], start, 1_000_000, 'capacity ceiling')
            low, high, upper, rows, boundary = 0, None, None, [], False
            rate, refined = start, 0
            for point in range(maximum):
                job = dict(id='capacity-%s-%s-p%02d-r%d' % (kind,name,point,rate), phase='capacity',
                    name=name, workload=kind, rate=rate, group=point+1, entry='capacity-'+kind,
                    contract=contract,
                    warmup=bounded(config.get('warmup_seconds',10),0,60,'capacity warmup'),
                    duration=bounded(config.get('score_seconds',30),1,300,'capacity duration'),
                    recovery={'rate':integer(config.get('recovery_rate',max(1,start//5)),1,start,'recovery rate'),
                              'seconds':bounded(config.get('recovery_seconds',2),1,5,'recovery seconds'),
                              'maximum_seconds':bounded(config.get('recovery_maximum_seconds',5),1,10,'recovery bound')})
                row = run_one(plan, job, output, state, identity, resume=resume, evidence=(evidence or {}).get(job['id']))
                rows.append(row)
                ok = (row.get('status') == 'observed' and row.get('qualification', {}).get('qualified') is True
                      and row.get('recovery', {}).get('passed') is True)
                if ok:
                    low = rate
                    if high is None:
                        if rate >= ceiling:
                            break
                        rate = min(ceiling, max(rate+1, math.ceil(rate*factor)))
                        continue
                else:
                    high = rate
                    upper = row.get('upper_point')
                    boundary = row.get('overload_boundary',{}).get('boundary_observed') is True
                    # Errors or unknown quality are retained and stop this stack;
                    # they cannot manufacture an observable capacity boundary.
                    if (row.get('functional_valid') is not True or row.get('accounting_valid') is not True
                            or row.get('measurement_invalid') or not row.get('overload_boundary', {}).get('boundary_observed')
                            or row.get('recovery', {}).get('passed') is not True):
                        break
                if not low or high is None or high <= low+1 or refined >= refinements:
                    break
                rate = (low+high)//2;refined += 1
            summary = {'name':name,'workload':kind,'qualified_lower_rate':low or None,'upper_rate':high,
                'upper_point':upper,'point_ids':[x['id'] for x in rows],
                'boundary_observed':boundary,
                'functional_valid':all(x.get('functional_valid') is True for x in rows),
                'accounting_valid':all(x.get('accounting_valid') is True for x in rows),
                'all_recovery_passed':all(x.get('recovery',{}).get('passed') is True for x in rows),
                'claim':'observed bracket' if low and high and upper and boundary and not upper.get('censored',True) else 'lower bound or unmeasured; maximum not established',
                'goal_status':'not_met' if any(x.get('functional_valid') is False for x in rows) else 'unknown','candidate_accepted':False}
            results.append(summary)
            write_json(Path(output)/'capacity.json', {'results':results})
    return results


def metric(row, path):
    value = row
    for part in path.split('.'):
        if not isinstance(value, dict) or part not in value:
            return None
        value = value[part]
    return value if type(value) in (int,float) and math.isfinite(value) and value>0 else None


def compare_goals(plan, rows):
    results = []
    for target in plan.get('targets', []):
        selected = [r for r in rows if all(r.get(k) == target[k] for k in ('phase','workload','contract'))]
        pairs = {}
        for row in selected:
            key = (row.get('entry'),row['group'],row.get('rate'))
            if row['name'] in (target['numerator'],target['denominator']):
                pair = pairs.setdefault(key,{})
                if row['name'] in pair:
                    raise ValueError('duplicate window in one paired group')
                pair[row['name']] = row
        complete = [p for p in pairs.values() if set(p) == {target['numerator'],target['denominator']}]
        a = [metric(p[target['numerator']],target.get('metric','cpu_us')) for p in complete]
        b = [metric(p[target['denominator']],target.get('metric','cpu_us')) for p in complete]
        interval = paired_interval(a,b,minimum=integer(target.get('minimum_pairs',5),5,100,'paired windows'),
                                   resamples=integer(plan.get('bootstrap_resamples',20000),100,100000,'bootstrap resamples'))
        qualified = target['phase']=='final' and bool(complete) and len(complete)==len(pairs) and all(
            r.get('qualification',{}).get('qualified') is True and r.get('natural_reclamation') is True
            and r.get('formal_duration') is True
            for p in complete for r in p.values())
        qualified = qualified and all(p[target['numerator']].get('comparison_contract') is not None
            and p[target['numerator']]['comparison_contract']==p[target['denominator']].get('comparison_contract') for p in complete)
        failed = any(r.get('functional_valid') is False or r.get('accounting_valid') is False for r in selected)
        limit = bounded(target.get('maximum_ratio',1),.01,100,'goal ratio')
        status = ('not_met' if failed else 'blocked' if any(r.get('goal_status')=='blocked' for r in selected) else 'unknown')
        if not failed and qualified and interval['status']=='estimated':
            status = 'met' if interval['upper95'] <= limit else 'not_met' if interval['lower95']>limit else 'unknown'
        results.append({'target':target,'paired_windows':len(complete),'interval':interval,
                        'formal_qualified':qualified,'goal_status':status,'candidate_accepted':False,
                        'adoption_basis':'measurement goal only; product promotion is a separate decision'})
    return results


def execute(plan_path, output, phase, workload='json_object', names=('A','B','R'), resume=False, evidence=None):
    plan_path, output = Path(plan_path), Path(output)
    plan = json.loads(plan_path.read_text())
    output.mkdir(parents=True, exist_ok=True)
    identity = {'plan_sha256':digest(plan_path),'entry_sha256':digest(__file__),
                'harness_sha256':digest(Path(__file__).with_name('linux_harness.py')),
                'phase':phase,'workload':workload,'names':list(names)}
    identity_path = output/'identity.json'
    if identity_path.exists():
        previous = json.loads(identity_path.read_text())
        if not resume or previous != identity:
            if previous != identity:
                write_json(output/('invalidation-'+str(time.time_ns())+'.json'),
                           {'reason':'plan/runner selection changed; old attempts retained, new output required','old':previous,'requested':identity})
            raise ValueError('existing frozen output requires matching --resume; changed identity needs new output')
        state = json.loads((output/'state.json').read_text())
    else:
        write_json(identity_path, identity)
        state = {'identity':identity,'windows':{}}
        write_json(output/'state.json', state)
    jobs = jobs_for(plan, phase, workload, names)
    rows = []
    for job in jobs:
        if (digest(plan_path)!=identity['plan_sha256'] or digest(__file__)!=identity['entry_sha256']
                or digest(Path(__file__).with_name('linux_harness.py'))!=identity['harness_sha256']):
            note={'reason':'plan or executing runner source changed; new output required','identity':identity,
                  'rows':rows,'execution_complete':False,'goal_status':'blocked','candidate_accepted':False}
            write_json(output/('invalidation-'+str(time.time_ns())+'.json'),note)
            write_json(output/'summary.json',note)
            raise ValueError('plan/runner changed during execution')
        failure_key=json.dumps([job['name'],job['workload'],job['contract']])
        previous_failure=state.get('blocked_candidates',{}).get(failure_key)
        try:
            if previous_failure and previous_failure['window_id']!=job['id']:
                row=dict(job,status='blocked_candidate_failure',goal_status='blocked',functional_valid=None,
                         accounting_valid=None,arrival_schedule_quality='unknown',environment_quality='unknown',
                         measurement_invalid=False,candidate_accepted=False,known_failure=previous_failure)
            else:
                row = run_one(plan,job,output,state,identity,resume=resume,evidence=(evidence or {}).get(job['id']))
        except (ValueError,KeyError,OSError) as error:
            # No score is fabricated when a source/build receipt is unavailable.
            row = dict(job,status='identity_or_plan_blocked',error=repr(error),goal_status='blocked',
                       functional_valid=None,accounting_valid=None,arrival_schedule_quality='unknown',
                       environment_quality='unknown',measurement_invalid=False,candidate_accepted=False)
            write_json(output/('blocked-'+job['id']+'-'+str(time.time_ns())+'.json'),row)
        if row.get('functional_valid') is False:
            state.setdefault('blocked_candidates',{}).setdefault(failure_key,
                {'window_id':job['id'],'reason':'confirmed functional failure in unchanged candidate; independent windows may continue'})
            write_json(output/'state.json',state)
        rows.append(row)
        write_json(output/'summary.json',{'identity':identity,'rows':rows,'goals':compare_goals(plan,rows)})
        print(json.dumps({k:row[k] for k in ('id','status','correct','cpu_us','goal_status') if k in row}),flush=True)
        # Legacy single-phase invocations keep their fail-fast behavior. The
        # explicit multi-phase schedule may finish independent named windows.
        if phase!='all' and row.get('status')!='observed':
            break
    capacity = capacity_jobs(plan,output,state,identity,resume=resume,evidence=evidence) if phase=='capacity' or phase=='all' and plan.get('capacity') else []
    goals = compare_goals(plan, rows)
    observed = bool(rows) and all(r.get('status')=='observed' for r in rows)
    formal = [r for r in rows if r.get('phase')=='final']
    expected_formal = [j for j in jobs if j['phase']=='final']
    groups = {}
    for r in formal:
        groups.setdefault((r['entry'],r['workload'],r['contract']),set()).add(r['group'])
    design_complete = len(formal)==len(expected_formal) and bool(groups) and all(len(v)>=5 for v in groups.values())
    qualified = design_complete and all(r.get('qualification',{}).get('qualified') is True
        and r.get('formal_duration') is True and r.get('natural_reclamation') is True for r in formal)
    controls = [r for r in rows if r.get('phase')=='control']
    costs = [r['cpu_us'] for r in controls if metric(r,'cpu_us')]
    spread = max(costs)/min(costs) if costs else None
    control_qualified = (bool(controls) and all(r.get('status')=='observed' and
        r.get('qualification',{}).get('qualified') is True for r in controls)
        and spread is not None and spread <= plan.get('control_cpu_max_min',1.10))
    result = {'identity':identity,'rows':rows,'capacity':capacity,'goals':goals,
              'dispatch_complete':len(rows)==len(jobs),
              'execution_complete':len(rows)==len(jobs) and all(not r.get('status','').startswith(('blocked_','identity_'))
                  and not (r.get('score_started') is False and r.get('status')!='observed') for r in rows),
              'formal_qualified':qualified,
              'formal_design_complete':design_complete,
              'exploration_allowed':all(r.get('functional_valid') is not False and r.get('accounting_valid') is not False for r in rows),
              'control_CPU_max_min':spread,'control_qualified':control_qualified,
              'goal_status':('not_met' if any(g['goal_status']=='not_met' for g in goals)
                                        or any(r.get('functional_valid') is False or r.get('accounting_valid') is False for r in rows) else
                             'blocked' if any(g['goal_status']=='blocked' for g in goals)
                                          or any(r.get('goal_status')=='blocked' for r in rows) else
                             'met' if goals and all(g['goal_status']=='met' for g in goals) else 'unknown'),
              'candidate_accepted':False,'replacement_count':sum(max(0,len(v)-1) for v in state['windows'].values()),
              'scope':'runner completion, formal qualification and goal/adoption are independent; per-request counts are never bootstrap N'}
    write_json(output/'summary.json',result)
    write_json(output/'qualification.json',dict(result,rows=None))
    return result


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument('--plan', required=True, type=Path)
    p.add_argument('--out', required=True, type=Path)
    p.add_argument('--phase', choices=(*PHASES,'all'), required=True)
    p.add_argument('--workload', default='json_object')
    p.add_argument('--names', default='A,B,R')
    p.add_argument('--resume', action='store_true')
    p.add_argument('--invalid-evidence', action='append', type=Path, default=[],
                   help='independent-fault receipt with window_id, result_sha256, cause and hashed evidence_files')
    args = p.parse_args()
    evidence = {}
    for path in args.invalid_evidence:
        value = json.loads(path.read_text())
        evidence[value['window_id']] = value
    def interrupted(signum,frame):
        raise KeyboardInterrupt('coordinator received signal '+str(signum))
    signal.signal(signal.SIGTERM, interrupted)
    result = execute(args.plan,args.out,args.phase,args.workload,tuple(args.names.split(',')),args.resume,evidence)
    if not result['execution_complete'] or not all(r.get('status')=='observed' for r in result['rows']):
        return 2
    if args.phase=='control' and not result['control_qualified'] or args.phase=='final' and not result['formal_qualified']:
        return 3
    return 0


if __name__ == '__main__':
    raise SystemExit(main())
