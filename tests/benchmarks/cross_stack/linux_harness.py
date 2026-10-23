#!/usr/bin/env python3
"""Permanent Linux cgroup-v2 comparison runner; no production source changes.

The root coordinator stays outside measured domains. Children enter a new
whole-service cgroup before dropping privileges and exec, so cluster children,
GC and background threads inherit the same CPU/memory accounting and limits.
"""
from __future__ import annotations
import argparse
import dataclasses
import csv
import gzip
import hashlib
import http.client
import json
import math
import os
import resource
import random
from pathlib import Path
import secrets
import signal
import shutil
import socket
import statistics
import subprocess
import sys
import threading
import time
import urllib.parse

STACKS = ('ku', 'go', 'rust', 'node')
GIB = 1 << 30
MIB = 1 << 20

def write_json(path, value):
    path = Path(path)
    path.parent.mkdir(parents=True, exist_ok=True)
    temporary = path.with_name(path.name + '.writing')
    temporary.write_text(json.dumps(value, ensure_ascii=False, indent=2) + '\n')
    os.replace(temporary, path)

def read_json(path):
    return json.loads(Path(path).read_text())

def counters(path):
    result = {}
    try:
        for line in Path(path).read_text().splitlines():
            key, value = line.split(None, 1)
            result[key] = int(value)
    except (OSError, ValueError):
        pass
    return result

def free_port():
    with socket.socket() as sock:
        sock.bind(('127.0.0.1', 0))
        return sock.getsockname()[1]

def request(address, path, method='GET', headers=None, timeout=2):
    host, port = address.rsplit(':', 1)
    connection = http.client.HTTPConnection(host, int(port), timeout=timeout)
    try:
        connection.request(method, path, headers=headers or {})
        response = connection.getresponse()
        body = response.read(1048577)
        if len(body) > 1048576:
            raise RuntimeError('management response exceeds 1MiB')
        return response.status, body
    finally:
        connection.close()

def template(value, replacements):
    for key, replacement in replacements.items():
        value = value.replace('{' + key + '}', str(replacement))
    return value

def arrival_cpu(samples, label, start, end):
    rows = sorted((d['monotonic_ns'], d['cpu_stat'].get('usage_usec', 0))
                  for sample in samples for d in sample['domains'] if d['label'] == label)
    def at(target):
        for left, right in zip(rows, rows[1:]):
            if left[0] <= target <= right[0] and right[0] > left[0]:
                weight = (target-left[0])/(right[0]-left[0])
                return left[1] + weight*(right[1]-left[1]), abs(right[1]-left[1])
        return None
    first, last = at(start), at(end)
    if first is None or last is None:
        return {'available':False, 'reason':'arrival endpoints not bracketed by resource samples'}
    return {'available':True,'cpu_seconds':max(0,last[0]-first[0])/1e6,
            'conservative_endpoint_uncertainty_seconds':(first[1]+last[1])/1e6,
            'method':'linear interpolation of 1Hz cumulative cgroup CPU on Linux CLOCK_MONOTONIC; uncertainty is full CPU in endpoint brackets'}

def measurement_clock(configuration):
    origin=configuration.get('origin_monotonic_ns')
    uncertainty=configuration.get('origin_monotonic_uncertainty_ns')
    if (configuration.get('origin_monotonic_status')!='available' or type(origin) is not int or origin<0
            or type(uncertainty) is not int or uncertainty<0):
        return {'available':False,'reason':'measurement_monotonic_clock_unavailable'}
    if uncertainty>1_000_000:
        return {'available':False,'reason':'measurement_monotonic_clock_uncertain',
                'uncertainty_ns':uncertainty,'limit_ns':1_000_000}
    return {'available':True,'origin_ns':origin,'uncertainty_ns':uncertainty,
            'clock':'Linux CLOCK_MONOTONIC; Unix timestamps are display only'}

def quiet_metrics(stack, value):
    def zero(mapping, fields):
        return isinstance(mapping,dict) and all(type(mapping.get(k)) is int and mapping[k]==0 for k in fields)
    if not isinstance(value,dict):
        return False
    if stack != 'ku':
        if not zero(value,('active','waiting_timer','waiting_tcp','actual_cpu_jobs')):
            return False
        return all(type(value[k]) is int and value[k]==0 for k in ('waiting_cpu','waiting_db','database_pending') if k in value)
    for key, fields in (('loops',('queued','running','resident','pending','connections')),
                        ('tasks',('ready','waiting','running','resident','timers'))):
        rows=value.get(key)
        # This fixture always has at least one event owner and async task owner;
        # an empty/missing list is unknown, never proof of zero retained work.
        if not isinstance(rows,list) or not rows or not all(zero(row,fields) for row in rows):
            return False
    if not zero(value.get('task_blocking'),('resident','queued','running','net_wait','net_step','cancelled_still_running','output_charge_bytes')):
        return False
    if not zero(value.get('database'),('pending_operations','pending_bytes')):
        return False
    if not zero(value,('buffer_bytes',)):
        return False
    if 'output' in value and not zero(value['output'],('queued_records','queued_bytes')):
        return False
    return True

def guest_memory():
    result={}
    for line in Path('/proc/meminfo').read_text().splitlines():
        key,value=line.split(':',1)
        if key in ('MemTotal','MemAvailable','MemFree','Cached','SwapTotal','SwapFree'):
            result[key+'_bytes']=int(value.strip().split()[0])*1024
    return result

