#!/usr/bin/env python3
"""Bounded Linux thread attribution, diagnostic only; no compiler/service launcher.

Prepare from an ordinary CLI Release emission, then link separately:
  python3 http_cost_diagnostic.py prepare server.c server-roles.c --source-commit SHA
  clang-18 server-roles.c -std=c11 -O2 -g -pthread /absolute/libpq.so \
      -Wl,--wrap=pthread_create -o server-roles
Keep the normal CLI binary for before/after controls and all scoring. The footer
can retain functions/change layout; its prefix identity is not .text identity.

Observe the already-started service (prefer its whole service cgroup):
  python3 http_cost_diagnostic.py sample --pid PID --cgroup /sys/fs/cgroup/owned \
      --seconds 15 --interval 0.1 --output thread-cost.json
SIGINT/SIGTERM asks this observer to save a final snapshot and exit; it never
forwards that signal to the observed service. The duration remains an upper bound.
No process is launched, signalled, frozen or stopped. Raw samples include each
live TID; deltas require identical process/thread start times at both endpoints.
Exited/new/missing threads remain unknown, not zero. Cgroup CPU from the existing
harness is the whole-domain denominator; these thread sums cannot replace it.
Instantaneous states, faults and syscall counts do not identify CPU hotspots,
I/O duration or swap ownership. Use independent perf/strace/scheduler evidence.
"""
from __future__ import annotations
import argparse
import hashlib
import json
import math
import os
from pathlib import Path
import re
import sys
import time

ROLE_ENTRIES = {
    'HTTP_WORKER': ('ku_http_worker', 'kuS', 'http_sync_handler'),
    'HTTP_OWNER': ('ku_http_loop_thread', 'kuO', 'http_owner'),
    'TASK_BLOCKING': ('ku_task_blocking_thread', 'kuB', 'task_blocking'),
    'TASK_DRIVER': ('ku_task_driver_thread', 'kuD', 'task_driver'),
    'OUTPUT': ('ku_output_thread', 'kuW', 'output'),
}
ROLE_CODES = {code: role for _, code, role in ROLE_ENTRIES.values()}
ROLE_CODES['kuU'] = 'unknown_wrapped_entry'
MAX_READ = 65536


def read_text(path: Path) -> str:
    with path.open('rb') as stream:
        data = stream.read(MAX_READ + 1)
    if len(data) > MAX_READ:
        raise ValueError(f'bounded proc read exceeded: {path.name}')
    return data.decode('utf-8', errors='replace')


def write_json(path: Path, data: dict) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    temporary = path.with_name(path.name + '.writing')
    temporary.write_text(json.dumps(data, ensure_ascii=False, indent=2) + '\n')
    temporary.replace(path)


def prepare_source(source: Path, destination: Path, source_commit: str | None = None) -> dict:
    if source.resolve() == destination.resolve():
        raise ValueError('diagnostic output must not overwrite the normal emission')
    identity_path = destination.with_suffix(destination.suffix + '.identity.json')
    if destination.exists() or identity_path.exists():
        raise FileExistsError('preserve prior diagnostic files; choose a fresh destination')
    if source.stat().st_size > 32 * 1024 * 1024:
        raise ValueError('generated C exceeds diagnostic input bound')
    original = source.read_bytes()
    text = original.decode('utf-8')
    definitions = {}
    for key, (symbol, code, role) in ROLE_ENTRIES.items():
        pattern = r'\bstatic\s+void\s*\*\s*' + re.escape(symbol) + r'\s*\([^;{}]*\)\s*\{'
        definitions[key] = {'symbol': symbol, 'comm_code': code, 'role': role,
                            'definition_present': re.search(pattern, text) is not None}
    footer = Path(__file__).with_name('http_thread_roles.c.inc').read_bytes()
    gates = '\n'.join('#define KU_COST_HAS_' + key + ' ' +
                      str(int(item['definition_present'])) for key, item in definitions.items())
    output = original + b'\n/* BEGIN DIAGNOSTIC FOOTER: NOT A SCORE */\n' + gates.encode() + b'\n' + footer
    destination.parent.mkdir(parents=True, exist_ok=True)
    destination.write_bytes(output)
    receipt = {
        'schema': 'ku-http-role-emission-v1', 'diagnostic_only': True,
        'source_commit': source_commit, 'normal_generated_c': str(source),
        'normal_prefix_bytes': len(original),
        'normal_prefix_sha256': hashlib.sha256(original).hexdigest(),
        'diagnostic_c_sha256': hashlib.sha256(output).hexdigest(),
        'footer_sha256': hashlib.sha256(footer).hexdigest(), 'definitions': definitions,
        'link_requirement': '-Wl,--wrap=pthread_create; same ordinary C11 O2/runtime libs, plus symbols',
        'roles': 'Static entry pointer match. Tagged comm includes actual startup TID; inheritance mismatch is unknown.',
        'limitations': 'Only ELF references are wrapped, not pthread_create inside DSOs. Naming failure stays unknown. '
                       'Function references can retain dead code/change layout; no claim of normal .text equality. '
                       'Observer and target must share a PID namespace for TID tags to match.',
    }
    write_json(identity_path, receipt)
    return receipt


