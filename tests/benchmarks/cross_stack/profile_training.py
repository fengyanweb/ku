#!/usr/bin/env python3
"""Bounded, identical batch-only PGO training with held-out business inputs.

This produces no HTTP/database or formal performance claim. Normal exit, every
checksum, whole-process resource cleanup and compiler-specific profile parsing
must pass before a profile can be used. Validation never contributes profiles.
"""
from __future__ import annotations
import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import subprocess
import sys
import time

from batch import prepare_runtime
from compile_variants import Commands, clean_environment, digest, load, save
from linux_harness import Domain, GIB, Harness, Sampler, template

PLANS = {'train': {'iterations': 512000, 'seed': 3, 'repeat': 256},
         'validate': {'iterations': 480000, 'seed': 7, 'repeat': 128}}


def encoded(value):
    return (json.dumps(value, sort_keys=True, separators=(',', ':'))+'\n').encode()


def identity(value):
    return hashlib.sha256(encoded(value)).hexdigest()


def expected(input_value):
    total, seed = input_value['seed'], input_value['seed']
    for index in range(input_value['iterations']):
        total = (total*33+index+seed) % 100000000
    return total


def plan(mode):
    return {'kind': 'serial_batch_compiler_'+mode, 'input': PLANS[mode],
            'cores': 1, 'memory_bytes': GIB, 'deadline_seconds': 60,
            'scope': 'fresh process batch only; no HTTP, database, pool or retry training'}


def parse_output(path, input_value):
    value = json.loads(path.read_text(), parse_constant=lambda x: (_ for _ in ()).throw(ValueError(x)))
    correct = expected(input_value)
    checksums = value.get('checksums')
    if (not isinstance(checksums, list) or len(checksums) != input_value['repeat']
            or any(type(x) is not int or x != correct for x in checksums)):
        raise ValueError('training/validation business checksum mismatch')
    return {'correct_computations': len(checksums), 'expected_checksum': correct,
            'output_sha256': digest(path), 'internal_compute_ms_observation': value.get('compute_ms')}


