#!/usr/bin/env python3
"""Independent-process CPU comparison using the existing Linux resource domain.

This is a fresh-process CLI workload, separate from HTTP and server-JIT results.
repeat=0 observes fresh process startup; its duration is never subtracted from
the fixed repeat=32768 computation. Earlier repeat=64 rows are R&D only.
Four-core rows execute four independent processes in one total 4-core,
1-GiB domain, rather than granting four separate resource budgets.
"""
from __future__ import annotations

import argparse
import hashlib
import json
import math
import os
from pathlib import Path
import shutil
import statistics
import subprocess
import sys
import threading
import time

from linux_harness import Domain, GIB, Harness, Sampler, interleaved_orders, read_json, template, write_json

STACKS = ("ku", "go", "rust", "node", "java", "dotnet", "cpp")
ORDER_SEED = 20261023

ITERATIONS = 512000
SEED = 3
REPEAT = 32768
EXPECTED = 22944003
ROUNDS = 7
PROCESS_DEADLINE_SECONDS = 600
STREAM_LIMIT = 1048576
MIN_PROCESS_WALL_SECONDS = 30
MIN_COMPUTE_MS = 30000
SAMPLE_INTERVAL_SECONDS = .1
MAX_RESOURCE_SAMPLES = 6016


def digest(path):
    return hashlib.sha256(Path(path).read_bytes()).hexdigest()


def expected_checksum(iterations=ITERATIONS, seed=SEED):
    total = seed
    for index in range(iterations):
        total = (total * 33 + index + seed) % 100000000
    return total


def parse_result(path, repeat):
    """Strictly validate every returned checksum, including the empty startup row."""
    path = Path(path)
    if path.stat().st_size > STREAM_LIMIT:
        raise ValueError('batch stdout exceeded the fixed 1-MiB limit')

    def reject_constant(value):
        raise ValueError('non-JSON numeric constant: ' + value)

    value = json.loads(path.read_text(encoding='utf-8'), parse_constant=reject_constant)
    if not isinstance(value, dict):
        raise ValueError('batch output must be one JSON object')
    checksums = value.get('checksums')
    if not isinstance(checksums, list) or len(checksums) != repeat:
        raise ValueError('incorrect checksum count')
    if any(type(checksum) is not int or checksum != EXPECTED for checksum in checksums):
        raise ValueError('incorrect CPU checksum')
    elapsed = value.get('compute_ms')
    if type(elapsed) not in (int, float) or not math.isfinite(elapsed) or elapsed < 0:
        raise ValueError('invalid internal compute_ms')
    return value


def formal_duration_eligibility(repeat, process_wall_seconds, compute_ms, processes):
    """Classify actual durations; never sleep or increase work to qualify a row."""
    def finite_at_least(value, minimum):
        return type(value) in (int, float) and math.isfinite(value) and value >= minimum

    if repeat == 0:
        return {'applicable': False, 'eligible': False,
                'process_wall_seconds': process_wall_seconds,
                'internal_compute_ms_by_process': compute_ms,
                'reasons': ['startup observation has no formal compute-duration requirement']}
    reasons = []
    if repeat != REPEAT:
        reasons.append('nonformal repeat count; retain as R&D only')
    if not finite_at_least(process_wall_seconds, MIN_PROCESS_WALL_SECONDS):
        reasons.append('process wall shorter than 30s or invalid')
    elif process_wall_seconds > PROCESS_DEADLINE_SECONDS:
        reasons.append('process wall exceeds the absolute 600s deadline')
    if len(compute_ms) != processes or not compute_ms:
        reasons.append('missing internal compute duration for a process')
    elif not all(finite_at_least(value, MIN_COMPUTE_MS) for value in compute_ms):
        reasons.append('at least one internal compute duration is below 30000ms or invalid')
    return {
        'applicable': repeat != 0,
        'frozen_formal_repeat': REPEAT,
        'minimum_process_wall_seconds': MIN_PROCESS_WALL_SECONDS,
        'minimum_internal_compute_ms_per_process': MIN_COMPUTE_MS,
        'process_wall_seconds': process_wall_seconds,
        'internal_compute_ms_by_process': compute_ms,
        'eligible': not reasons,
        'reasons': reasons,
    }


def cpu_seconds(before, after):
    return max(0, after['cpu_stat'].get('usage_usec', 0)
               - before['cpu_stat'].get('usage_usec', 0)) / 1e6