def parse_stat(text: str) -> dict:
    # comm may itself contain spaces or parentheses. Fields below start at #3.
    left, right = text.index('('), text.rindex(')')
    fields = text[right + 1:].split()
    if len(fields) <= 36:
        raise ValueError('short proc stat')
    return {'id': int(text[:left].strip()), 'comm': text[left + 1:right],
            'state': fields[0], 'ppid': int(fields[1]),
            'user_ticks': int(fields[11]), 'system_ticks': int(fields[12]),
            'minor_faults': int(fields[7]), 'major_faults': int(fields[9]),
            'start_ticks': int(fields[19]), 'last_cpu': int(fields[36])}


def thread_role(pid: int, tid: int, comm: str) -> tuple[str, str]:
    match = re.fullmatch(r'(ku[SOBDWU]):([1-9][0-9]*)', comm)
    if match and int(match[2]) == tid:
        return ROLE_CODES[match[1]], 'entry_pointer_tag_and_matching_tid'
    if tid == pid:
        return 'process_main', 'kernel_tgid_equals_tid'
    return 'unknown', 'unwrapped_or_inherited_or_unavailable_tag'


def snapshot(root_pid: int, cgroup: Path | None = None, *, proc: Path = Path('/proc'),
             max_processes: int = 64, max_threads: int = 4096) -> dict:
    result = {'monotonic_ns': time.monotonic_ns(), 'unix_ns': time.time_ns(),
              'processes': [], 'threads': [], 'issues': [], 'complete': True}
    def issue(message):
        result['complete'] = False
        if len(result['issues']) < 64:
            result['issues'].append(message)
    pending = [(root_pid, None)]
    def enqueue(items, parent):
        for value in items:
            if len(pending) >= max_processes:
                issue('pending_process_limit'); break
            pending.append((int(value), parent))
    if cgroup is not None:
        pending = []
        directories = [cgroup]
        visited = 0
        while directories:
            directory = directories.pop(); visited += 1
            if visited > 256:
                issue('cgroup_directory_limit'); break
            try:
                enqueue(read_text(directory / 'cgroup.procs').split(), None)
                for entry in directory.iterdir():
                    if not entry.is_dir():
                        continue
                    if visited + len(directories) >= 256:
                        issue('cgroup_directory_limit'); break
                    directories.append(entry)
            except (OSError, ValueError) as error:
                issue('cgroup_read:' + type(error).__name__)
    seen = set()
    thread_attempts = 0
    while pending:
        pid, expected_parent = pending.pop(0)
        if pid in seen:
            continue
        if len(seen) >= max_processes:
            issue('process_limit'); break
        seen.add(pid)
        base = proc / str(pid)
        try:
            process = parse_stat(read_text(base / 'stat'))
            if expected_parent is not None and process['ppid'] != expected_parent:
                issue(f'child_identity_changed:{pid}'); continue
            result['processes'].append({'pid': pid, 'start_ticks': process['start_ticks'],
                                        'ppid': process['ppid'], 'comm': process['comm']})
            tasks = []
            for task in base.joinpath('task').iterdir():
                if len(tasks) + thread_attempts >= max_threads:
                    issue('thread_limit'); break
                tasks.append(task)
            tasks.sort(key=lambda p: int(p.name))
        except (OSError, ValueError) as error:
            issue(f'process_unavailable:{pid}:{type(error).__name__}'); continue
        for task in tasks:
            if thread_attempts >= max_threads:
                issue('thread_limit'); pending.clear(); break
            thread_attempts += 1
            tid = int(task.name)
            try:
                row = parse_stat(read_text(task / 'stat'))
                status = dict(line.split(':', 1) for line in read_text(task / 'status').splitlines() if ':' in line)
                name = status.get('Name', '').strip()
                role, provenance = thread_role(pid, tid, row['comm'])
                if name != row['comm']:
                    role, provenance = 'unknown', 'comm_changed_during_read'
                row.update(pid=pid, tid=tid, process_start_ticks=process['start_ticks'],
                           role=role, role_provenance=provenance,
                           voluntary_switches=int(status['voluntary_ctxt_switches'].strip()),
                           nonvoluntary_switches=int(status['nonvoluntary_ctxt_switches'].strip()))
                try:
                    sched = read_text(task / 'schedstat').split()
                    row['schedstat'] = {'runtime_ns': int(sched[0]), 'runqueue_ns': int(sched[1]),
                                        'timeslices': int(sched[2]), 'zero_may_mean_stats_disabled': True}
                except (OSError, ValueError, IndexError):
                    row['schedstat'] = None
                result['threads'].append(row)
                if cgroup is None:
                    enqueue(read_text(task / 'children').split(), pid)
            except (OSError, ValueError, KeyError) as error:
                issue(f'thread_unavailable:{pid}:{tid}:{type(error).__name__}')
    if not result['processes'] or not result['threads']:
        issue('no_live_threads_observed')
    if cgroup is not None and not any(row['pid'] == root_pid for row in result['processes']):
        issue('root_pid_not_observed_in_selected_cgroup')
    result['read_end_monotonic_ns'] = time.monotonic_ns()
    return result