def run(manifest_path, output, stack, mode, tools):
    if output.exists():
        raise ValueError('fresh evidence output required')
    output.mkdir(parents=True)
    manifest = load(manifest_path)
    spec = manifest['stacks'][stack]
    variant = spec.get('compiler_variant', {})
    if (variant.get('stack') != stack or (mode == 'train' and variant.get('variant') != 'generate')
            or (mode == 'validate' and variant.get('variant') != 'use')):
        raise ValueError('train requires generate binary; held-out validation requires profile-use binary')
    h = Harness(manifest_path.parent, output/'harness')
    domain = Domain(h, 'pgo-'+mode+'-'+stack, 1, GIB, '0')
    cwd = output/'work'
    cwd.mkdir()
    os.chown(cwd, h.uid, h.gid)
    replacements = {'root': h.root, 'workdir': cwd, 'cores': 1, 'workers': 1, 'memory': GIB}
    input_path, replacements, original, command, placement = prepare_runtime(cwd, spec, replacements, h.uid, h.gid)
    input_path.write_bytes(encoded(PLANS[mode]))
    input_path.chmod(0o600)
    os.chown(input_path, h.uid, h.gid)
    env = clean_environment(tools)
    env.update({k: template(str(v), replacements) for k,v in spec.get('env', {}).items()})
    env.update(GOMAXPROCS='1', BENCH_WORKERS='1', BENCH_CPU_WORKERS='1')
    if mode == 'validate':
        env.pop('LLVM_PROFILE_FILE', None)
        env.pop('BENCH_CPU_PROFILE', None)
    if mode == 'train':
        raw_directory = Path(env['LLVM_PROFILE_FILE']).parent if stack != 'go' else None
        if (raw_directory and any(raw_directory.glob('*.profraw'))) or (stack == 'go' and Path(env['BENCH_CPU_PROFILE']).exists()):
            input_path.unlink(missing_ok=True)
            if placement:
                Path(placement['actual_binary']).unlink(missing_ok=True)
            removed = domain.remove()
            save(output/'receipt.json', {'passed':False,'mode':mode,'stack':stack,'preflight_error':'profile generation must start with an empty output set','normal_exit':False,'forced_termination':False,'domain_removed':removed,'profile_complete':False})
            raise ValueError('profile generation must start with an empty output set')
    manifest_snapshot=output/'manifest.training-input.json'
    manifest_snapshot.write_bytes(manifest_path.read_bytes())
    result = {'schema': 1, 'stack': stack, 'mode': mode,
        'training_driver_sha256':digest(__file__), 'manifest_snapshot':str(manifest_snapshot),
        'source_fingerprint': variant['source_fingerprint'],
        'manifest': str(manifest_path), 'manifest_sha256': digest(manifest_path),
        'plan': plan(mode), 'plan_sha256': identity(plan(mode)),
        'training_plan_sha256': identity(plan('train')), 'validation_plan_sha256': identity(plan('validate')),
        'training_input_sha256': identity(PLANS['train']), 'validation_input_sha256': identity(PLANS['validate']),
        'validation_inputs_reserved_before_training': True,
        'validation_executed': mode == 'validate', 'command': command,
        'original_command': original, 'runtime_layout': placement,
        'command_files_sha256': {x:digest(x) for x in command if Path(x).is_file()},
        'limits': domain.settings, 'forced_termination': False, 'normal_exit': False,
        'all_business_results_correct': False, 'profile_complete': False,
        'performance_measured': False, 'passed': False, 'started_wall_ns': time.time_ns(),
        'scope': plan(mode)['scope']}
    process = None
    sampler = Sampler([domain], output/'resources.json', interval=.1, maximum=1024).start()
    try:
        stdout, stderr = output/'stdout.json', output/'stderr.log'
        before = domain.snapshot()
        with input_path.open('rb') as source, stdout.open('xb') as out, stderr.open('xb') as err:
            process = subprocess.Popen([sys.executable, str(Path(__file__).with_name('linux_harness.py')),
                '__child', str(domain.path), str(h.uid), str(h.gid), str(cwd), '--', *command],
                env=env, stdin=source, stdout=out, stderr=err, start_new_session=True)
            deadline = time.monotonic()+60
            while process.poll() is None:
                if time.monotonic()>deadline or any(x.stat().st_size>65536 for x in (stdout,stderr)):
                    result['forced_termination'] = True
                    raise RuntimeError('fixed training deadline or per-stream output bound')
                time.sleep(.01)
        result['exit_code'] = process.returncode
        result['resource_before'], result['resource_after'] = before, domain.snapshot()
        if process.returncode != 0:
            raise RuntimeError('profiled child did not exit normally: '+str(process.returncode))
        result['normal_exit'] = True
        result.update(parse_output(stdout, PLANS[mode]))
        result['all_business_results_correct'] = True
        if mode == 'train':
            commands = Commands(output/'profile-check', clean_environment(tools))
            if stack == 'go':
                profile = Path(env['BENCH_CPU_PROFILE'])
                if not profile.is_file() or not profile.stat().st_size:
                    raise ValueError('normal-exit Go profile is absent/empty')
                parsed = commands.run('pprof-parse', [str(tools/'go/bin/go'), 'tool', 'pprof', '-top',
                    spec['command'][0], str(profile)], output, timeout=30)
                sample = re.search(r'Total samples =\s*([0-9.]+)', parsed)
                if not sample or float(sample.group(1)) <= 0:
                    raise ValueError('Go profile has no usable CPU samples')
            else:
                # The profile-output directory belongs to the generate build;
                # LLVM_PROFILE_FILE is a frozen absolute path in its manifest.
                profiles = Path(env['LLVM_PROFILE_FILE']).parent
                raw = sorted(profiles.glob('*.profraw'))
                if not raw or any(not p.stat().st_size for p in raw):
                    raise ValueError('normal-exit LLVM raw profiles absent/empty')
                if stack == 'rust':
                    compiler = tools/'cargo/bin/rustc'
                    sysroot = commands.run('rust-sysroot', [str(compiler), '+1.89.0', '--print', 'sysroot'], output, timeout=30)
                    merger = Path(sysroot)/'lib/rustlib/aarch64-unknown-linux-gnu/bin/llvm-profdata'
                else:
                    merger = Path('/usr/bin/llvm-profdata-18')
                profile = output/'merged.profdata'
                commands.run('profile-version', [str(merger), '--version'], output, timeout=30)
                commands.run('profile-merge', [str(merger), 'merge', '-o', str(profile), *map(str,raw)], output, timeout=60)
                parsed = commands.run('profile-parse', [str(merger), 'show', '--counts', str(profile)], output, timeout=30)
                sample = re.search(r'Maximum function count:\s*(\d+)', parsed)
                if not sample or int(sample.group(1)) <= 0:
                    raise ValueError('LLVM profile has no observed function execution')
                result['raw_profiles'] = {str(p): {'sha256':digest(p),'bytes':p.stat().st_size} for p in raw}
            result.update(profile_path=str(profile), profile_sha256=digest(profile),
                profile_bytes=profile.stat().st_size, profile_complete=True)
        result['passed'] = True
    except BaseException as error:
        result['error'] = repr(error)
        raise
    finally:
        if process and process.poll() is None:
            result['forced_termination'] = True
            domain.kill()
            process.wait(timeout=3)
        sampler.stop()
        result['resource_final'] = domain.snapshot()
        result['domain_removed'] = domain.remove()
        result['ended_wall_ns'] = time.time_ns()
        if result['resource_final']['process_count'] or not result['domain_removed']:
            result['passed'] = False
        if placement:
            Path(placement['actual_binary']).unlink(missing_ok=True)
        input_path.unlink(missing_ok=True)
        save(output/'receipt.json', result)
    return result


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--manifest', type=Path, required=True)
    parser.add_argument('--output', type=Path, required=True)
    parser.add_argument('--tools', type=Path, default=Path('/opt/ku-bench/tools'))
    parser.add_argument('--stack', choices=('ku','go','rust','cpp'), required=True)
    parser.add_argument('--mode', choices=('train','validate'), required=True)
    args = parser.parse_args()
    if os.geteuid()!=0 or sys.platform!='linux':
        parser.error('Linux cgroup coordinator root required; workload drops to manifest uid/gid')
    result=run(args.manifest.resolve(strict=True), args.output.resolve(), args.stack, args.mode, args.tools.resolve(strict=True))
    print(json.dumps({'passed':result['passed'],'receipt':str(args.output/'receipt.json')}))


if __name__=='__main__':
    main()