def prepare_runtime(cwd, spec, replacements, uid, gid):
    """Place executable-relative Ku input under this child's private directory.

    Other stacks keep their original command and cwd/batch.json. The copied Ku
    executable is byte-identical; no shared build/input path or C source changes.
    """
    cwd = Path(cwd)
    layout = spec.get('runtime_layout')
    input_path = cwd / 'batch.json'
    placement = None
    binary_dir = None
    if layout:
        def relative_directory(name):
            suffix = Path(layout[name])
            if suffix.is_absolute() or not suffix.parts or '..' in suffix.parts:
                raise ValueError('runtime layout must stay inside private work directory')
            target = cwd
            for part in suffix.parts:
                target = target / part
                if target.is_symlink():
                    raise ValueError('runtime layout cannot use a symlink directory')
                target.mkdir(exist_ok=True)
                target.chmod(0o755)
                os.chown(target, uid, gid)
            return target
        binary_dir = relative_directory('binary_dir')
        input_path = relative_directory('fs_base_dir') / 'batch.json'
    replacements = dict(replacements, input=input_path, batch_input=input_path,
                        batch_json=input_path)
    original_command = [template(str(item), replacements) for item in spec['batch_command']]
    command = list(original_command)
    if layout:
        original_binary = Path(command[0]).resolve(strict=True)
        if not original_binary.is_file():
            raise ValueError('runtime layout requires an existing executable file')
        copied_binary = binary_dir / original_binary.name
        original_hash = digest(original_binary)
        shutil.copy2(original_binary, copied_binary)
        copied_binary.chmod(0o755)
        os.chown(copied_binary, uid, gid)
        actual_hash = digest(copied_binary)
        if actual_hash != original_hash:
            raise RuntimeError('private executable copy changed its content hash')
        command[0] = str(copied_binary)
        placement = {'requested': layout, 'original_binary': str(original_binary),
                     'actual_binary': str(copied_binary),
                     'original_binary_sha256': original_hash,
                     'actual_binary_sha256': actual_hash, 'byte_identical': True,
                     'input_path': str(input_path), 'executable_mode': '0755',
                     'uid': uid, 'gid': gid,
                     'scope': 'this child private work directory; no shared build inputs'}
    return input_path, replacements, original_command, command, placement