def summarize(first: dict, last: dict, clock_ticks: int) -> dict:
    def identity(row):
        return (row['pid'], row['process_start_ticks'], row['tid'], row['start_ticks'])
    before = {identity(row): row for row in first['threads']}
    after = {identity(row): row for row in last['threads']}
    deltas, unknown = [], []
    if first is last:
        return {'clock_ticks_per_second': clock_ticks, 'thread_deltas': [], 'roles': {},
                'endpoints_complete': False, 'unknown_thread_cost': [
                    {'identity': key, 'reason': 'no_second_snapshot'} for key in before],
                'scope': 'Single snapshot; no CPU delta is available.'}
    counters = ('user_ticks', 'system_ticks', 'minor_faults', 'major_faults',
                'voluntary_switches', 'nonvoluntary_switches')
    for key, row in before.items():
        end = after.get(key)
        if end is None:
            unknown.append({'identity': key, 'reason': 'exited_or_unavailable_at_end'}); continue
        values = {name: end[name] - row[name] for name in counters}
        if any(value < 0 for value in values.values()):
            unknown.append({'identity': key, 'reason': 'counter_regressed', 'raw_deltas': values}); continue
        role = row['role'] if row['role'] == end['role'] else 'unknown_role_changed'
        deltas.append({'identity': key, 'role': role, **values,
                       'user_seconds': values['user_ticks'] / clock_ticks,
                       'system_seconds': values['system_ticks'] / clock_ticks,
                       'last_cpu_before': row['last_cpu'], 'last_cpu_after': end['last_cpu']})
    for key in after.keys() - before.keys():
        unknown.append({'identity': key, 'reason': 'new_or_unavailable_at_start'})
    roles = {}
    for row in deltas:
        item = roles.setdefault(row['role'], {'same_identity_threads': 0, 'user_seconds': 0, 'system_seconds': 0})
        item['same_identity_threads'] += 1
        item['user_seconds'] += row['user_seconds']; item['system_seconds'] += row['system_seconds']
    return {'clock_ticks_per_second': clock_ticks, 'thread_deltas': deltas, 'roles': roles,
            'unknown_thread_cost': unknown, 'endpoints_complete': first['complete'] and last['complete'],
            'scope': 'Endpoint live-thread deltas only; dead/new/missing-thread CPU is unknown. '
                     'Never clamp a negative delta to zero or substitute this sum for whole-service cgroup CPU.'}