def histogram_bucket(n):
    """Same bounded representation as V5 Go driver, integer upper edge in ns."""
    q=max(0,n)//10000
    if q<256:
        return (q+1)*10000
    shift=q.bit_length()-8
    return min((q//(1<<shift)+1)*(1<<shift)*10000,(1<<63)-1)

def paired_interval(numerators, denominators, seed=20261023, resamples=20000, minimum=7):
    """Resample entire paired windows, never individual requests.

    The estimand is exp(mean(log(per-window ratio))). The percentile bootstrap
    is descriptive at n=7; its assumptions and all window ratios remain visible.
    """
    if len(numerators)!=len(denominators):
        raise ValueError('paired windows differ in count')
    pairs=list(zip(numerators,denominators))
    if any(type(v) not in (int,float) or not math.isfinite(v) or v<=0 for p in pairs for v in p):
        return {'status':'unavailable_nonpositive_or_missing_window','pairs':pairs}
    ratios=[a/b for a,b in pairs]
    if len(pairs)<minimum:
        return {'status':'insufficient_paired_windows','minimum':minimum,'ratios':ratios}
    logs=[math.log(v) for v in ratios]
    rng=random.Random(seed)
    values=sorted(math.exp(statistics.mean(rng.choices(logs,k=len(logs)))) for _ in range(resamples))
    return {'status':'estimated','unit':'paired interleaved window','pairs':pairs,'ratios':ratios,
            'estimate':math.exp(statistics.mean(logs)),
            'lower95':values[math.floor(.025*(resamples-1))],
            'upper95':values[math.ceil(.975*(resamples-1))],
            'seed':seed,'resamples':resamples,'method':'paired-window percentile bootstrap of geometric mean ratio',
            'assumption':'windows approximate independent stationary replicates; serial dependence or common drift can invalidate coverage'}

def interleaved_orders(stacks, repeats=7, seed=20261023):
    rng=random.Random(seed)
    result=[]
    for _ in range(repeats):
        order=list(stacks)
        rng.shuffle(order)
        result.append(order)
    return result

def paired_capacity_goodput(summaries, state, references=('go','rust'), seed=20261023):
    """Compare each stack's qualified own point, never common-rate goodput."""
    result={}
    own=summaries.get('own_capacity',{})
    for reference in references:
        if 'ku' not in own or reference not in own:
            continue
        selected=('ku',reference)
        ku_windows=own['ku'].get('round_goodput_rps',[])
        reference_windows=own[reference].get('round_goodput_rps',[])
        interval=(paired_interval(ku_windows,reference_windows,seed) if len(ku_windows)==len(reference_windows)
                  else {'status':'unavailable_unpaired_windows','ku_windows':len(ku_windows),
                        'reference_windows':len(reference_windows),'minimum':7})
        qualified=all(own[s].get('qualified') is True for s in selected) and interval['status']=='estimated'
        bounds={}
        for stack in selected:
            capacity=state.get('capacity',{}).get(stack,{})
            point=capacity.get('upper_bound_point') or {}
            reasons=point.get('qualification',{}).get('reasons',[])
            credible=(capacity.get('upper_bound_censored') is False and
                      point.get('gate_kind')=='service_or_slo_failure' and
                      not any(r in ('http_500','wrong_body','service_exited') for r in reasons))
            bounds[stack]={'qualified_rate':capacity.get('candidate_rate'),
                           'upper_rate':point.get('rate'), 'credible_upper_boundary':credible,
                           'upper_bound_kind':capacity.get('upper_bound_kind'),
                           'upper_point':point}
        credible=all(v['credible_upper_boundary'] for v in bounds.values())
        result[reference]={'goodput_rps_ratio':interval,
            'eligible_for_operating_point_comparison':qualified,
            'eligible_for_capacity_target_claim':qualified and credible,
            'throughput_target_lower95_minimum':1.0,
            'throughput_target_passed':qualified and credible and interval.get('lower95',0)>=1.0,
            'capacity_brackets':bounds,
            'scope':'Ku/reference goodput at separate seven-round qualified own offered-rate points under identical SLOs; upper exploration boundaries and censoring retained. Not the common equal-arrival goodput ratio, nor an exact continuous maximum capacity.',
            'status':'qualified_operating_points_with_observed_upper_boundaries' if qualified and credible else
                ('qualified_operating_points_capacity_upper_censored' if qualified else 'own_points_not_formally_qualified')}
    return result

def benchmark_limits(env):
    mapping={'max_active_requests':('BENCH_MAX_ACTIVE',1024),
             'max_connections':('BENCH_MAX_CONNECTIONS',4096),
             'max_pending_requests':('BENCH_MAX_PENDING',128)}
    values={name:int(env.get(key,default)) for name,(key,default) in mapping.items()}
    if any(value<=0 for value in values.values()):
        raise ValueError('benchmark admission limits must be positive integers')
    return values

def declared_case_stacks(stacks, cores, memory, workload):
    return tuple(s for s in stacks if s in STACKS or s=='control' or
                 cores==4 and workload in ('ok','json_object','mixed') and
                 (memory==GIB or memory==256*MIB and workload in ('json_object','mixed')))

def sustained_case(args):
    if not args.case:
        return (4,GIB,'mixed')
    for case in cases(args):
        if args.case==f'c{case[0]}-m{case[1]}-{case[2]}' and case[0]==4 and case[2]=='mixed':
            return case
    raise ValueError('sustained --case must be a declared four-core mixed case (1GiB or256MiB)')

def aggregate_rounds(reports, workload, minimum_core_samples=1000, minimum_rounds=7):
    bins, overflow = {}, False
    # Only merge one target point and one frozen driver/protocol/service build.
    signatures=[(r.get('schema'),json.dumps({k:r.get('configuration',{}).get(k) for k in
        ('protocol_sha256','corpora_sha256','workload','duration_seconds','rate','concurrency','timeout_seconds','keepalive_idle_ms')},sort_keys=True),
        json.dumps(r.get('measurement_identity',{}).get('service'),sort_keys=True)) for r in reports]
    if signatures and any(v!=signatures[0] for v in signatures):
        raise ValueError('cannot merge different driver, protocol, load or service identity')
    for report in reports:
        counts={}
        with gzip.open(report['events']['path'], 'rt', newline='') as source:
            for row in csv.DictReader(source):
                if row['terminal'] != 'correct':
                    continue
                upper=histogram_bucket(int(row['completed_ns'])-int(row['scheduled_ns']))
                kind = row['type']
                current = bins.setdefault(kind,{})
                current[upper] = current.get(upper,0)+1
                counts[kind]=counts.get(kind,0)+1
        if sum(counts.values())!=report['ledger'].get('correct_total'):
            raise ValueError('raw correct event count differs from terminal ledger')
    pooled = {}
    for kind,current in bins.items():
        count = sum(current.values())
        quantiles = {}
        for name,fraction,minimum in (('p50_ms',.5,1),('p95_ms',.95,20),('p99_ms',.99,100),('p999_ms',.999,1000)):
            value = None
            if count >= minimum:
                rank,acc = math.ceil(count*fraction),0
                for upper,n in sorted(current.items()):
                    acc += n
                    if acc >= rank:
                        value=upper/1e6
                        break
            quantiles[name]=value
        pooled[kind]={'correct_samples':count,'representation':'v5-fixed-log128',**quantiles}
    limits={'ok':100,'json_object':100,'json_array':100,'json_escape':100,'cpu':1000,'db':250,'timer':100}
    required=('json_object','db','timer','cpu') if workload=='mixed' else (workload,)
    tails_sufficient=all(pooled.get(k,{}).get('correct_samples',0)>=minimum_core_samples for k in required)
    pooled_pass=tails_sufficient and all(pooled[k]['p99_ms']<=limits[k] for k in required)
    eligible=bool(reports) and all(qualification(r,workload)['eligible_for_formal_candidate'] for r in reports)
    rounds_sufficient=len(reports)>=minimum_rounds
    physical_complete=all(r.get('service_cost',{}).get('full_lifecycle',{}).get('available') is True and r['service_cost']['full_lifecycle'].get('natural_reclamation_passed') is True for r in reports)
    qualified=eligible and pooled_pass and rounds_sufficient and physical_complete and not overflow
    return {'pooled_correct_scheduled_to_complete':pooled,'pooled_tail_sufficient':tails_sufficient,
            'core_p99_minimum_samples_per_type':minimum_core_samples,'minimum_paired_rounds':minimum_rounds,
            'rounds_sufficient':rounds_sufficient,'all_physical_reclamation_passed':physical_complete,'all_per_round_eligible':eligible,'histogram_overflow':overflow,
            'qualified':qualified,
            'status':'qualified' if qualified else ('indeterminate_tail_sample_size' if eligible and not tails_sufficient else
                ('insufficient_paired_windows' if eligible and not rounds_sufficient else ('physical_reclamation_failed_or_unmeasured' if eligible and not physical_complete else 'failed'))),
            'round_goodput_rps':[r['ledger']['goodput_rps'] for r in reports],
            'median_goodput_rps':statistics.median(r['ledger']['goodput_rps'] for r in reports) if reports else None,
            'round_failure_fraction_planned':[r['ledger']['failure_fraction_planned'] for r in reports],
            'round_service_cpu_seconds_per_correct':[r['service_cost']['cpu_seconds_per_correct'] for r in reports],
            'per_round_p99_note':'below100 use conservative observed max; core pooled claim requires1000; never average p99'}

def error_reason_delta(before, after):
    left=before.get('errors',{}).get('counts') if isinstance(before,dict) else None
    right=after.get('errors',{}).get('counts') if isinstance(after,dict) else None
    if not isinstance(left,dict) or not isinstance(right,dict):
        return {'available':False,'reason':'protected internal error counters unavailable for this stack'}
    if set(left)!=set(right) or any(type(v) is not int or type(left.get(k)) is not int or v<left[k] for k,v in right.items()):
        return {'available':False,'reason':'counter reset or invalid protected metrics'}
    return {'available':True,'counts':{k:v-left.get(k,0) for k,v in right.items()},
            'scope':'protected management metric deltas; external HTTP status is retained separately'}

def full_lifecycle_cost(report, physical_end, quiet, termination, final_metrics):
    cost=report['service_cost']
    before,driver_end=cost['before'],cost['after']
    begin_cpu=before['cpu_stat'].get('usage_usec',0)
    end_cpu=physical_end['cpu_stat'].get('usage_usec',0)
    total=max(0,end_cpu-begin_cpu)/1e6
    tail=max(0,end_cpu-driver_end['cpu_stat'].get('usage_usec',0))/1e6
    correct=report['ledger']['correct_total']
    return {'available':quiet or bool(termination and physical_end.get('process_count')==0 and termination.get('cgroup_removed')),'cpu_seconds':total,'cpu_seconds_per_correct':total/correct if correct else None,
        'post_driver_cpu_seconds':tail,'physical_end':physical_end,'final_metrics':final_metrics,
        'physically_quiet_before_freeze':quiet,'forced_termination':bool(termination and termination.get('forced')),
        'termination':termination,'natural_reclamation_passed':quiet and not bool(termination and termination.get('forced')),
        'scope':'whole service domain from driver launch until observable physical quiescence or completed forced termination; includes expensive failed generations',
        'internal_error_reason_delta':error_reason_delta(cost.get('runtime_before',{}),final_metrics),
        'useless_work_cpu_seconds':None,'useless_work_note':'not inferable from cgroup total: needs per-request undeliverable/cancellable responsibility; post-driver CPU is not automatically useless work'}

def throttle_delta(before, after):
    keys=('nr_periods','nr_throttled','throttled_usec','usage_usec')
    return {key:max(0,after.get('cpu_stat',{}).get(key,0)-before.get('cpu_stat',{}).get(key,0)) for key in keys}

def overload_boundary(report, workload):
    """Only an actual observable service boundary, with valid generation."""
    gate=qualification(report,workload)
    censored=any(reason in gate['reasons'] for reason in ('invalid_driver_or_not_started','unreconciled_plan','driver_cpu_saturated','resource_endpoints_missing','resource_observation_invalid','measurement_monotonic_clock_unavailable','measurement_monotonic_clock_uncertain'))
    results=report['ledger'].get('results',{})
    evidence=[]
    if results.get('rejected_503',0) or results.get('rejected_429',0):
        evidence.append('explicit_service_rejection')
    if any('_p99_over_slo' in reason for reason in gate['reasons']):
        evidence.append('observed_tail_slo_exceeded')
    if any(reason in gate['reasons'] for reason in ('oom','oom_kill')):
        evidence.append('service_memory_limit')
    fault=any(reason in gate['reasons'] for reason in ('http_500','wrong_body','service_exited'))
    return {'status':'measurement_invalid_censored' if censored else ('service_fault_without_capacity_attribution' if fault else ('boundary_observed' if evidence else 'pressure_below_observed_boundary')),
            'boundary_observed':bool(evidence) and not censored and not fault,'evidence':evidence,'qualification':gate,
            'http_500_count':report['ledger'].get('http_500_total',report['ledger'].get('transport_counts',{}).get('status:500',0)),
            'note':'unknown500 is a defect observation, never expected503; no claim of true overload without boundary evidence'}

class Domain:
    def __init__(self, harness, label, cores, memory, cpus):
        self.harness, self.label = harness, label
        self.path = harness.cgroup_root / (label + '-' + secrets.token_hex(4))
        self.path.mkdir()
        self.frozen = False
        settings = {'memory.max': str(memory), 'memory.swap.max': '0',
                    'cpu.max': f'{cores * 100000} 100000', 'pids.max': '4096'}
        if (self.path / 'cpuset.cpus').exists():
            settings['cpuset.cpus'] = cpus
            settings['cpuset.mems'] = (harness.cgroup_root / 'cpuset.mems.effective').read_text().strip()
        for key, value in settings.items():
            (self.path / key).write_text(value)
        self.settings = settings
    def snapshot(self):
        result = {'label': self.label, 'monotonic_ns': time.monotonic_ns(),
                  'wall_time_ns': time.time_ns(), 'frozen': self.frozen,
                  'cpu_stat': counters(self.path / 'cpu.stat'),
                  'memory_stat': counters(self.path / 'memory.stat'),
                  'memory_events': counters(self.path / 'memory.events')}
        for key in ('memory.current', 'memory.peak', 'pids.current'):
            try:
                result[key.replace('.', '_')] = int((self.path / key).read_text().strip())
            except OSError:
                result[key.replace('.', '_')] = None
        try:
            pids = [int(p) for p in (self.path / 'cgroup.procs').read_text().split()]
        except OSError:
            pids = []
        result.update(process_count=len(pids), threads=0, fd=0, sum_rss_bytes=0,
                      sum_pss_bytes=0, rss_scope='sum of process RSS; shared pages may repeat',
                      pss_scope='sum of process proportional resident pages',
                      voluntary_context_switches_live_threads=0,nonvoluntary_context_switches_live_threads=0,
                      migrations_live_threads=0,last_cpu_thread_counts={},scheduler_threads_observed=0,
                      scheduler_scope='live-thread cumulative counters; terminated threads disappear, never treat a negative delta as zero-cost')
        for pid in pids:
            proc = Path('/proc') / str(pid)
            try:
                tasks=list((proc / 'task').iterdir())
                result['threads'] += len(tasks)
                for task in tasks[:4096]:
                    try:
                        status_thread=(task/'status').read_text()
                        for line in status_thread.splitlines():
                            if line.startswith('voluntary_ctxt_switches:'):
                                result['voluntary_context_switches_live_threads']+=int(line.split()[1])
                            elif line.startswith('nonvoluntary_ctxt_switches:'):
                                result['nonvoluntary_context_switches_live_threads']+=int(line.split()[1])
                        for line in (task/'sched').read_text().splitlines():
                            if line.lstrip().startswith('se.nr_migrations'):
                                result['migrations_live_threads']+=int(line.rsplit(':',1)[1])
                        cpu=(task/'stat').read_text().rsplit(')',1)[1].split()[36]
                        result['last_cpu_thread_counts'][cpu]=result['last_cpu_thread_counts'].get(cpu,0)+1
                        result['scheduler_threads_observed']+=1
                    except (OSError,ValueError,IndexError):
                        pass
                result['fd'] += len(list((proc / 'fd').iterdir()))
                status = (proc / 'status').read_text()
                for line in status.splitlines():
                    if line.startswith('VmRSS:'):
                        result['sum_rss_bytes'] += int(line.split()[1]) * 1024
                for line in (proc / 'smaps_rollup').read_text().splitlines():
                    if line.startswith('Pss:'):
                        result['sum_pss_bytes'] += int(line.split()[1]) * 1024
            except (OSError, ValueError):
                continue
        return result
    def freeze(self, value):
        (self.path / 'cgroup.freeze').write_text('1' if value else '0')
        self.frozen = bool(value)
        target = int(value)
        deadline = time.monotonic() + 2
        while time.monotonic() < deadline:
            if counters(self.path / 'cgroup.events').get('frozen', 0) == target:
                return
            time.sleep(.01)
        raise RuntimeError('cgroup freeze transition deadline')
    def kill(self):
        if self.frozen:
            self.freeze(False)
        try:
            (self.path / 'cgroup.kill').write_text('1')
        except OSError:
            for pid in (self.path / 'cgroup.procs').read_text().split():
                try:
                    os.kill(int(pid), signal.SIGKILL)
                except ProcessLookupError:
                    pass
    def remove(self):
        deadline = time.monotonic() + 3
        while time.monotonic() < deadline:
            try:
                if not (self.path / 'cgroup.procs').read_text().strip():
                    self.path.rmdir()
                    return True
            except FileNotFoundError:
                return True
            except OSError:
                pass
            time.sleep(.02)
        return False

class ExternalDomain:
    def __init__(self, path, label='database'):
        self.path, self.label, self.frozen = Path(path), label, False
    snapshot = Domain.snapshot

class Sampler:
    def __init__(self, domains, output=None, interval=1.0, maximum=2048):
        self.domains, self.output = list(domains), Path(output) if output else None
        self.interval, self.maximum = interval, maximum
        self.samples, self.overflow, self.error = [], False, None
        self.done = threading.Event()
        self.thread = None
    def sample(self):
        if len(self.samples) >= self.maximum:
            self.overflow = True
            return
        try:
            usage=resource.getrusage(resource.RUSAGE_SELF)
            cpu_ticks=Path('/proc/stat').read_text().splitlines()[0].split()[1:]
            self.samples.append({'wall_time_ns': time.time_ns(), 'monotonic_ns': time.monotonic_ns(),
                                 'observer':{'pid':os.getpid(),'cpu_user_seconds':usage.ru_utime,'cpu_system_seconds':usage.ru_stime,
                                             'lifetime_peak_rss_bytes':usage.ru_maxrss*1024,'scope':'root coordinator, outside all service domains'},
                                 'guest_memory':guest_memory(),
                                 'guest_cpu_ticks':dict(zip(('user','nice','system','idle','iowait','irq','softirq','steal','guest','guest_nice'),map(int,cpu_ticks))),
                                 'guest_loadavg':Path('/proc/loadavg').read_text().strip(),
                                 'domains': [d.snapshot() for d in self.domains]})
        except Exception as error:
            self.error = repr(error)
    def start(self):
        self.sample()
        def work():
            while not self.done.wait(self.interval):
                self.sample()
        self.thread = threading.Thread(target=work, daemon=True)
        self.thread.start()
        return self
    def stop(self):
        self.done.set()
        if self.thread:
            self.thread.join(timeout=3)
        self.sample()
        if self.output:
            write_json(self.output, {'samples': self.samples, 'overflow': self.overflow,
                                     'error': self.error, 'interval_seconds': self.interval})
        return self.samples

JAVA_SHUTDOWN_PREFIX = b'BENCH_SHUTDOWN '
JAVA_SHUTDOWN_STAGES = ['http_drained', 'in_flight_completed', 'cpu_terminated', 'pool_closed', 'vertx_closed']


def java_shutdown_receipt(path):
    """Read only a bounded log tail; incomplete output is never a close receipt."""
    try:
        with Path(path).open('rb') as stream:
            stream.seek(0, os.SEEK_END)
            size = stream.tell()
            stream.seek(max(0, size - 8192))
            tail = stream.read(8192)
    except OSError:
        return {'status': 'missing'}
    lines = [line for line in tail.splitlines() if line.startswith(JAVA_SHUTDOWN_PREFIX)]
    if len(lines) != 1:
        return {'status': 'missing' if not lines else 'duplicate'}
    if not tail.endswith(b'\n'):
        return {'status': 'truncated'}
    if len(lines[0]) > 2048:
        return {'status': 'oversized'}
    try:
        receipt = json.loads(lines[0][len(JAVA_SHUTDOWN_PREFIX):])
    except (UnicodeError, ValueError):
        return {'status': 'invalid'}
    return {'status': 'recorded', 'receipt': receipt}


def service_termination_succeeded(stack, termination):
    """Preserve exit-zero gates; classify only a proven Java SIGTERM hook."""
    if termination.get('forced') is not False or termination.get('cgroup_removed') is not True:
        return False
    if stack != 'java':
        return termination.get('exit_code') == 0
    if (termination.get('exit_code') != 143 or termination.get('requested_signal') != signal.SIGTERM
            or any(type(termination.get('after', {}).get(name)) is not int or termination['after'][name] != 0
                   for name in ('process_count', 'threads', 'fd'))):
        return False
    for name in ('oom', 'oom_kill', 'oom_group_kill'):
        before = termination.get('before', {}).get('memory_events', {}).get(name)
        after = termination.get('after', {}).get('memory_events', {}).get(name)
        if type(before) is not int or type(after) is not int or before != 0 or after != 0:
            return False
    evidence = termination.get('java_shutdown', {})
    receipt = evidence.get('receipt')
    if evidence.get('status') != 'recorded' or not isinstance(receipt, dict):
        return False
    if (receipt.get('schema') != 'ku-java-shutdown-v1' or receipt.get('complete') is not True
            or receipt.get('deadline_expired') is not False or 'error' in receipt
            or receipt.get('completed_stages') != JAVA_SHUTDOWN_STAGES):
        return False
    for name, expected in (('pid', termination.get('process_id')),
                           ('listeners', termination.get('expected_listeners')),
                           ('expected_listeners', termination.get('expected_listeners'))):
        if type(expected) is not int or expected <= 0 or type(receipt.get(name)) is not int or receipt[name] != expected:
            return False
    if type(receipt.get('elapsed_ns')) is not int or not 0 <= receipt['elapsed_ns'] < 4_500_000_000:
        return False
    return all(type(receipt.get(name)) is int and receipt[name] == 0
               for name in ('active', 'waiting_timer', 'actual_cpu_jobs', 'database_pending'))


@dataclasses.dataclass
class Service:
    harness: object
    stack: str
    address: str
    domain: Domain
    cwd: Path
    process: subprocess.Popen
    startup: dict
    management_address: str | None = None
    control_token: str | None = None
    stopped: bool = False
    def metrics(self):
        if self.stack == 'ku':
            status, body = request(self.management_address, '/__ku/metrics',
                                   headers={'X-Ku-Control-Token': self.control_token})
        else:
            status, body = request(self.address, '/metrics')
        if status != 200:
            raise RuntimeError(f'metrics HTTP {status}')
        result=json.loads(body)
        if self.stack=='ku' and (not isinstance(result.get('loops'),list) or len(result['loops'])!=self.startup['cores']):
            raise RuntimeError('Ku metrics owner list does not match configured event loops')
        return result
    def freeze(self, value):
        self.domain.freeze(value)
    def stop(self):
        if self.stopped:
            return self.startup.get('cleanup', {})
        self.stopped = True
        if self.domain.frozen:
            self.freeze(False)
        before = self.domain.snapshot()
        drain = None
        requested_signal = None
        if self.stack == 'ku' and self.process.poll() is None:
            try:
                status, body = request(self.management_address, '/__ku/drain', method='POST',
                                       headers={'X-Ku-Control-Token': self.control_token}, timeout=1)
                drain = {'status': status, 'body_prefix': body[:256].decode(errors='replace')}
            except Exception as error:
                drain = {'error': repr(error)}
        else:
            try:
                if self.process.poll() is None:
                    self.process.terminate()
                    requested_signal = signal.SIGTERM
            except ProcessLookupError:
                pass
        forced = False
        try:
            self.process.wait(timeout=5)
        except subprocess.TimeoutExpired:
            forced = True
            self.domain.kill()
            self.process.wait(timeout=3)
        # A primary may exit before children. Bound the entire cgroup cleanup.
        deadline = time.monotonic() + 2
        while time.monotonic() < deadline:
            if not (self.domain.path / 'cgroup.procs').read_text().strip():
                break
            time.sleep(.02)
        remaining = (self.domain.path / 'cgroup.procs').read_text().strip()
        if remaining:
            forced = True
            self.domain.kill()
            deadline=time.monotonic()+3
            while time.monotonic()<deadline and (self.domain.path/'cgroup.procs').read_text().strip():
                time.sleep(.01)
        after = self.domain.snapshot()
        removed = self.domain.remove()
        runtime_cleanup=[]
        if removed and after['process_count']==0:
            layout=self.startup.get('runtime_layout')
            targets=[self.cwd/'config.json']
            if layout:
                targets.extend([Path(layout['local_executable']),Path(layout['config_path'])])
            for path in targets:
                if path.is_file():
                    path.unlink()
                    runtime_cleanup.append(str(path))
        result = {'exit_code': self.process.returncode, 'forced': forced, 'drain': drain,
                  'before': before, 'after': after, 'cgroup_removed': removed,'removed_runtime_files':runtime_cleanup}
        result.update(requested_signal=requested_signal, process_id=self.process.pid,
                      expected_listeners=self.startup['cores'])
        if self.stack == 'java':
            result['java_shutdown'] = java_shutdown_receipt(self.cwd.parent / 'service.log')
        succeeded = service_termination_succeeded(self.stack, result)
        result['graceful_signal'] = self.stack == 'java' and succeeded
        result['termination_kind'] = ('graceful_signal' if result['graceful_signal'] else 'normal_exit') if succeeded else 'failed'
        self.startup['cleanup'] = result
        write_json(self.cwd.parent / 'service.json', self.startup)
        return result

class Harness:
    def __init__(self, manifest_root, output_root):
        self.root, self.output = Path(manifest_root).resolve(), Path(output_root).resolve()
        self.manifest = read_json(self.root / 'manifest.json')
        self.output.mkdir(parents=True, exist_ok=True)
        self.uid, self.gid = self.manifest.get('uid', 501), self.manifest.get('gid', 501)
        soft, hard = resource.getrlimit(resource.RLIMIT_NOFILE)
        desired = min(65536, hard) if hard != resource.RLIM_INFINITY else 65536
        if desired < 65536:
            raise RuntimeError('RLIMIT_NOFILE hard limit must be at least65536')
        resource.setrlimit(resource.RLIMIT_NOFILE, (max(soft, desired), hard))
        self.cgroup_root = Path(self.manifest.get('cgroup_root', '/sys/fs/cgroup/ku-cross-stack'))
        if not Path('/sys/fs/cgroup/cgroup.controllers').exists() or os.geteuid() != 0:
            raise RuntimeError('root and Linux cgroup v2 are required; no sampling-only substitute')
        parent = self.cgroup_root.parent
        required = {'cpu', 'memory', 'pids', 'cpuset'}
        for current in (parent, self.cgroup_root):
            current.mkdir(exist_ok=True)
            available = set((current / 'cgroup.controllers').read_text().split())
            enable = required & available
            (current / 'cgroup.subtree_control').write_text(' '.join('+' + x for x in enable))
        self.services, self.serial = [], 0
        self.db_domain = ExternalDomain(self.manifest['db_cgroup']) if self.manifest.get('db_cgroup') else None
        self.protocol = Path(self.manifest.get('protocol', Path(__file__).with_name('protocol.json')))
        self.identity={'manifest_sha256':hashlib.sha256((self.root/'manifest.json').read_bytes()).hexdigest(),
                       'protocol_sha256':hashlib.sha256(self.protocol.read_bytes()).hexdigest(),
                       'harness_sha256':hashlib.sha256(Path(__file__).read_bytes()).hexdigest()}
        identity_path=self.output/'identity.json'
        if identity_path.exists() and read_json(identity_path)!=self.identity:
            raise RuntimeError('candidate/driver/config identity changed; preserve prior output and use a new root')
        write_json(identity_path,self.identity)
    def samplers(self, domains, output=None):
        return Sampler(domains, output)
    def launch(self, command, cwd, env, domain, log):
        log = open(log, 'ab', buffering=0)
        try:
            return subprocess.Popen([sys.executable, str(Path(__file__).resolve()), '__child',
                                     str(domain.path), str(self.uid), str(self.gid), str(cwd), '--'] + list(command),
                                    env=env, stdout=log, stderr=log, start_new_session=True)
        finally:
            log.close()
    def start(self, stack, cores=4, memory=GIB, output=None, extra_env=None, peer_port=None):
        self.serial += 1
        output = Path(output or self.output / f'service-{stack}-{self.serial:04d}')
        output.mkdir(parents=True, exist_ok=True)
        cwd = output / 'work'
        cwd.mkdir(exist_ok=True)
        os.chown(cwd, self.uid, self.gid)
        address = f'127.0.0.1:{free_port()}'
        management = f'127.0.0.1:{free_port()}'
        token = secrets.token_hex(24)
        peer_port = peer_port or self.manifest.get('peer_port', 19091)
        replacements = {'root': self.root, 'cores': cores, 'workers': cores, 'memory': memory,
                        'address': address, 'workdir': cwd, 'peer_port': peer_port}
        spec = self.manifest['stacks'][stack]
        command = [template(x, replacements) for x in spec['command']]
        source_command=list(command)
        runtime_layout=None
        config_directory=cwd
        if stack=='ku' and spec.get('runtime_layout'):
            layout=spec['runtime_layout']
            binary_directory=cwd/layout['binary_dir']
            config_directory=cwd/layout['fs_base_dir']
            for directory in (binary_directory,config_directory):
                directory.mkdir(parents=True,exist_ok=True)
                os.chown(directory,self.uid,self.gid)
            original=Path(command[0])
            copied=binary_directory/original.name
            shutil.copy2(original,copied)
            copied.chmod(0o755)
            os.chown(copied,self.uid,self.gid)
            original_hash=hashlib.sha256(original.read_bytes()).hexdigest()
            if hashlib.sha256(copied.read_bytes()).hexdigest()!=original_hash:
                raise RuntimeError('isolated Ku executable copy differs from source artifact')
            command[0]=str(copied)
            runtime_layout={'source_executable':str(original),'local_executable':str(copied),
                            'sha256':original_hash,'config_path':str(config_directory/'config.json'),
                            'reason':'native fs base is executable-relative, not cwd; no runtime modification'}
        env = os.environ.copy()
        env.update({k: template(str(v), replacements) for k, v in self.manifest.get('service_env', {}).items()})
        env.update({k: template(str(v), replacements) for k, v in spec.get('env', {}).items()})
        env.update(BENCH_ADDR=address, BENCH_WORKERS=str(cores), BENCH_CPU_WORKERS=str(cores),
                   BENCH_DB_POOL='16', BENCH_PEER_PORT=str(peer_port), BENCH_MAX_ACTIVE='1024',
                   BENCH_MAX_PENDING='128', BENCH_MAX_CONNECTIONS='4096')
        env.update({k: str(v) for k, v in (extra_env or {}).items()})
        if self.manifest.get('db_url') and not env.get('BENCH_DB_URL'):
            env['BENCH_DB_URL'] = self.manifest['db_url']
        if env.get('BENCH_DB_URL'):
            parsed=urllib.parse.urlsplit(env['BENCH_DB_URL'])
            if parsed.scheme not in ('postgres','postgresql'):
                raise RuntimeError('benchmark DB configuration must be a PostgreSQL URI')
            parameters=[(key,value) for key,value in urllib.parse.parse_qsl(parsed.query,keep_blank_values=True)
                        if key!='application_name']
            parameters.append(('application_name','ku-cross-stack-'+stack))
            env['BENCH_DB_URL']=urllib.parse.urlunsplit(parsed._replace(query=urllib.parse.urlencode(parameters)))
        if stack == 'ku':
            env.update(KU_SERVICE_PROBE_ADDR=management, KU_SERVICE_CONTROL_TOKEN=token)
        config = config_directory / 'config.json'
        admission=benchmark_limits(env)
        config.write_text(json.dumps({'addr': address, 'db_url': env.get('BENCH_DB_URL', ''),
                                      'workers': cores, 'metrics_file': str(cwd / 'metrics.json'),**admission}))
        config.chmod(0o600)
        os.chown(config, self.uid, self.gid)
        domain = Domain(self, 'service-' + stack, cores, memory, '0' if cores == 1 else f'0-{cores-1}')
        startup = {'stack': stack, 'cores': cores, 'memory': memory, 'command': command,
                   'cgroup': str(domain.path), 'limits': domain.settings, 'address': address,
                   'environment': {k: v for k, v in env.items()
                                   if (k.startswith(('BENCH_', 'KU_', 'JAVA', 'DOTNET_')) or
                                       k in ('GOMAXPROCS','GOGC','GOMEMLIMIT','GODEBUG','NODE_OPTIONS','UV_THREADPOOL_SIZE','LD_LIBRARY_PATH'))
                                   and k not in ('BENCH_DB_URL','KU_SERVICE_CONTROL_TOKEN')},
                   'db_url_sha256': hashlib.sha256(env.get('BENCH_DB_URL', '').encode()).hexdigest(),
                   'started_wall_time_ns': time.time_ns(), 'reuse': 'same instance within case; frozen outside its turn'}
        startup['source_command']=source_command
        startup['configured_admission_limits']=admission
        if runtime_layout:
            startup['runtime_layout']=runtime_layout
        startup['identity']={'stack':stack,'cores':cores,'memory':memory,
                             'command':[x.replace(address,'<address>').replace(str(cwd),'<workdir>') for x in command],
                             'environment':{k:v.replace(address,'<address>').replace(management,'<management>').replace(str(cwd),'<workdir>') for k,v in startup['environment'].items()},
                             'db_url_sha256':startup['db_url_sha256'],**self.identity}
        process = self.launch(command, cwd, env, domain, output / 'service.log')
        service = Service(self, stack, address, domain, cwd, process, startup, management, token)
        self.services.append(service)
        deadline = time.monotonic() + 20
        try:
            while time.monotonic() < deadline:
                if process.poll() is not None:
                    raise RuntimeError(f'{stack} exited during startup: {process.returncode}; see {output}/service.log')
                try:
                    status, body = request(address, '/ok?rid=1', timeout=.2)
                    if status == 200 and body == b'ok:1':
                        startup['ready_wall_time_ns'] = time.time_ns()
                        startup['idle_resources'] = domain.snapshot()
                        write_json(output / 'service.json', startup)
                        return service
                except (OSError, http.client.HTTPException):
                    pass
                time.sleep(.02)
            raise RuntimeError(f'{stack} readiness deadline')
        except BaseException:
            service.stop()
            raise
    def client(self, service, workload, rate, duration, output, concurrency=256, timeout=3, events=True, runtime_metrics=False, idle_ms=500):
        output = Path(output)
        output.mkdir(parents=True, exist_ok=True)
        report = output / 'client.json'
        measurement_identity={'service':service.startup['identity'],'workload':workload,'rate':rate,
                              'duration':duration,'concurrency':concurrency,'timeout':timeout,'events':events,
                              'protocol_sha256':self.identity['protocol_sha256'],'driver_manifest_sha256':self.identity['manifest_sha256'],'harness_revision':5,'keepalive_idle_ms':idle_ms}
        if report.exists():
            previous = read_json(report)
            if 'service_cost' in previous:
                if previous.get('measurement_identity')!=measurement_identity:
                    raise RuntimeError(f'completed window identity differs: {report}; do not mix or overwrite')
                return previous
            report.rename(report.with_name('client.interrupted-' + str(time.time_ns()) + '.json'))
        for existing in output.iterdir():
            if existing.name in ('events.csv.gz', 'client.log', 'resources.json'):
                existing.rename(existing.with_name(existing.name + '.interrupted-' + str(time.time_ns())))
        os.chown(output, self.uid, self.gid)
        domain = Domain(self, 'driver', 3, GIB, self.manifest.get('driver_cpuset', '4-6'))
        spec = self.manifest['client']
        replacements = {'root': self.root}
        command = [template(x, replacements) for x in spec['command']]
        command += ['--address', service.address, '--workload', workload, '--protocol', str(self.protocol),
                    '--duration', str(duration), '--rate', str(rate), '--concurrency', str(concurrency),
                    '--queue', '65536', '--timeout', str(timeout), '--keepalive-idle-ms',str(idle_ms),'--output', str(report)]
        if events:
            command += ['--events', str(output / 'events.csv.gz')]
        env = os.environ.copy()
        env.update({k: str(v) for k, v in spec.get('env', {}).items()})
        env.setdefault('GOMAXPROCS', '3')
        domains = [service.domain, domain] + ([self.db_domain] if self.db_domain else [])
        sampler = Sampler(domains, output / 'resources.json').start()
        runtime_stop = threading.Event()
        runtime_samples = []
        def runtime_work():
            while not runtime_stop.is_set() and len(runtime_samples) < 125:
                try:
                    value = service.metrics()
                except Exception as error:
                    value = {'observation_error': repr(error)}
                runtime_samples.append({'wall_time_ns': time.time_ns(), 'monotonic_ns':time.monotonic_ns(), 'metrics': value})
                if runtime_stop.wait(5):
                    break
        runtime_thread = threading.Thread(target=runtime_work, daemon=True) if runtime_metrics else None
        if runtime_thread:
            runtime_thread.start()
        observer_before = resource.getrusage(resource.RUSAGE_SELF)
        before = service.domain.snapshot()
        try:
            runtime_before=service.metrics()
        except Exception as error:
            runtime_before={'observation_error':repr(error)}
        process = self.launch(command, Path(spec.get('cwd', self.root)), env, domain, output / 'client.log')
        try:
            code = process.wait(timeout=duration + timeout + 15)
        except subprocess.TimeoutExpired:
            domain.kill()
            process.wait(timeout=3)
            raise RuntimeError('bounded driver execution deadline; see retained log')
        finally:
            runtime_stop.set()
            if runtime_thread:
                runtime_thread.join(timeout=3)
            samples = sampler.stop()
            after = service.domain.snapshot()
            driver_after = domain.snapshot()
            domain.kill()
            removed = domain.remove()
        if code != 0 or not report.exists():
            raise RuntimeError(f'driver exit={code}, report exists={report.exists()}; see {output}/client.log')
        result = read_json(report)
        result['measurement_identity']=measurement_identity
        observer_after = resource.getrusage(resource.RUSAGE_SELF)
        cpu = max(0, after['cpu_stat'].get('usage_usec', 0) - before['cpu_stat'].get('usage_usec', 0)) / 1e6
        correct = result['ledger']['correct_total']
        result['service_cost'] = {'lifetime_cpu_seconds': cpu, 'driver_lifetime_cpu_seconds':cpu, 'complete_lifecycle_accounted':False, 'cpu_seconds_per_correct': cpu / correct if correct else None,
                                  'correct_per_cpu_second': correct / cpu if cpu else None,
                                  'scope': 'entire service cgroup over driver lifetime only; post-driver work is separate full_lifecycle cost',
                                  'runtime_before':runtime_before, 'throttling':throttle_delta(before,after), 'before': before, 'after': after, 'samples_path': str(output / 'resources.json'),
                                  'arrival_window_interpolation': 'see 1Hz samples and client origin; do not claim exact subsecond accounting',
                                  'driver_after': driver_after, 'driver_cgroup_removed': removed,
                                  'sampler_overflow': sampler.overflow, 'sampler_error': sampler.error,
                                  'observer_cpu_seconds': (observer_after.ru_utime + observer_after.ru_stime) - (observer_before.ru_utime + observer_before.ru_stime),
                                  'observer_cpu_scope': 'whole coordinator process during this client lifetime; overlapping probe-client observers are not additive',
                                  'service_exited': service.process.poll() is not None}
        first_driver=next((d for sample in samples for d in sample['domains'] if d['label']=='driver'),None)
        if first_driver:
            elapsed=(driver_after['monotonic_ns']-first_driver['monotonic_ns'])/1e9
            driver_cpu=(driver_after['cpu_stat'].get('usage_usec',0)-first_driver['cpu_stat'].get('usage_usec',0))/1e6
            fraction=driver_cpu/(3*elapsed) if elapsed>0 else None
        else:
            elapsed=driver_cpu=fraction=None
        result['service_cost']['driver_headroom']={'domain_cpu_seconds':driver_cpu,
            'domain_lifetime_seconds':elapsed,'quota_cores':3,'fraction_of_quota':fraction,
            'saturated':fraction is not None and fraction>=.9,
            'gate':'average whole-driver CPU >=90% of its quota censors server capacity; bursts and queueing still require raw-evidence review'}
        if runtime_metrics:
            write_json(output / 'runtime-metrics.json', {'interval_seconds':5,'maximum':125,'samples':runtime_samples,
                        'allocator_counts':'Go/Node standard metrics only; Rust allocator totals not available without instrumentation; Ku existing ledgers only'})
            result['runtime_metrics_path'] = str(output / 'runtime-metrics.json')
        clock=measurement_clock(result['configuration'])
        result['service_cost']['cross_process_clock']=clock
        if clock['available']:
            start=clock['origin_ns']
            estimate=arrival_cpu(samples,service.domain.label,start,start+int(duration*1e9))
            estimate['client_anchor_uncertainty_ns']=clock['uncertainty_ns']
        else:
            estimate={'available':False,'reason':clock['reason']}
        result['service_cost']['arrival_window_cpu_estimate']=estimate
        rows=[d for sample in samples for d in sample['domains'] if d['label']==service.domain.label
              and clock['available'] and start<=d['monotonic_ns']<=start+int(duration*1e9)]
        result['service_cost']['arrival_window_memory']={
            'steady_median_bytes':statistics.median(d['memory_current'] for d in rows[len(rows)//5:len(rows)*4//5]) if len(rows)>=5 else None,
            'observed_peak_bytes':max((d['memory_current'] for d in rows),default=None),
            'sample_count':len(rows),'scope':'whole service cgroup current memory sampled within arrival window; steady middle60%; peak is observed not exact allocation highwater'}
        per_interval=[]
        for label in [d.label for d in domains]:
            observations=[d for sample in samples for d in sample['domains'] if d['label']==label]
            deltas=[throttle_delta(a,b) for a,b in zip(observations,observations[1:])]
            per_interval.append({'label':label,'total':throttle_delta(observations[0],observations[-1]) if observations else None,
                'maximum_interval_throttled_usec':max((d['throttled_usec'] for d in deltas),default=None)})
        result['service_cost']['all_domain_throttling']=per_interval
        write_json(report, result)
        return result
    def close(self):
        results = []
        for service in reversed(self.services):
            if not service.stopped:
                try:
                    results.append(service.stop())
                except Exception as error:
                    results.append({'error': repr(error), 'stack': service.stack})
        return results

def qualification(result, workload):
    """Small-tail samples stay explicit, never silently treated as p99 evidence."""
    ledger = result['ledger']
    reasons = []
    clock=measurement_clock(result.get('configuration',{}))
    if not clock['available']:
        reasons.append(clock['reason'])
    if not result.get('valid_driver'):
        reasons.append('invalid_driver_or_not_started')
    if not ledger.get('planned_reconciled'):
        reasons.append('unreconciled_plan')
    if ledger.get('failure_fraction_planned') is None or ledger['failure_fraction_planned'] > .001:
        reasons.append('correct_fraction_below_99.9_percent')
    if ledger.get('http_500_total',ledger.get('transport_counts',{}).get('status:500',0)):
        reasons.append('http_500')
    if ledger.get('results', {}).get('body_error', 0):
        reasons.append('wrong_body')
    if result.get('service_cost', {}).get('service_exited'):
        reasons.append('service_exited')
    cost = result.get('service_cost', {})
    if cost.get('driver_headroom',{}).get('saturated'):
        reasons.append('driver_cpu_saturated')
    if not cost.get('before') or not cost.get('after'):
        reasons.append('resource_endpoints_missing')
    if cost.get('sampler_overflow') or cost.get('sampler_error'):
        reasons.append('resource_observation_invalid')
    before, after = cost.get('before', {}), cost.get('after', {})
    for name in ('oom', 'oom_kill'):
        if after.get('memory_events', {}).get(name, 0) > before.get('memory_events', {}).get(name, 0):
            reasons.append(name)
    types = result['by_type'] if workload == 'mixed' else {workload: ledger}
    limits = {'ok': 100, 'json_object': 100, 'json_array': 100, 'json_escape': 100,
              'cpu': 1000, 'db': 250, 'timer': 100}
    tail_insufficient = []
    if workload == 'mixed':
        for kind in ('json_object','db','timer','cpu'):
            if kind not in types:
                tail_insufficient.append({'type':kind,'correct_samples':0,'reason':'short arrival window did not cover complete100-plan mixture cycle'})
    for kind, view in types.items():
        if kind not in limits:
            continue
        hist = view['correct_scheduled_to_complete']
        p99 = hist.get('p99_ms')
        if p99 is None:
            tail_insufficient.append({'type': kind, 'correct_samples': hist['count'],
                                      'max_observed_ms': hist.get('max_ms')})
            # Conservative observed maximum is allowed to locate the next test
            # point, but remains provisional instead of a qualified p99 claim.
            if hist.get('max_ms', 0) > limits[kind]:
                reasons.append(kind + '_observed_max_over_slo')
        elif p99 > limits[kind]:
            reasons.append(kind + '_p99_over_slo')
    return {'eligible_for_formal_candidate': not reasons, 'qualified': not reasons and not tail_insufficient,
            'tail_insufficient': tail_insufficient, 'reasons': reasons}

def derive_verified_rates(state, summaries, stacks=STACKS):
    """Only seven-round formal gates may select subsequent load rates."""
    verified={}
    for stack in stacks:
        own=summaries.get('own_capacity',{}).get(stack,{})
        common=summaries.get('common',{}).get(stack,{})
        candidate=state.get('capacity',{}).get(stack,{}).get('candidate_rate',0)
        common_rate=state.get('common_rate')
        rate,basis=None,None
        if own.get('qualified') is True and candidate>0:
            rate,basis=candidate,'seven_round_own_capacity'
        elif common.get('qualified') is True and common_rate is not None and common_rate>0:
            rate,basis=common_rate,'seven_round_common_point_fallback'
        verified[stack]={'verified_rate':rate,'basis':basis,
                         'own_formal_status':own.get('status','not_executed'),
                         'common_formal_status':common.get('status','not_executed')}
    included=[s for s in stacks if verified[s]['verified_rate'] is not None]
    minimum=min(verified[s]['verified_rate'] for s in included) if included else None
    sustained=math.floor(.6*minimum) if minimum is not None else None
    return {'verified_capacity':verified,'verified_rates':{s:verified[s]['verified_rate'] for s in stacks},
            'verified_partial_stacks':included,'unverified_stacks':[s for s in stacks if s not in included],
            'derived_minimum_verified_rate':minimum,
            'derived_sustained_rate':sustained if sustained is not None and sustained>0 else None,
            'derived_common_absolute_overload_rate':5*minimum if minimum is not None else None,
            'derived_rate_rule':'formal own point if qualified, otherwise qualified formal common point; no short-window substitution',
            'derived_sustained_not_executed_reason':('no_formally_verified_nonzero_capacity' if minimum is None else
                ('floor_60_percent_is_zero' if sustained==0 else None))}

def upper_point(rate, result, gate, report):
    reasons=gate.get('reasons',[])
    if result.get('valid_driver') is not True or any(reason in reasons for reason in
            ('invalid_driver_or_not_started','unreconciled_plan','driver_cpu_saturated')):
        kind,censored='driver_invalid_censored',True
    elif any(reason in reasons for reason in ('resource_endpoints_missing','resource_observation_invalid',
                'measurement_monotonic_clock_unavailable','measurement_monotonic_clock_uncertain')):
        kind,censored='measurement_invalid_censored',True
    elif gate.get('eligible_for_formal_candidate') is False:
        kind,censored='service_or_slo_failure',False
    else:
        kind,censored='unknown_gate_censored',True
    return {'rate':rate,'gate_kind':kind,'censored':censored,'valid_driver':result.get('valid_driver'),
            'qualification':gate,'report':str(report)}

def business_send_end(configuration):
    key='last_actual_sent_monotonic_ns'
    if key not in configuration:
        return {'status':'unknown_missing_client_field','monotonic_ns':None,'field':'configuration.'+key}
    value=configuration[key]
    if value is None:
        return {'status':'not_applicable_no_actual_business_send','monotonic_ns':None,'field':'configuration.'+key}
    clock=measurement_clock(configuration)
    if not clock['available']:
        return {'status':clock['reason'],'monotonic_ns':None,'field':'configuration.'+key}
    origin=clock['origin_ns']
    offset=configuration.get('last_actual_sent_offset_ns')
    if type(value) is not int or value<origin or type(offset) is not int or value-origin!=offset:
        return {'status':'unknown_invalid_or_inconsistent_client_field','monotonic_ns':None,'field':'configuration.'+key}
    return {'status':'observed','monotonic_ns':value,'offset_ns':offset,
            'unix_ns_display_only':configuration.get('last_actual_sent_unix_ns'),
            'anchor_uncertainty_ns':clock['uncertainty_ns'],'field':'configuration.'+key}

def recovery_probe_views(rows, probe_origin, starts, uncertainty_ns=0):
    views={name:{'origin_monotonic_ns':start,'deadline_seconds':5,'first_complete_new_probe':None,
                 'combined_anchor_uncertainty_ns':uncertainty_ns,'boundary_ambiguous_correct_probes':0,
                 'probe_phases':{'before_withdrawal':{},'within5s_after_withdrawal':{},'later':{}},
                 'last_observed_probe_plan_monotonic_ns':None}
           for name,start in starts.items() if start is not None}
    for row in rows:
        planned=probe_origin+int(row['scheduled_ns'])
        completed=probe_origin+int(row['completed_ns'])
        for value in views.values():
            start=value['origin_monotonic_ns']
            phase='before_withdrawal' if planned<start else ('within5s_after_withdrawal' if planned<start+5_000_000_000 else 'later')
            counts=value['probe_phases'][phase]
            counts[row['terminal']]=counts.get(row['terminal'],0)+1
            previous=value['last_observed_probe_plan_monotonic_ns']
            value['last_observed_probe_plan_monotonic_ns']=planned if previous is None else max(previous,planned)
            if row['terminal']=='correct' and planned>=start and completed<=start+5_000_000_000:
                if planned-uncertainty_ns<start or completed+uncertainty_ns>start+5_000_000_000:
                    value['boundary_ambiguous_correct_probes']+=1
                    continue
                sample={'rid':row['rid'],'planned_after_seconds':(planned-start)/1e9,
                        'complete_after_seconds':(completed-start)/1e9,
                        'complete_after_seconds_upper_bound':(completed-start+uncertainty_ns)/1e9}
                first=value['first_complete_new_probe']
                if first is None or sample['complete_after_seconds']<first['complete_after_seconds']:
                    value['first_complete_new_probe']=sample
    for value in views.values():
        first=value['first_complete_new_probe']
        last=value['last_observed_probe_plan_monotonic_ns']
        covered=last is not None and last-uncertainty_ns>=value['origin_monotonic_ns']+5_000_000_000
        value['observation_covers_full5s']=covered
        ambiguous=value['boundary_ambiguous_correct_probes']>0
        value['recovered']=True if first else (False if covered and not ambiguous else None)
        value['status']='recovered' if first else ('unknown_boundary_clock_uncertainty' if ambiguous else
                ('not_recovered_within5s' if covered else 'unknown_incomplete_probe_window'))
    return views

class Case:
    def __init__(self, harness, cores, memory, workload, args):
        self.h, self.cores, self.memory, self.workload, self.args = harness, cores, memory, workload, args
        self.stacks=declared_case_stacks(getattr(args,'stacks',STACKS),cores,memory,workload)
        if not self.stacks:
            raise RuntimeError('no selected stack belongs to this declared case matrix')
        self.path = harness.output / f'c{cores}-m{memory}-{workload}'
        self.path.mkdir(parents=True, exist_ok=True)
        self.instances = {}
        self.mode = 'persistent_frozen_between_turns'
        self.state_path = self.path / 'case.json'
        self.state = read_json(self.state_path) if self.state_path.exists() else {'cores': cores, 'memory': memory, 'workload': workload, 'capacity': {}, 'history': [], 'mode': self.mode}
        self.mode = self.state.get('mode', self.mode)
    def save(self):
        self.state['mode'] = self.mode
        write_json(self.state_path, self.state)
    def apply_verified(self, summaries):
        derived=derive_verified_rates(self.state,summaries,self.stacks)
        self.state.update(derived)
        self.state['verified_summary_sha256']=hashlib.sha256((self.path/'formal-summary.json').read_bytes()).hexdigest()
        self.save()
        return derived
    def require_verified(self):
        path=self.path/'formal-summary.json'
        if not path.exists():
            self.formal()
        # formal writes an explicit unexecuted summary when no nonzero short
        # candidate exists. It never substitutes a zero-rate window.
        if not path.exists():
            raise RuntimeError('formal summary unavailable; cannot choose sustained or overload rates')
        return self.apply_verified(read_json(path))
    def check_memory(self):
        available=guest_memory()['MemAvailable_bytes']
        self.state['guest_available_memory_last']=available
        if self.mode!='persistent_frozen_between_turns' or available>=800*MIB:
            return
        for service in self.instances.values():
            if not service.stopped:
                service.stop()
        self.mode='restart_each_formal_turn'
        invalid=self.path/'invalid-before-uniform-memory-fallback'/str(time.time_ns())
        moved=[]
        for path in list(self.path.iterdir()):
            if path.name.startswith(('formal-','overload-')) or path.name=='sustained':
                invalid.mkdir(parents=True,exist_ok=True)
                path.rename(invalid/path.name)
                moved.append(path.name)
        self.state.pop('formal_status',None)
        for key in list(self.state):
            if key.startswith(('verified_','derived_')) or key in ('unverified_stacks','sustained_status','sustained_scope','overload_status','overload_scope'):
                self.state.pop(key,None)
        self.state['history'].append({'event':'uniform_restart_fallback','guest_available_memory_bytes':available,
                                      'invalidated_prior_phase_paths':moved,'preserved_at':str(invalid) if moved else None,
                                      'reason':'guest available below800MiB; re-run whole case uniformly with fresh30s warmup'})
        self.save()
        raise UniformRestart('case switched uniformly to fresh instances after measured guest memory pressure')
    def activate(self, stack):
        self.check_memory()
        for name, service in self.instances.items():
            if name != stack and not service.stopped and not service.domain.frozen:
                self.idle(name)
        if stack not in self.instances or self.instances[stack].stopped:
            generation = 1 + sum(1 for h in self.state['history'] if h.get('stack') == stack and h.get('event') == 'start')
            service = self.h.start(stack, self.cores, self.memory, self.path / 'services' / f'{stack}-{generation:03d}')
            self.instances[stack] = service
            self.state['history'].append({'event': 'start', 'stack': stack, 'generation': generation, 'wall_time_ns': time.time_ns()})
            self.save()
            self.check_memory()
            warm_rate = 10 if self.workload == 'cpu' else 100
            self.h.client(service, self.workload, warm_rate, self.args.warmup,
                          self.path / 'warmups' / f'{stack}-{generation:03d}', events=False,idle_ms=self.args.keepalive_idle_ms)
        service = self.instances[stack]
        if service.domain.frozen:
            service.freeze(False)
        return service
    def idle(self, stack, report_path=None):
        service = self.instances.get(stack)
        if service and not service.stopped:
            # Each client closed all its connections; snapshot the remaining
            # runtime bookkeeping before freeze without inventing equivalent
            # native metrics layouts.
            settle_start=time.monotonic()
            deadline=settle_start+self.args.physical_settle_seconds
            value=None
            settle_samples=[]
            fixture_sample=None
            last_sample=settle_start-1
            while time.monotonic() < deadline:
                try:
                    value = service.metrics()
                except Exception as error:
                    value = {'observation_error': repr(error)}
                    break
                now=time.monotonic()
                if now-last_sample>=.5 and len(settle_samples)<128:
                    settle_samples.append({'seconds_after_driver':now-settle_start,'resources':service.domain.snapshot(),'metrics':value})
                    last_sample=now
                if now-settle_start>=5 and fixture_sample is None:
                    fixture_sample={'seconds_after_driver':now-settle_start,'resources':service.domain.snapshot(),'metrics':value}
                if quiet_metrics(stack, value):
                    break
                time.sleep(.05)
            self.state['history'].append({'event': 'before_freeze', 'stack': stack,
                                          'wall_time_ns': time.time_ns(), 'metrics': value})
            settle={'fixture5s_passed':time.monotonic()-settle_start<=5 and quiet_metrics(stack,value),
                    'observation_allowance_seconds_after_driver':self.args.physical_settle_seconds,
                    'actual_observation_seconds':time.monotonic()-settle_start,
                    'fixture5s_sample':fixture_sample,'samples':settle_samples,'sample_limit':128,
                    'contract':'5s is fixture quiescence gate only; additional bounded observation does not change runtime request30s/handler45s deadlines, DB lease or refund responsibility',
                    'runtime_deadline_environment':{k:v for k,v in service.startup.get('environment',{}).items() if 'TIMEOUT' in k or 'DEADLINE' in k}}

            if 'observation_error' in value or not quiet_metrics(stack,value):
                termination=service.stop()
                if report_path and Path(report_path).exists():
                    report=read_json(report_path)
                    report['service_cost']['full_lifecycle']=full_lifecycle_cost(report,termination['after'],False,termination,value)
                    report['service_cost']['full_lifecycle']['settle_observation']=settle
                    report['service_cost']['complete_lifecycle_accounted']=True
                    write_json(report_path,report)
                self.save()
                raise RuntimeError(f'{stack} did not reach physical quiescence within unchanged-runtime observation allowance; see retained lifecycle cost')
            end=service.domain.snapshot()
            termination=None
            if self.mode == 'restart_each_formal_turn':
                termination=service.stop()
                end=termination['after']
            if report_path and Path(report_path).exists():
                report=read_json(report_path)
                report['service_cost']['full_lifecycle']=full_lifecycle_cost(report,end,True,termination,value)
                report['service_cost']['full_lifecycle']['settle_observation']=settle
                report['service_cost']['complete_lifecycle_accounted']=True
                write_json(report_path,report)
            if self.mode != 'restart_each_formal_turn':
                service.freeze(True)
            self.save()
            self.check_memory()
    def observe(self, stack, rate, duration, name, events=True, prime=True, idle_after=True):
        directory = self.path / name / stack
        report = directory / 'client.json'
        if report.exists() and 'service_cost' in read_json(report):
            previous=read_json(report)
            configuration=previous.get('configuration',{})
            if previous.get('schema')!='ku-cross-stack-client-v5' or previous.get('measurement_identity',{}).get('harness_revision')!=5:
                raise RuntimeError('completed window belongs to another driver/harness version; preserve it and choose new results root')
            if configuration.get('workload')!=self.workload or configuration.get('rate')!=rate or configuration.get('duration_seconds')!=duration:
                raise RuntimeError(f'completed window load differs from current frozen formal basis: {report}')
            return previous
        service = self.activate(stack)
        if prime:
            self.h.client(service, self.workload, max(1, min(rate, 100)), self.args.prime,
                          directory / 'prime', events=False,idle_ms=self.args.keepalive_idle_ms)
        print(json.dumps({'event': 'window', 'case': self.path.name, 'stack': stack,
                          'name': name, 'rate': rate, 'seconds': duration}), flush=True)
        try:
            result = self.h.client(service, self.workload, rate, duration, directory, events=events,
                                   runtime_metrics=name == 'sustained',idle_ms=self.args.keepalive_idle_ms)
            result['qualification'] = qualification(result, self.workload)
            write_json(report, result)
            return result
        finally:
            if idle_after:
                self.idle(stack,report)
    def explore(self):
        for stack in self.stacks:
            if stack in self.state['capacity']:
                continue
            start = 4 if self.workload == 'cpu' else 16
            rate, low, high = start, 0, None
            high_record,high_history=None,[]
            observations = []
            for index in range(18):
                result = self.observe(stack, rate, self.args.explore_seconds, f'explore-{index:02d}', prime=index > 0)
                gate = result['qualification']
                report=self.path / f'explore-{index:02d}' / stack / 'client.json'
                observations.append({'rate': rate, 'qualification': gate, 'valid_driver':result.get('valid_driver'),'report':str(report)})
                if gate['eligible_for_formal_candidate']:
                    low = rate
                    if high is not None:
                        break
                    if rate >= self.args.maximum_rate:
                        break
                    rate = min(self.args.maximum_rate, rate * 2)
                else:
                    high = rate
                    high_record=upper_point(rate,result,gate,report)
                    high_history.append(high_record)
                    if low == 0 and rate > 1:
                        rate = max(1,rate//2)
                        continue
                    break
            refinements = []
            if high and low and high_record['gate_kind']=='service_or_slo_failure':
                for index in range(3):
                    rate = max(low + 1, (low + high) // 2)
                    if rate >= high:
                        break
                    result = self.observe(stack, rate, self.args.explore_seconds, f'refine-{index:02d}')
                    report=self.path/f'refine-{index:02d}'/stack/'client.json'
                    refinements.append({'rate': rate, 'qualification': result['qualification'],
                                        'valid_driver':result.get('valid_driver'),'report':str(report)})
                    if result['qualification']['eligible_for_formal_candidate']:
                        low = rate
                    else:
                        high = rate
                        high_record=upper_point(rate,result,result['qualification'],report)
                        high_history.append(high_record)
            self.state['capacity'][stack] = {'candidate_rate': low, 'next_failed_or_invalid_rate': high,
                                             'observations': observations, 'refinements': refinements,
                                             'claim': 'short-window candidate; formal seven-round and tail checks still required',
                                             'upper_bound_kind': high_record['gate_kind'] if high_record else 'censored_no_failed_or_invalid_upper_point',
                                             'upper_bound_censored':high_record['censored'] if high_record else True,
                                             'upper_bound_point':high_record,'upper_bound_history':high_history}
            self.save()
        if 'common_rate' not in self.state:
            self.state['common_rate'] = max(1, math.floor(.6 * min(v['candidate_rate'] for v in self.state['capacity'].values() if v['candidate_rate'] > 0))) if any(v['candidate_rate'] > 0 for v in self.state['capacity'].values()) else None
        self.state['partial_stacks'] = [s for s in self.stacks if self.state['capacity'][s]['candidate_rate'] > 0]
        self.save()
        # Uniform fallback is decided before formal rounds. Do not quietly
        # change only one stack's warmup/history if total resident memory rises.
        self.state['guest_available_memory_before_formal']=guest_memory()['MemAvailable_bytes']
        self.check_memory()
        self.save()
        return self.state
    def diagnose(self):
        if not self.args.diagnostic_rates:
            raise RuntimeError('diagnose requires exactly3 predeclared low/mid/high --diagnostic-rates')
        for idle_ms in (500,0):
            for rate in self.args.diagnostic_rates:
                for stack in self.stacks:
                    service=self.activate(stack)
                    directory=self.path/f'diagnose-idle{idle_ms}-rate{rate}'/stack
                    try:
                        result=self.h.client(service,self.workload,rate,self.args.explore_seconds,directory,idle_ms=idle_ms)
                        result['qualification']=qualification(result,self.workload)
                        result['diagnostic_scope']='three predeclared points and explicit idle-policy A/B; not a formal capacity/causality conclusion'
                        write_json(directory/'client.json',result)
                        if not result['qualification']['eligible_for_formal_candidate']:
                            raise RuntimeError('first control diagnostic anomaly retained; stop and analyze before more collection')
                    finally:
                        self.idle(stack,directory/'client.json')

    def formal(self):
        self.explore()
        common = self.state['common_rate']
        if common is None:
            self.state['formal_status'] = 'no_nonzero_eligible_candidate'
            summaries={view:{s:{'qualified':False,'status':'not_executed_no_nonzero_short_candidate'} for s in self.stacks}
                       for view in ('common','own_capacity')}
            summaries['paired_capacity_goodput']=paired_capacity_goodput(summaries,self.state,seed=self.args.random_seed)
            write_json(self.path/'formal-summary.json',summaries)
            self.apply_verified(summaries)
            return
        for viewpoint in ('common', 'own_capacity'):
            # Extend every stack uniformly for rare1% mixed CPU tails. No mixture change.
            rates=[common] if viewpoint=='common' else [v['candidate_rate'] for v in self.state['capacity'].values() if v['candidate_rate']>0]
            share=.01 if self.workload=='mixed' else 1
            formal_duration=max(30,self.args.formal_seconds,math.ceil(1000/(min(rates)*share*self.args.formal_repeats)))
            if formal_duration*max(rates)>10000000:
                raise RuntimeError('frozen uniform formal window exceeds10million driver plans; split separately declared capacity points without merging driver versions')
            if formal_duration>3600:
                raise RuntimeError('1000 core tail samples require >3600s/window; point remains unmeasured, choose a predeclared higher valid common rate')
            self.state.setdefault('formal_design',{}).update({'repeats':self.args.formal_repeats,'seed':self.args.random_seed,'orders':interleaved_orders(self.stacks,self.args.formal_repeats,self.args.random_seed),'minimum_core_samples':1000})
            self.state['formal_design'][viewpoint+'_seconds']=formal_duration
            self.save()
            for repeat,order in enumerate(interleaved_orders(self.stacks,self.args.formal_repeats,self.args.random_seed)):
                for stack in order:
                    rate = common if viewpoint == 'common' else self.state['capacity'][stack]['candidate_rate']
                    if rate <= 0:
                        continue
                    self.observe(stack, rate, formal_duration, f'formal-{viewpoint}-{repeat:02d}')
        self.check_memory()
        summaries={}
        for viewpoint in ('common','own_capacity'):
            summaries[viewpoint]={}
            for stack in self.stacks:
                paths=[self.path/f'formal-{viewpoint}-{i:02d}'/stack/'client.json' for i in range(self.args.formal_repeats)]
                if not all(path.exists() for path in paths):
                    summaries[viewpoint][stack]={'qualified':False,'status':'not_executed_no_nonzero_short_candidate'}
                else:
                    summaries[viewpoint][stack]=aggregate_rounds([read_json(path) for path in paths],self.workload)
        paired={}
        for reference in ('go','rust'):
            if 'ku' not in self.stacks or reference not in self.stacks:
                continue
            reports={stack:[read_json(self.path/f'formal-common-{i:02d}'/stack/'client.json') for i in range(self.args.formal_repeats)] for stack in ('ku',reference)}
            pairs={}
            for metric in ('goodput_rps','cpu_seconds_per_correct','steady_median_bytes','observed_peak_bytes'):
                def values(stack):
                    if metric=='goodput_rps': return [r['ledger'][metric] for r in reports[stack]]
                    if metric=='cpu_seconds_per_correct': return [r['service_cost'].get('full_lifecycle',{}).get(metric) for r in reports[stack]]
                    return [r['service_cost'].get('arrival_window_memory',{}).get(metric) for r in reports[stack]]
                pairs[metric]=paired_interval(values('ku'),values(reference),self.args.random_seed)
            kinds=('json_object','db','timer','cpu') if self.workload=='mixed' else (self.workload,)
            for kind in kinds:
                def tails(stack):
                    return [(r['by_type'].get(kind,{}) if self.workload=='mixed' else r['ledger']).get('correct_scheduled_to_complete',{}).get('p99_ms') for r in reports[stack]]
                pairs['p99_ratio_'+kind]=paired_interval(tails('ku'),tails(reference),self.args.random_seed)
            pairs['targets']={'common_goodput_ratio_lower95_minimum':1.0,'lifecycle_cpu_cost_ratio_upper95_maximum':.9,
                'steady_and_peak_memory_ratio_upper95_maximum':.8,'p99_ratio_upper95_noninferiority_maximum':1.05,
                'quantile_resolution_note':'upper-edge buckets10us minimum/<=1/128 relative; differences within quantified bucket/clock error cannot establish a precise ranking'}
            pairs['eligible_for_target_claim']=all(summaries['common'][stack]['qualified'] for stack in ('ku',reference)) and all(r['service_cost'].get('full_lifecycle',{}).get('natural_reclamation_passed') is True for stack in ('ku',reference) for r in reports[stack])
            pairs['note']='common-rate ratios only; capacity ratio requires separately credible curves. Any failed gate remains visible and prevents target claim.'
            paired[reference]=pairs
        summaries['paired_common_costs']=paired
        summaries['paired_capacity_goodput']=paired_capacity_goodput(summaries,self.state,seed=self.args.random_seed)
        write_json(self.path/'formal-summary.json',summaries)
        self.state['formal_status'] = 'all_planned_windows_recorded; see per-window gates'
        self.apply_verified(summaries)
    def sustained(self):
        if self.args.sustained_seconds<600:
            raise RuntimeError('formal sustained windows require at least600seconds; no short diagnostic substitute')
        derived=self.require_verified()
        rate = derived['derived_sustained_rate']
        self.state['sustained_scope']={'included':derived['verified_partial_stacks'],
                                      'excluded':derived['unverified_stacks'],'rate':rate,
                                      'cores':self.cores,'memory':self.memory,'workload':self.workload,
                                      'duration_seconds':self.args.sustained_seconds,
                                      'basis_summary_sha256':self.state['verified_summary_sha256']}
        self.save()
        if not rate:
            self.state['sustained_status'] = 'not_executed_'+derived['derived_sustained_not_executed_reason']
        else:
            for stack in derived['verified_partial_stacks']:
                self.observe(stack, rate, self.args.sustained_seconds, 'sustained')
            self.state['sustained_status'] = 'recorded at60percent of minimum formally verified nonzero rate; see partial scope'
        self.save()
    def overload(self):
        derived=self.require_verified()
        self.state['overload_scope']={'included':derived['verified_partial_stacks'],'excluded':derived['unverified_stacks'],
                                     'basis_summary_sha256':self.state['verified_summary_sha256']}
        self.save()
        if not derived['verified_partial_stacks']:
            self.state['overload_status']='not_executed_no_formally_verified_nonzero_capacity'
            self.save()
            return
        for kind,multiple in (('relative-1.5',1.5),('relative-2',2),('relative-5',5),('relative-10',10),('relative-20',20),('relative-40',40),('common_absolute',None)):
            for stack in derived['verified_partial_stacks']:
                prior=self.state.setdefault('overload_escalation',{}).get(stack)
                if multiple and multiple>5 and (prior is None or prior['boundary_observed'] or prior['status'] in ('measurement_invalid_censored','service_fault_without_capacity_attribution') or prior.get('rate_was_capped')):
                    continue
                directory = self.path / ('overload-' + kind) / stack
                own = derived['verified_rates'][stack]
                wanted = math.ceil(multiple*own) if multiple else derived['derived_common_absolute_overload_rate']
                rate = min(self.args.maximum_rate, wanted)
                basis={'verified_rate':own,'formal_summary_sha256':self.state['verified_summary_sha256'],
                       'kind':kind,'actual_arrival_rate':rate}
                if (directory / 'recovery.json').exists():
                    if read_json(directory/'recovery.json').get('verified_rate_basis')!=basis:
                        raise RuntimeError('completed overload window has a different or missing formal verified-rate basis; preserve old evidence')
                    continue
                service = self.activate(stack)
                self.h.client(service,self.workload,min(100,rate),self.args.prime,directory/'prime',events=False)
                probe_result, probe_error = [], []
                def run_probes():
                    try:
                        probe_result.append(self.h.client(service,'ok',10,self.args.formal_seconds+8,
                                                          directory/'independent-probes',concurrency=16,timeout=1))
                    except Exception as error:
                        probe_error.append(repr(error))
                thread = threading.Thread(target=run_probes,daemon=True)
                thread.start()
                try:
                    result = self.h.client(service,self.workload,rate,self.args.formal_seconds,directory)
                    result['qualification']=qualification(result,self.workload)
                    boundary=overload_boundary(result,self.workload)
                    boundary['rate_was_capped']=rate!=wanted
                    self.state['overload_escalation'][stack]=boundary
                    self.save()
                    write_json(directory/'client.json',result)
                    thread.join(timeout=self.args.formal_seconds+15)
                    if thread.is_alive():
                        raise RuntimeError('independent probes did not finish within bounded deadline')
                    if probe_error:
                        raise RuntimeError('independent probe driver failure: '+probe_error[0])
                    probes=probe_result[0]
                    business_clock=measurement_clock(result['configuration'])
                    probe_clock=measurement_clock(probes['configuration'])
                    sent_end=business_send_end(result['configuration'])
                    cutoff=None
                    views={}
                    if business_clock['available'] and probe_clock['available']:
                        cutoff=business_clock['origin_ns']+int(self.args.formal_seconds*1e9)
                        with gzip.open(probes['events']['path'],'rt',newline='') as source:
                            views=recovery_probe_views(csv.DictReader(source),probe_clock['origin_ns'],
                                {'planned_cutoff':cutoff,'actual_business_send_end':sent_end['monotonic_ns']},
                                business_clock['uncertainty_ns']+probe_clock['uncertainty_ns'])
                    primary=views.get('planned_cutoff',{'status':'unknown_monotonic_clock',
                        'recovered':None,'first_complete_new_probe':None,'probe_phases':{},
                        'business_clock':business_clock,'probe_clock':probe_clock})
                    actual=views.get('actual_business_send_end',{'status':sent_end['status'],
                        'origin_monotonic_ns':None,'deadline_seconds':5,'recovered':None,'first_complete_new_probe':None})
                    actual['role']='additional view only; never replaces or widens the planned-cutoff5s gate'
                    tail=(sent_end['monotonic_ns']-cutoff)/1e9 if cutoff is not None and sent_end['monotonic_ns'] is not None else None
                    write_json(directory/'recovery.json',{'overload_boundary':boundary, 'recovery_after_genuine_overload_passed':boundary['boundary_observed'] and primary['recovered'] is True and probes['valid_driver'], 'recovered':primary['recovered'],'first_complete_new_probe':primary['first_complete_new_probe'],
                        'origin':'business arrival cutoff = Linux CLOCK_MONOTONIC origin + frozen duration; anchor uncertainty conservatively included',
                        'business_cutoff_monotonic_ns':cutoff,'deadline_seconds':5,'probe_phases':primary['probe_phases'],
                        'clock_scope':'all recovery gates and resource correlation use CLOCK_MONOTONIC; Unix values are display-only and may step',
                        'planned_cutoff_recovery_view':primary,
                        'last_actual_business_sent_monotonic_ns':sent_end['monotonic_ns'],
                        'last_actual_business_sent_unix_ns_display_only':result['configuration'].get('last_actual_sent_unix_ns'),
                        'last_actual_business_sent_status':sent_end['status'],
                        'last_actual_business_sent_evidence':sent_end,
                        'actual_send_tail_seconds_relative_to_planned_cutoff':tail,
                        'continued_business_sends_after_planned_cutoff':tail>0 if tail is not None else None,
                        'actual_business_send_end_recovery_view':actual,
                        'probe_report':str(directory/'independent-probes'/'client.json'),
                        'actual_overload_multiple':rate/own,'requested_rate':wanted,'actual_rate':rate,
                        'rate_was_capped':rate!=wanted,'probe_driver_valid':probes['valid_driver'],
                        'verified_rate_basis':basis,
                        'probe_is_ordinary_data_plane':'same /ok handler, not an isolated management guarantee',
                        'all_probe_failures_retained':True})
                finally:
                    thread.join(timeout=self.args.formal_seconds+15)
                    self.idle(stack,directory/'client.json')
        self.state['overload_status']='1.5x/2x/5x and bounded10x/20x/40x escalation; only observed valid saturation earns genuine-overload recovery claim; all outcomes retained'
        self.save()
    def close(self):
        for service in self.instances.values():
            if not service.stopped:
                service.stop()


def cases(args):
    for workload in ('ok', 'json_object', 'json_array', 'json_escape', 'cpu', 'db', 'mixed'):
        yield 4, GIB, workload
    for workload in ('ok', 'json_object', 'cpu'):
        yield 1, GIB, workload
    yield 2, GIB, 'cpu'
    yield 2, GIB, 'json_object'
    yield 4, 256 * MIB, 'json_object'
    yield 4, 256 * MIB, 'mixed'

class UniformRestart(RuntimeError):
    """A single bounded, whole-case policy transition; original data retained."""
    pass

def child(arguments):
    group, uid, gid, cwd, separator, *command = arguments
    if separator != '--' or not command:
        raise RuntimeError('invalid exec shim arguments')
    (Path(group) / 'cgroup.procs').write_text(str(os.getpid()))
    if (Path(group) / 'cpuset.cpus.effective').exists():
        cpus = set()
        for part in (Path(group) / 'cpuset.cpus.effective').read_text().strip().split(','):
            if '-' in part:
                lo, hi = map(int, part.split('-'))
                cpus.update(range(lo, hi+1))
            elif part:
                cpus.add(int(part))
        if cpus:
            os.sched_setaffinity(0, cpus)
    os.setgroups([])
    os.setgid(int(gid))
    os.setuid(int(uid))
    os.chdir(cwd)
    os.execvpe(command[0], command, os.environ)

def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--root', type=Path, required=True)
    parser.add_argument('--output', type=Path, required=True)
    parser.add_argument('--phase', choices=('smoke','diagnose','explore','formal','waiting','batch','overload','sustained','all'), default='smoke')
    parser.add_argument('--case', help='optional exact c4-m1073741824-json_object name')
    parser.add_argument('--warmup', type=float, default=30)
    parser.add_argument('--prime', type=float, default=3)
    parser.add_argument('--explore-seconds', type=float, default=5)
    parser.add_argument('--formal-seconds', type=float, default=30)
    parser.add_argument('--sustained-seconds', type=float, default=600)
    parser.add_argument('--maximum-rate', type=int, default=100000)
    parser.add_argument('--formal-repeats',type=int,default=7)
    parser.add_argument('--random-seed',type=int,default=20261023)
    parser.add_argument('--diagnostic-rates',type=int,nargs=3)
    parser.add_argument('--keepalive-idle-ms',type=int,default=500)
    parser.add_argument('--physical-settle-seconds',type=float,default=45,help='bounded observation after driver exit; does not alter service deadlines; fixture5s gate remains separate')
    parser.add_argument('--stacks',nargs='+',choices=(*STACKS,'java','dotnet','cpp','control'),default=list(STACKS))
    args = parser.parse_args()
    if args.phase=='sustained':
        try:
            sustained_case(args)
        except ValueError as error:
            parser.error(str(error))
    if args.phase in ('sustained','all') and args.sustained_seconds<600:
        parser.error('formal sustained windows require at least600seconds')
    if 'control' in args.stacks and args.case not in ('c1-m1073741824-ok','c4-m1073741824-ok'):
        parser.error('control diagnosis requires an explicit supported /ok case')
    if args.phase!='diagnose' and 'control' in args.stacks:
        parser.error('control is diagnosis only, never included in language rankings')
    if not 5<=args.physical_settle_seconds<=120:
        parser.error('physical observation must be5..120seconds; no runtime deadline change')
    if not 0<=args.keepalive_idle_ms<=60000 or (args.diagnostic_rates and (min(args.diagnostic_rates)<=0 or sorted(set(args.diagnostic_rates))!=args.diagnostic_rates)):
        parser.error('idle timeout out of range or diagnostic rates not3 increasing positive points')
    if args.formal_repeats<7 or len(set(args.stacks))!=len(args.stacks):
        parser.error('at least7 paired windows and unique stack names required')
    if min(args.warmup,args.prime,args.explore_seconds,args.formal_seconds,args.sustained_seconds) <= 0:
        parser.error('durations must be positive; deviations must be marked diagnostic')
    harness = Harness(args.root, args.output)
    configuration = {'phase': args.phase, 'arguments': vars(args), 'manifest_sha256': hashlib.sha256((args.root/'manifest.json').read_bytes()).hexdigest(),
                     'protocol_sha256': hashlib.sha256(harness.protocol.read_bytes()).hexdigest(),
                     'formal_rules': 'at least7 seeded interleaved paired windows; >=30s and1000 pooled core samples/type; 30s warmup/3s prime',
                     'coordinator': {'pid': os.getpid(), 'inside_service_domain': False},
                     'rlimit_nofile': list(resource.getrlimit(resource.RLIMIT_NOFILE)),
                     'identity': {'uname': list(os.uname()), 'cpu_online': Path('/sys/devices/system/cpu/online').read_text().strip(),
                                  'cpuinfo': Path('/proc/cpuinfo').read_text(), 'kernel_commandline': Path('/proc/cmdline').read_text().strip()}}
    configuration['arguments'] = {k: str(v) if isinstance(v,Path) else v for k,v in vars(args).items()}
    freeze={'manifest_sha256':configuration['manifest_sha256'],'protocol_sha256':configuration['protocol_sha256'],
            'warmup':args.warmup,'prime':args.prime,'formal_seconds':args.formal_seconds,
            'explore_seconds':args.explore_seconds,'sustained_seconds':args.sustained_seconds,'maximum_rate':args.maximum_rate,
            'formal_repeats':args.formal_repeats,'random_seed':args.random_seed,'stacks':args.stacks,'keepalive_idle_ms':args.keepalive_idle_ms,'diagnostic_rates':args.diagnostic_rates,'physical_settle_seconds':args.physical_settle_seconds}
    freeze_path=args.output/'freeze.json'
    if freeze_path.exists() and read_json(freeze_path)!=freeze:
        raise RuntimeError('resume identity/configuration changed; choose a new output directory and preserve prior data')
    write_json(freeze_path,freeze)
    write_json(args.output/'invocation.json', configuration)
    write_json(args.output/'invocations'/f'{time.time_ns()}-{args.phase}.json',configuration)
    try:
        if args.phase == 'smoke':
            for stack in args.stacks:
                service = harness.start(stack, output=args.output/'smoke'/stack/'service')
                try:
                    for workload in ('ok','json_object','json_array','json_escape','cpu','timer','db'):
                        result = harness.client(service,workload,4,2,args.output/'smoke'/stack/workload,concurrency=8)
                        if result['ledger']['correct_total'] != result['ledger']['planned']:
                            raise RuntimeError(f'golden failure {stack}/{workload}')
                finally:
                    service.stop()
        if args.phase in ('diagnose','explore','formal','overload','all'):
            for cores,memory,workload in cases(args):
                name = f'c{cores}-m{memory}-{workload}'
                if args.phase=='overload' and (cores!=4 or memory!=GIB or workload!='mixed'):
                    continue
                if args.case and args.case != name:
                    continue
                if not declared_case_stacks(args.stacks,cores,memory,workload):
                    continue
                case = Case(harness,cores,memory,workload,args)
                try:
                    for attempt in range(2):
                        try:
                            if args.phase == 'diagnose':
                                case.diagnose()
                            elif args.phase == 'explore':
                                case.explore()
                            elif args.phase=='overload':
                                case.overload()
                            else:
                                case.formal()
                            if args.phase == 'all' and workload == 'mixed' and cores == 4:
                                case.overload()
                            if args.phase == 'all' and workload == 'mixed' and cores == 4:
                                case.sustained()
                            break
                        except UniformRestart:
                            if attempt:
                                raise
                            print(json.dumps({'event':'uniform_memory_fallback','case':case.path.name}),flush=True)
                finally:
                    case.close()
        if args.phase == 'sustained':
            case = Case(harness,*sustained_case(args),args)
            try:
                for attempt in range(2):
                    try:
                        case.sustained()
                        break
                    except UniformRestart:
                        if attempt:
                            raise
            finally:
                case.close()
        if args.phase in ('waiting','all'):
            import waiting
            waiting.run(harness)
        if args.phase in ('batch','all'):
            import batch
            batch.run(harness)
    finally:
        write_json(args.output/'last-cleanup.json', {'services': harness.close(), 'wall_time_ns': time.time_ns()})

if __name__ == '__main__':
    if len(sys.argv) > 1 and sys.argv[1] == '__child':
        child(sys.argv[2:])
    else:
        main()