def run_group(harness, stack, cores, repeat, round_index, output):
    output = Path(output)
    output.mkdir(parents=True, exist_ok=True)
    report_path = output / 'batch.json'
    identity = {
        'manifest_sha256': digest(harness.root / 'manifest.json'),
        'protocol_sha256': digest(harness.protocol),
        'driver_sha256': digest(__file__),
        'stack': stack, 'cores': cores, 'repeat': repeat, 'round': round_index,
        'order_seed': ORDER_SEED, 'expected_paired_rounds': ROUNDS,
        'iterations': ITERATIONS, 'seed': SEED, 'processes': cores,
    }
    if report_path.exists():
        previous = read_json(report_path)
        if previous.get('identity') != identity:
            raise RuntimeError('existing batch evidence has different inputs; choose a new output root')
        return previous
    spec = harness.manifest['stacks'][stack]
    if not spec.get('batch_command'):
        raise RuntimeError('manifest is missing stacks.' + stack + '.batch_command')

    domain = Domain(harness, 'batch-' + stack, cores, GIB,
                    '0' if cores == 1 else '0-' + str(cores - 1))
    result = {
        'schema': 1, 'identity': identity,
        'kind': 'fresh_process_startup_observation' if repeat == 0 else 'independent_process_cpu',
        'measurement_class': ('startup_observation' if repeat == 0 else
                              'formal_candidate' if repeat == REPEAT else 'research_and_development'),
        'formal_eligible': False,
        'limits': domain.settings, 'cgroup': str(domain.path),
        'batch_input': {'iterations': ITERATIONS, 'seed': SEED, 'repeat': repeat},
        'expected_checksum': EXPECTED, 'children': [], 'passed': False,
        'startup_subtracted': False,
        'bounds': {'process_group_absolute_seconds': PROCESS_DEADLINE_SECONDS,
                   'bytes_per_stdout_or_stderr': STREAM_LIMIT,
                   'resource_sample_interval_seconds': SAMPLE_INTERVAL_SECONDS,
                   'maximum_resource_samples': MAX_RESOURCE_SAMPLES},
        'timing': 'process-wall starts at shared frozen-domain release and ends at last child exit; internal compute is separate',
        'memory_scope': 'whole service cgroup, including all independent processes; kernel memory.peak retained',
        'internal_timer': 'Ku monotonic API has 1ms resolution; other stacks use their monotonic high-resolution API; printed decimals do not establish actual precision; intervals below20ms are not precise ratio evidence',
        'jit_gc_warmup': {'mode':'fresh process, no in-process prewarm on any stack', 'scope':'whole-process startup/JIT/GC remain included; internal compute includes JIT/GC occurring during computation, so this is not a claim of steady JIT code-generation speed', 'gc':'normal defaults except the published heap fraction; whole cgroup accounts GC/helper threads', 'per_process_gc_or_jit_events':'unavailable in these batch CLI outputs, not zero'},
        'page_cache': 'fresh process, not a claim of cold filesystem page cache; no cache dropping',
        'coordinator_clock_resolution_seconds': time.get_clock_info('monotonic').resolution,
    }
    children = []
    waiter_threads = []
    workdirs = []
    sampler = None
    released_ns = None
    before = None
    error = None
    timed_out = False
    overflow = False
    try:
        # All children enter the domain before any workload starts. A frozen
        # empty domain is a barrier, not extra application work or another API.
        domain.freeze(True)
        for index in range(cores):
            child_dir = output / ('process-%02d' % index)
            child_dir.mkdir()
            cwd = child_dir / 'work'
            cwd.mkdir()
            os.chown(cwd, harness.uid, harness.gid)
            workdirs.append(cwd)
            replacements = {'root': harness.root, 'cores': 1, 'group_cores': cores, 'workers': 1,
                            'memory': GIB, 'workdir': cwd}
            input_path, replacements, original_command, command, placement = prepare_runtime(
                cwd, spec, replacements, harness.uid, harness.gid)
            input_path.write_text(json.dumps(result['batch_input']) + '\n')
            input_path.chmod(0o600)
            os.chown(input_path, harness.uid, harness.gid)
            env = os.environ.copy()
            env.update({key: template(str(value), replacements)
                        for key, value in harness.manifest.get('service_env', {}).items()})
            env.update({key: template(str(value), replacements)
                        for key, value in spec.get('env', {}).items()})
            # Each process computes serially; all four processes share the same
            # four-core domain. OS/GC helper threads are still fully accounted.
            env.update(BENCH_WORKERS='1', BENCH_CPU_WORKERS='1', GOMAXPROCS='1', DOTNET_PROCESSOR_COUNT='1')
            stdout_path, stderr_path = child_dir / 'stdout.json', child_dir / 'stderr.log'
            observed_env = {key: value for key, value in env.items()
                            if ((key.startswith('BENCH_') or key.startswith('KU_')
                                 or key.startswith(('TOKIO_', 'JAVA', 'DOTNET_')))
                                and key not in ('BENCH_DB_URL', 'KU_SERVICE_CONTROL_TOKEN'))
                            or key in ('GOGC', 'GOMEMLIMIT', 'GODEBUG', 'GOMAXPROCS',
                                       'NODE_OPTIONS', 'UV_THREADPOOL_SIZE', 'LD_LIBRARY_PATH')}
            entry = {'index': index, 'command': command,
                     'original_command': original_command, 'runtime_layout': placement,
                     'input_sha256': digest(input_path),
                     'stdout': str(stdout_path), 'stderr': str(stderr_path),
                     'environment': observed_env,
                     'command_files_sha256': {item: digest(item) for item in command
                                              if Path(item).is_file()},
                     'launch_monotonic_ns': time.monotonic_ns()}
            with input_path.open('rb') as stdin, stdout_path.open('wb') as stdout, stderr_path.open('wb') as stderr:
                process = subprocess.Popen(
                    [sys.executable, str(Path(__file__).with_name('linux_harness.py')),
                     '__child', str(domain.path), str(harness.uid), str(harness.gid),
                     str(cwd), '--'] + command,
                    stdin=stdin, stdout=stdout, stderr=stderr, env=env,
                    start_new_session=True)
            entry['pid'] = process.pid
            result['children'].append(entry)
            children.append((process, entry, stdout_path, stderr_path))

        admission_deadline = time.monotonic() + 5
        while time.monotonic() < admission_deadline:
            joined = (domain.path / 'cgroup.procs').read_text().split()
            if len(joined) == cores:
                break
            if any(process.poll() is not None for process, *_ in children):
                raise RuntimeError('batch launcher exited before joining the frozen domain')
            time.sleep(.005)
        else:
            raise RuntimeError('batch children did not all enter resource domain within 5s')

        def wait_for_child(process, entry):
            entry['exit_code'] = process.wait()
            entry['finished_monotonic_ns'] = time.monotonic_ns()

        for process, entry, *_ in children:
            waiter = threading.Thread(target=wait_for_child, args=(process, entry), daemon=True)
            waiter.start()
            waiter_threads.append(waiter)
        sampler = Sampler([domain], output / 'resources.json',
                          interval=SAMPLE_INTERVAL_SECONDS, maximum=MAX_RESOURCE_SAMPLES).start()
        before = domain.snapshot()
        released_ns = time.monotonic_ns()
        result['released_wall_time_ns'] = time.time_ns()
        domain.freeze(False)
        deadline_ns = released_ns + PROCESS_DEADLINE_SECONDS * 1000000000
        while any(waiter.is_alive() for waiter in waiter_threads):
            if time.monotonic_ns() >= deadline_ns:
                timed_out = True
                raise RuntimeError('batch exceeded the fixed absolute 600s process-group deadline')
            if any(path.stat().st_size > STREAM_LIMIT
                   for _, _, stdout, stderr in children for path in (stdout, stderr)):
                overflow = True
                raise RuntimeError('batch process output exceeded the fixed per-stream 1-MiB limit')
            time.sleep(.005)
        for waiter in waiter_threads:
            waiter.join(timeout=1)
        after = domain.snapshot()
        ended_ns = max(entry['finished_monotonic_ns'] for _, entry, *_ in children)
        elapsed = (ended_ns - released_ns) / 1e9
        result['process_wall_seconds'] = elapsed
        result['resource_before'] = before
        result['resource_after'] = after
        result['cpu_seconds'] = cpu_seconds(before, after)
        result['cpu_accounting_interval'] = 'entire shared domain from pre-release snapshot through all child exits, including startup/JIT/GC/output and failed work'
        if ended_ns > deadline_ns:
            timed_out = True
            raise RuntimeError('batch child finished after the fixed absolute 600s deadline')
        for _, entry, stdout, stderr in children:
            entry['stdout_bytes'], entry['stderr_bytes'] = stdout.stat().st_size, stderr.stat().st_size
            if max(entry['stdout_bytes'], entry['stderr_bytes']) > STREAM_LIMIT:
                overflow = True
                raise RuntimeError('batch process output exceeded the fixed per-stream 1-MiB limit')
            if entry['exit_code'] != 0:
                raise RuntimeError('batch child exited nonzero: ' + str(entry['exit_code']))
            entry['result'] = parse_result(stdout, repeat)
            entry['wall_seconds_from_group_release'] = (entry['finished_monotonic_ns'] - released_ns) / 1e9
        total = cores * repeat
        result['correct_computations'] = total
        result['correct_computations_per_wall_second'] = total / elapsed if total else None
        result['cpu_seconds_per_correct_computation'] = result['cpu_seconds'] / total if total else None
        result['correct_computations_per_cpu_second'] = total / result['cpu_seconds'] if total and result['cpu_seconds'] else None
        memory_oom = after.get('memory_events', {}).get('oom_kill', 0) - before.get('memory_events', {}).get('oom_kill', 0)
        result['oom_kills'] = memory_oom
        result['correctness_passed'] = memory_oom == 0
        result['formal_duration'] = formal_duration_eligibility(
            repeat, elapsed, [entry['result']['compute_ms'] for _, entry, *_ in children], cores)
        # Startup is a separate observation. A short formal compute row stays
        # visible and fails the run; it is never padded or retried with more work.
        result['passed'] = result['correctness_passed'] and (
            repeat != REPEAT or result['formal_duration']['eligible'])
    except BaseException as caught:
        error = caught
        result['error'] = repr(caught)
    finally:
        # A timeout/failure cannot strand a frozen child, a descendant, a budget
        # reservation or a partial worker directory.
        try:
            domain.kill()
        except Exception as caught:
            result['kill_error'] = repr(caught)
        for process, entry, stdout, stderr in children:
            try:
                process.wait(timeout=3)
            except subprocess.TimeoutExpired:
                try:
                    process.kill()
                except ProcessLookupError:
                    pass
                process.wait(timeout=3)
            entry.setdefault('exit_code', process.returncode)
            for path in (stdout, stderr):
                size = path.stat().st_size
                if size > STREAM_LIMIT:
                    entry[path.name + '_original_bytes'] = size
                    with path.open('r+b') as stream:
                        stream.truncate(STREAM_LIMIT)
                    entry[path.name + '_truncated'] = True
        for waiter in waiter_threads:
            waiter.join(timeout=1)
        if sampler is not None:
            samples = sampler.stop()
            result['sampler_overflow'] = sampler.overflow
            result['sampler_error'] = sampler.error
            domain_rows = [sample['domains'][0] for sample in samples if sample.get('domains')]
            result['sampled_resources'] = {
                'interval_seconds': sampler.interval, 'samples': len(samples),
                'maximum_samples': MAX_RESOURCE_SAMPLES,
                'peak_memory_current_bytes': max((row.get('memory_current', 0) or 0 for row in domain_rows), default=0),
                'kernel_memory_peak_bytes': max((row.get('memory_peak', 0) or 0 for row in domain_rows), default=0),
                'peak_threads': max((row.get('threads', 0) for row in domain_rows), default=0),
                'peak_processes': max((row.get('process_count', 0) for row in domain_rows), default=0),
                'peak_fd': max((row.get('fd', 0) for row in domain_rows), default=0),
                'scope': 'whole cgroup; short-lived per-process RSS may be missed between samples; memory.peak is kernel-maintained',
            }
        result['final_resources'] = domain.snapshot()
        result['timed_out'], result['output_overflow'] = timed_out, overflow
        result['cgroup_removed'] = domain.remove()
        cleanup_errors = []
        for cwd in workdirs:
            try:
                # Created fresh by this run: only private derived executable,
                # input and runtime scratch. Logs/evidence are siblings outside.
                shutil.rmtree(cwd)
            except OSError as caught:
                cleanup_errors.append(repr(caught))
        result['workdir_cleanup_errors'] = cleanup_errors
        if error is not None or result.get('sampler_error') or result.get('sampler_overflow') or not result['cgroup_removed'] or cleanup_errors:
            result['passed'] = False
        result['formal_eligible'] = (result['passed'] and repeat == REPEAT
                                     and result.get('formal_duration', {}).get('eligible', False))
        write_json(report_path, result)
    if isinstance(error, (KeyboardInterrupt, SystemExit)):
        raise error
    return result