def collect_thread_window(root_pid: int, seconds: float, interval: float = .1,
                          cgroup: Path | None = None, *, max_samples: int = 2048,
                          max_thread_records: int = 100000, stop_event=None) -> dict:
    if sys.platform != 'linux':
        raise RuntimeError('thread sampling requires Linux /proc')
    import resource
    if root_pid <= 0 or not math.isfinite(seconds) or not 0 < seconds <= 120:
        raise ValueError('pid must be positive and duration finite within0..120s')
    if (not math.isfinite(interval) or not .05 <= interval <= 5
            or type(max_samples) is not int or not 2 <= max_samples <= 2048
            or type(max_thread_records) is not int or not 1 <= max_thread_records <= 250000):
        raise ValueError('interval0.05..5s,samples2..2048,thread records1..250000 required')
    observer_before = resource.getrusage(resource.RUSAGE_SELF)
    start = time.monotonic(); samples = []; records = 0; stopped = None
    stop_requested = False
    while True:
        remaining = max_thread_records - records
        if remaining <= 0:
            stopped = 'bounded_sample_or_thread_record_limit'; break
        sample = snapshot(root_pid, cgroup, max_threads=min(4096, remaining))
        samples.append(sample); records += len(sample['threads'])
        elapsed = time.monotonic() - start
        if stop_event is not None and stop_event.is_set():
            stop_requested = True; break
        if elapsed >= seconds:
            break
        if len(samples) >= max_samples or records >= max_thread_records:
            stopped = 'bounded_sample_or_thread_record_limit'; break
        wait = min(interval, max(0, seconds - elapsed))
        if stop_event is None:
            time.sleep(wait)
        else:
            stop_event.wait(wait)
    ticks = os.sysconf('SC_CLK_TCK')
    observer_after = resource.getrusage(resource.RUSAGE_SELF)
    return {'schema': 'ku-http-thread-cost-v1', 'diagnostic_only': True,
            'root_pid': root_pid, 'cgroup': str(cgroup) if cgroup else None,
            'interval_seconds': interval, 'duration_requested_seconds': seconds,
            'elapsed_seconds': time.monotonic() - start, 'bounded_stop': stopped,
            'stop_requested': stop_requested,
            'samples': samples, 'summary': summarize(samples[0], samples[-1], ticks),
            'observer': {'pid': os.getpid(),
                         'user_seconds': observer_after.ru_utime - observer_before.ru_utime,
                         'system_seconds': observer_after.ru_stime - observer_before.ru_stime,
                         'lifetime_peak_rss_bytes': observer_after.ru_maxrss * 1024,
                         'scope': 'Whole current observer process during collection, excluding final JSON write. '
                                  'Keep outside service cgroup; do not add overlapping root-observer measurements.'},
            'interpretation': 'Thread state/lastCPU are snapshots, not scheduler-duration traces. '
                              'Faults are not proof of swap. Schedstat availability/configuration is separate.'}


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = parser.add_subparsers(dest='operation', required=True)
    prepare = sub.add_parser('prepare'); prepare.add_argument('source', type=Path)
    prepare.add_argument('destination', type=Path); prepare.add_argument('--source-commit')
    sample = sub.add_parser('sample'); sample.add_argument('--pid', type=int, required=True)
    sample.add_argument('--cgroup', type=Path); sample.add_argument('--seconds', type=float, default=15)
    sample.add_argument('--interval', type=float, default=.1); sample.add_argument('--output', type=Path, required=True)
    args = parser.parse_args()
    if args.operation == 'prepare':
        receipt = prepare_source(args.source, args.destination, args.source_commit)
        print(json.dumps(receipt, ensure_ascii=False, indent=2))
    else:
        import signal
        import threading
        stop = threading.Event()
        def request_stop(signum, frame):
            stop.set()
        previous = {kind: signal.signal(kind, request_stop)
                    for kind in (signal.SIGINT, signal.SIGTERM)}
        try:
            result = collect_thread_window(args.pid, args.seconds, args.interval, args.cgroup,
                                           stop_event=stop)
        finally:
            for kind, handler in previous.items():
                signal.signal(kind, handler)
        write_json(args.output, result)
        print(json.dumps({'samples': len(result['samples']), 'bounded_stop': result['bounded_stop'],
                          'roles': result['summary']['roles']}, indent=2))


if __name__ == '__main__':
    main()
