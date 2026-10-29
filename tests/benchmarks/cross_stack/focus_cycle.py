#!/usr/bin/env python3
"""Small reproducible control/screen/final entry; uses the existing V5 driver.

Plans contain private fixture roots locally. Keep credentials in manifests, not
in plans/reports. No build, installation, automatic retries or environment
changes occur here. Failed outcomes remain even if quality also fails.
"""
import argparse
import hashlib
import json
from pathlib import Path
import time
from linux_harness import Harness, full_lifecycle_cost, quiet_metrics, write_json


def digest(path):
    return hashlib.sha256(Path(path).read_bytes()).hexdigest()


def window(plan, name, workload, rate, warmup, duration, output):
    spec = plan['versions'][name]
    root = Path(spec['root'])
    h = Harness(root, output)
    stack = spec.get('stack', 'ku')
    binary = Path(h.manifest['stacks'][stack]['command'][0])
    if digest(binary) != spec['binary_sha256']:
        raise RuntimeError('binary changed before window: ' + name)
    if (h.manifest.get('driver_cores') != plan['resources']['driver_cores'] or
            h.manifest.get('driver_cpuset') != plan['resources']['driver_cpuset']):
        raise RuntimeError('driver resource contract differs from frozen plan')
    if h.manifest.get('quality_policy') != plan['quality_policy']:
        raise RuntimeError('arrival policy differs from frozen plan')
    row = {'name': name, 'workload': workload, 'rate': rate,
           'started_unix_ns': time.time_ns(), 'binary_sha256': spec['binary_sha256']}
    s = None
    try:
        s = h.start(stack, cores=plan['resources']['service_cores'],
                    memory=plan['resources']['service_memory_bytes'],
                    output=output/'service', extra_env=spec.get('env', {}))
        s.startup['reuse'] = 'fresh serial instance; predecessor exited; never frozen'
        write_json(output/'service/service.json', s.startup)
        warm = h.client(s, workload, rate, warmup, output/'warmup', runtime_metrics=False)
        row['warmup_assessment'] = warm['assessment']
        if not warm['assessment']['functional_valid'] or not warm['assessment']['accounting_valid']:
            row.update(status='warmup_failure', warmup_ledger=warm['ledger'])
            return row
        result = h.client(s, workload, rate, duration, output/'score', runtime_metrics=True)
        deadline, quiet = time.monotonic()+5, False
        metrics = {}
        while time.monotonic() < deadline:
            metrics = s.metrics()
            quiet = quiet_metrics(stack, metrics)
            if quiet:
                break
            time.sleep(.1)
        physical = s.domain.snapshot()
        termination = s.stop()
        full = full_lifecycle_cost(result, physical, quiet, termination, metrics)
        write_json(output/'full-lifecycle.json', full)
        write_json(output/'termination.json', termination)
        a = result['assessment']
        complete = (result['ledger']['correct_total'] == rate*duration and quiet and
                    not termination.get('forced') and termination.get('cgroup_removed'))
        row.update(status='functional_failure' if not a['functional_valid'] or not complete else
                   ('accounting_failure' if not a['accounting_valid'] else 'observed'),
                   assessment=a, correct=result['ledger']['correct_total'], planned=result['ledger']['planned'],
                   cpu_us=result['service_cost']['cpu_seconds_per_correct']*1e6,
                   full_cpu_us=full['cpu_seconds_per_correct']*1e6,
                   memory=result['service_cost']['arrival_window_memory'],
                   quiet=quiet, termination=termination,
                   send_delay=result['ledger']['scheduled_to_sent'],
                   attempt_delay=result['ledger']['scheduled_to_attempt'],
                   latency=result['ledger']['correct_scheduled_to_complete'])
        if digest(binary) != spec['binary_sha256']:
            raise RuntimeError('binary changed during window: ' + name)
        return row
    finally:
        write_json(output/'cleanup.json', h.close())


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--plan', required=True, type=Path)
    parser.add_argument('--out', required=True, type=Path)
    parser.add_argument('--phase', choices=('control', 'screen', 'final'), required=True)
    parser.add_argument('--workload', choices=('json_object', 'json_array', 'json_escape', 'db', 'ok', 'cpu'), default='json_object')
    parser.add_argument('--names', default='A,B,R', help='names in frozen plan, comma separated')
    args = parser.parse_args()
    plan = json.loads(args.plan.read_text())
    args.out.mkdir(parents=True, exist_ok=True)
    identity = {'plan_sha256': digest(args.plan), 'entry_sha256': digest(__file__), 'phase': args.phase,
                'workload': args.workload, 'names': args.names}
    if (args.out/'identity.json').exists():
        raise RuntimeError('output exists; do not overwrite or silently repeat a prior experiment')
    write_json(args.out/'identity.json', identity)
    rate = plan['rates'][args.workload]
    names = args.names.split(',')
    if args.phase == 'control':
        orders = [[names[0]]]*3
        control = plan.get('control', {'warmup_seconds': 5, 'score_seconds': 10})
        warmup, duration = control['warmup_seconds'], control['score_seconds']
    elif args.phase == 'screen':
        orders = [names]
        warmup, duration = plan['screen']['warmup_seconds'], plan['screen']['score_seconds']
    else:
        orders = [[names['ABR'.index(key)] for key in order] for order in plan['final_orders']]
        warmup, duration = 10, 30
    if not 0 <= warmup <= 60 or not 1 <= duration <= 300:
        raise ValueError('warmup0..60s and score1..300s required')
    rows = []
    for group, order in enumerate(orders, 1):
        for name in order:
            here = args.out/('g%d-%s' % (group, name))
            try:
                row = window(plan, name, args.workload, rate, warmup, duration, here)
            except Exception as error:
                write_json(here/'exception.json', {'error': repr(error)})
                raise
            row['group'] = group
            rows.append(row)
            write_json(args.out/'summary.json', {'identity': identity, 'rows': rows})
            print(json.dumps({k: row[k] for k in ('group', 'name', 'status', 'correct', 'cpu_us') if k in row}), flush=True)
            # All axes remain in the receipt; neither late arrivals nor unknown
            # environmental cause erase functional errors or create retries.
            if row['status'] != 'observed':
                return 2
    qualified = all(r['assessment']['functional_valid'] and r['assessment']['accounting_valid'] and
                    r['assessment']['arrival_schedule_quality']=='pass' and r['quiet'] for r in rows)
    spread = max(r['cpu_us'] for r in rows)/min(r['cpu_us'] for r in rows) if rows else None
    if args.phase == 'control':
        qualified = qualified and spread <= plan['control_cpu_max_min']
    else:
        qualified = qualified and all(r['assessment']['environment_quality']=='pass' for r in rows)
    write_json(args.out/'qualification.json', {'quality': 'pass' if qualified else 'fail',
               'CPU_max_min': spread, 'functional_failures_preserved': True,
               'measurement_fault_confirmed': False, 'candidate_accepted': False,
               'scope': 'control conformance or window eligibility; not a gain/causality decision',
               'replacement_count': 0, 'evidence': str(args.out/'summary.json')})
    return 0 if qualified else 3


if __name__ == '__main__':
    raise SystemExit(main())