def summary(rows, stacks=STACKS, core_points=(1,)):
    result = []
    for cores in core_points:
        for stack in stacks:
            formal = [row for row in rows if row.get('identity', {}).get('cores') == cores
                      and row.get('identity', {}).get('stack') == stack
                      and row.get('identity', {}).get('repeat') == REPEAT]
            valid = [row for row in formal if row.get('passed') and row.get('formal_eligible') is True]
            cold = [row for row in rows if row.get('identity', {}).get('cores') == cores
                    and row.get('identity', {}).get('stack') == stack
                    and row.get('identity', {}).get('repeat') == 0 and row.get('passed')]
            def values(field):
                return [row[field] for row in valid]
            item = {'stack': stack, 'cores': cores, 'processes': cores,
                    'expected_formal_rounds': ROUNDS, 'valid_formal_rounds': len(valid),
                    'failed_formal_rounds': len(formal) - len(valid),
                    'formal_ineligible_reasons': [row.get('formal_duration', {}).get('reasons') or
                                                 [row.get('error', 'other validation or lifecycle gate failed')]
                                                 for row in formal if row not in valid],
                    'fresh_process_startup_rounds': len(cold),
                    'startup_subtracted': False,
                    'cold_start_definition': 'repeat0 fresh process with ordinary page cache, not a cold-disk claim'}
            for field in ('process_wall_seconds', 'cpu_seconds', 'correct_computations_per_wall_second',
                          'cpu_seconds_per_correct_computation'):
                sample = values(field)
                item[field] = {'rounds': sample, 'median': statistics.median(sample) if sample else None,
                               'min': min(sample) if sample else None, 'max': max(sample) if sample else None}
            item['internal_compute_ms_by_round_process'] = [[child['result']['compute_ms']
                                                            for child in row['children']] for row in valid]
            item['fresh_process_startup_wall_seconds'] = [row['process_wall_seconds'] for row in cold]
            result.append(item)
    return result


def run(harness, output=None, stacks=STACKS, core_points=(1,)):
    output = Path(output or harness.output / 'batch')
    output.mkdir(parents=True, exist_ok=True)
    if expected_checksum() != EXPECTED:
        raise RuntimeError('independent checksum oracle disagrees with frozen expected value')
    rows, stopped = [], {}
    report = {'schema': 1, 'kind': 'independent_process_cpu_not_HTTP',
              'warmup': 'fresh processes are intentional; repeat0 is a separate observation without subtraction; Java/.NET/Node JIT and normal GC work belongs to each fresh process; this table is not prewarmed JIT peak speed',
              'matrix': {'cores': list(core_points), 'stacks': list(stacks), 'rounds': ROUNDS, 'order_seed': ORDER_SEED, 'order_by_round': interleaved_orders(stacks,ROUNDS,ORDER_SEED),
                         'iterations': ITERATIONS, 'seed': SEED, 'repeat': REPEAT,
                         'formal_minimum_process_wall_seconds': MIN_PROCESS_WALL_SECONDS,
                         'formal_minimum_compute_ms_per_process': MIN_COMPUTE_MS,
                         'process_group_absolute_seconds': PROCESS_DEADLINE_SECONDS,
                         'bytes_per_stdout_or_stderr': STREAM_LIMIT,
                         'resource_sample_interval_seconds': SAMPLE_INTERVAL_SECONDS,
                         'maximum_resource_samples': MAX_RESOURCE_SAMPLES,
                         'previous_repeat64': 'R&D only; retained evidence is not merged into formal rows',
                         'memory_bytes_total_per_group': GIB, 'order': 'frozen seeded random interleaving; same seven paired windows per stack',
                         'processes_per_group': 'equal to cores; all share one total resource domain'},
              'rows': rows, 'skipped': [], 'passed': False}
    for cores in core_points:
        for round_index, order in enumerate(interleaved_orders(stacks,ROUNDS,ORDER_SEED)):
            for stack in order:
                if stack in stopped:
                    report['skipped'].append({'stack': stack, 'cores': cores, 'round': round_index,
                                              'reason': stopped[stack]})
                    continue
                for repeat in (0, REPEAT):
                    path = output / ('cores-%d' % cores) / ('round-%02d' % round_index) / stack / ('startup' if repeat == 0 else 'compute')
                    row = run_group(harness, stack, cores, repeat, round_index, path)
                    row['report_path'] = str(path / 'batch.json')
                    rows.append(row)
                    report['summaries'] = summary(rows,stacks,core_points)
                    write_json(output / 'summary.json', report)
                    if not row['passed'] or (repeat != 0 and row.get('formal_eligible') is not True):
                        stopped[stack] = 'first invalid independent-process batch; preserve evidence and diagnose before more repeats'
                        break
    report['summaries'] = summary(rows,stacks,core_points)
    report['passed'] = len(rows) == len(core_points) * ROUNDS * len(stacks) * 2 and all(
        row['passed'] and (row['identity']['repeat'] == 0 or row.get('formal_eligible') is True)
        for row in rows)
    write_json(output / 'summary.json', report)
    return report


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--root', required=True, help='directory containing manifest.json')
    parser.add_argument('--output', required=True)
    parser.add_argument('--stacks', nargs='+', choices=STACKS, default=list(STACKS))
    parser.add_argument('--cores', nargs='+', type=int, choices=(1,4), default=[1], help='single-core is the common seven-stack main table; four-core means four independent serial processes')
    args = parser.parse_args()
    if len(set(args.stacks)) != len(args.stacks) or len(set(args.cores)) != len(args.cores):
        parser.error('duplicate stacks/cores are not paired independent windows')
    harness = Harness(args.root, args.output)
    try:
        result = run(harness,stacks=tuple(args.stacks),core_points=tuple(args.cores))
        print(json.dumps({'passed': result['passed'], 'summary': str(Path(args.output) / 'batch' / 'summary.json')}))
    finally:
        harness.close()


if __name__ == '__main__':
    main()
