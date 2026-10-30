#!/usr/bin/env python3
"""Bounded isolated batch or HTTP PGO training with held-out business inputs.

Batch remains the default; HTTP opt-in covers existing public HTTP/DB routes.
Neither produces a formal performance claim. Normal exit, every business result,
resource cleanup and compiler-specific parsing gate profile use. Validation never contributes profiles.
"""
from __future__ import annotations
import argparse
import hashlib
import http.client
import json
import os
from pathlib import Path
import re
import subprocess
import socket
import struct
import sys
import time

from batch import prepare_runtime
from compile_variants import Commands, clean_environment, digest, load, save
from linux_harness import (Domain, GIB, Harness, Sampler, template, quiet_metrics,
                           service_termination_succeeded)

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


def merge_profile(spec, env, tools, output, stack, result):
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
        # Only the explicitly attested LLVM_PROFILE_FILE directory is merged;
        # HTTP uses a fresh owned directory and batch retains its frozen path.
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
            merge_profile(spec, env, tools, output, stack, result)
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


def http_recipe(mode):
    held = mode == 'validate'
    payload = ({'data': [{'note': '留出🚀\t雪', 'values': [None, False, 1234567]}]*5, 'counter': 17}
               if held else {'counter': 7, 'data': {'message': '中文🙂"\\\n\0', 'items': [1, True, None]}})
    return {'transport': 'http', 'workloads': ['ok', 'cpu', 'json_object', 'db'],
        'seconds_per_workload': 2, 'rates': {'ok': 2000, 'json_object': 500, 'cpu': 80, 'db': 200}, 'concurrency': 8,
        'input': {'payload': payload, 'ascii_json': held, 'cpu_units': 47 if held else 32,
                  'seed': 9 if held else 5, 'db_key': 997 if held else 37,
                  'rid_prefix': 200 if held else 100},
        'negative_paths': ['invalid_json', 'malformed_content_length', 'one_sent_tcp_rst'],
        'negative_expected_status': {'invalid_json': {'ku': 500, 'rust': 400}, 'malformed_content_length': 400},
        'limits': {'manual_requests': 8, 'timeout_seconds': 2, 'response_bytes': 65536},
        'normal_driver_scope': 'shared frozen protocol/corpora/CPU arrays and DB keys1..400; regression, not held-out business evidence',
        'heldout_scope': 'supplements only: new JSON shape/order/size, seed, work, RID and DB997 outside train normal1..400/manual37',
        'scope': 'shared normal-route regression plus independent supplemental business inputs; no retry; no performance score'}


def http_probes(address, recipe, stack, rows):
    value = recipe['input']; rid = str(value['rid_prefix']*1000)
    payload = json.dumps(value['payload'], ensure_ascii=value['ascii_json'], separators=(',', ':')).encode()
    key, seed, units = value['db_key'], value['seed'], value['cpu_units']
    checks = [('/ok', None, 'ok:'+rid),
        ('/work&work='+'x'*units+'&seed='+'s'*seed, None,
         rid+':'+format(expected({'iterations': units*1000, 'seed': seed}), '08d')),
        ('/db&key='+str(key), None, {'id': key, 'score': key*17, 'label': f'item-{key}-中文🙂', 'rid': rid}),
        ('/json', payload, {'id': rid, 'counter': value['payload']['counter']+1, 'data': value['payload']['data']}),
        ('/json', b'{', recipe['negative_expected_status']['invalid_json'][stack]),
        ('/framing', None, recipe['negative_expected_status']['malformed_content_length'])]
    for route, body, wanted in checks:
        connection = http.client.HTTPConnection(address, timeout=2)
        try:
            path, separator, query = route.partition('&')
            if path == '/framing':
                connection.connect()
                connection.sock.sendall(b'GET /ok?rid='+rid.encode()+b' HTTP/1.1\r\nHost: localhost\r\nContent-Length: bad\r\n\r\n')
                response = http.client.HTTPResponse(connection.sock); response.begin()
            else:
                connection.request('POST' if body is not None else 'GET', path+'?rid='+rid+('&'+query if separator else ''), body,
                                   {'Content-Type': 'application/json', 'Connection': 'close'})
                response = connection.getresponse()
            data = response.read(65537)
            status = wanted if type(wanted) is int else 200
            row = {'path': route, 'expected_status': status, 'actual_status': response.status,
                   'expected_negative': type(wanted) is int, 'body_sha256': hashlib.sha256(data).hexdigest()}
            rows.append(row)
            if len(data)>65536 or response.status != status:
                raise ValueError('HTTP supplemental status/body bound mismatch: '+route)
            if type(wanted) is not int and (json.loads(data) if isinstance(wanted, dict) else data.decode()) != wanted:
                raise ValueError('HTTP supplemental business response mismatch: '+route)
        finally:
            connection.close()
    connection = http.client.HTTPConnection(address, timeout=2)
    try:
        connection.connect()
        request = ('GET /work?rid='+rid+'&work='+'x'*512+'&seed='+'s'*seed+' HTTP/1.1\r\nHost: localhost\r\n\r\n').encode()
        connection.sock.sendall(request); sent = time.monotonic_ns()
        time.sleep(.00025)
        connection.sock.setsockopt(socket.SOL_SOCKET, socket.SO_LINGER, struct.pack('ii', 1, 0))
        connection.close()
        rows.append({'path': '/work', 'planned_rst': True, 'request_fully_sent': True,
                     'sent_monotonic_ns': sent, 'reset_issued_ns': time.monotonic_ns(), 'correct': False,
                     'scope': 'client issued RST only; not proof of execution stop or per-request retirement'})
    finally:
        connection.close()
    connection = http.client.HTTPConnection(address, timeout=2)
    try:
        connection.request('GET', '/ok?rid='+rid)
        response = connection.getresponse(); body = response.read(65537)
        rows.append({'path': '/ok', 'after_errors_and_cancel': True, 'expected_status': 200, 'actual_status': response.status})
        if response.status != 200 or body != ('ok:'+rid).encode():
            raise ValueError('HTTP did not recover after explicit errors/cancel')
    finally:
        connection.close()


def run_http(manifest_path, output, stack, mode, tools):
    if stack not in ('ku', 'rust') or output.exists():
        raise ValueError('HTTP profile requires Ku/Rust and fresh evidence output')
    manifest = load(manifest_path); spec = manifest['stacks'][stack]; variant = spec['compiler_variant']
    build = load(variant['receipt'])
    if (variant.get('stack') != stack or variant.get('variant') != ('generate' if mode=='train' else 'use')
            or build.get('passed') is not True or build.get('source_files_unchanged') is not True
            or build.get('stack') != stack or build.get('variant') != variant['variant']
            or build.get('source_fingerprint') != variant['source_fingerprint']):
        raise ValueError('HTTP requires completed matching generate/use build receipt')
    if mode=='validate' and any(build.get('training_receipt', {}).get(key) != value for key,value in {
            'transport': 'http', 'training_plan_sha256': identity(http_recipe('train')),
            'validation_plan_sha256': identity(http_recipe('validate'))}.items()):
        raise ValueError('HTTP held-out validation requires this reserved HTTP training recipe, not batch training')
    bound = dict(build['source_inputs'])
    bound.update({path: value['sha256'] for path,value in build['executable_artifacts'].items()})
    if str(Path(spec['command'][0])) not in build['executable_artifacts']:
        raise ValueError('HTTP server command is not an attested build executable')
    def unchanged():
        if any(digest(path) != sha for path,sha in bound.items()):
            raise ValueError('HTTP profile source/ELF identity changed')
    unchanged(); output.mkdir(parents=True)
    snapshot = output/'manifest.training-input.json'; snapshot.write_bytes(manifest_path.read_bytes())
    recipe = http_recipe(mode)
    with Path(spec['command'][0]).open('rb') as binary: elf_header = binary.read(64)
    if elf_header[:4] != b'\x7fELF': raise ValueError('HTTP profile target must be the attested Linux ELF')
    result = {'schema': 1, 'transport': 'http', 'stack': stack, 'mode': mode, 'passed': False,
        'source_fingerprint': variant['source_fingerprint'], 'manifest_snapshot': str(snapshot), 'manifest': str(manifest_path),
        'manifest_sha256': digest(manifest_path), 'training_driver_sha256': digest(__file__),
        'build_receipt_sha256': digest(variant['receipt']), 'compiler_version': build['compiler_version'],
        'compiler_launcher_sha256': build['compiler_launcher_sha256'], 'source_and_elf_sha256': bound,
        'target_elf_header': elf_header.hex(),
        'target_elf_machine': int.from_bytes(elf_header[18:20], 'little' if elf_header[5]==1 else 'big'),
        'plan': recipe, 'plan_sha256': identity(recipe), 'manual_samples': [], 'driver_windows': [],
        'heldout_new_inputs': recipe['input'] if mode=='validate' else None,
        'normal_driver_inputs_shared': recipe['normal_driver_scope'],
        'training_plan_sha256': identity(http_recipe('train')), 'validation_plan_sha256': identity(http_recipe('validate')),
        'training_input_sha256': identity(http_recipe('train')['input']),
        'validation_input_sha256': identity(http_recipe('validate')['input']),
        'validation_inputs_reserved_before_training': True, 'validation_executed': mode=='validate',
        'normal_exit': False, 'forced_termination': False, 'domain_removed': False,
        'profile_complete': False, 'all_business_results_correct': False, 'performance_measured': False, 'scope': recipe['scope']}
    h, service = None, None
    try:
        h = Harness(manifest_path.parent, output/'harness')
        if not h.db_domain: raise ValueError('HTTP DB route requires the declared owned database fixture/domain')
        result['base_protocol_sha256'] = digest(h.protocol)
        if mode=='validate' and build['training_receipt'].get('base_protocol_sha256') != digest(h.protocol):
            raise ValueError('held-out HTTP protocol differs from training frozen base')
        profiles = output/'http-profiles'; profiles.mkdir(); os.chown(profiles, h.uid, h.gid)
        env = {'LLVM_PROFILE_FILE': str(profiles/'%m-%p.profraw') if mode=='train' else '/dev/null'}
        protocol_path = h.protocol; protocol = load(protocol_path); corpus_dir = output/'corpora'; corpus_dir.mkdir()
        for name,item in protocol['corpora'].items():
            source = h.protocol.parent/item['path']; destination = corpus_dir/(name+'.json')
            body = source.read_bytes()
            if hashlib.sha256(body).hexdigest()!=item['sha256']:
                raise ValueError('frozen HTTP corpus changed: '+name)
            if json.loads(body)==http_recipe('validate')['input']['payload']:
                raise ValueError('reserved held-out payload overlaps a normal training corpus')
            destination.write_bytes(body)
        h.protocol = output/'protocol.http.json'; h.protocol.write_bytes(protocol_path.read_bytes())
        h.identity['protocol_sha256'] = digest(h.protocol)
        result.update(protocol_sha256=digest(h.protocol), corpora_sha256={name:item['sha256'] for name,item in protocol['corpora'].items()},
                      profile_environment=env, database_domain=str(h.db_domain.path), driver_commands=[])
        service = h.start(stack, cores=2, memory=GIB, output=output/'service', extra_env=env)
        result['command'] = service.startup['command']; result['startup'] = service.startup
        original = list(h.manifest['client']['command'])
        for index,kind in enumerate(recipe['workloads']):
            h.manifest['client']['command'] = original+['--rid-prefix', str(recipe['input']['rid_prefix']+index)]
            result['driver_commands'].append(h.manifest['client']['command'])
            report = h.client(service, kind, recipe['rates'][kind], 2, output/('driver-'+kind), concurrency=8, timeout=3)
            result['driver_windows'].append({'kind': kind, 'ledger': report['ledger'], 'configuration': report['configuration']})
            if (report.get('valid_driver') is not True or report['ledger'].get('planned_reconciled') is not True
                    or report['ledger'].get('planned') != recipe['rates'][kind]*2
                    or report['ledger'].get('correct_total') != report['ledger'].get('planned')
                    or report['ledger'].get('results') != {'correct': report['ledger'].get('planned')}):
                raise ValueError('HTTP training main driver results not entirely correct: '+kind)
        http_probes(service.address, recipe, stack, result['manual_samples'])
        result['all_business_results_correct'] = True
        deadline = time.monotonic()+5; metrics = service.metrics()
        while not quiet_metrics(stack, metrics) and time.monotonic()<deadline:
            time.sleep(.1); metrics = service.metrics()
        result['quiet_metrics'] = metrics; termination = service.stop(); result['termination'] = termination
        result.update(normal_exit=service_termination_succeeded(stack, termination),
                      forced_termination=termination.get('forced'), domain_removed=termination.get('cgroup_removed'))
        if not result['normal_exit'] or not quiet_metrics(stack, metrics):
            raise ValueError('HTTP profile requires quiet drain and normal service exit before merge')
        unchanged()
        if mode=='train': merge_profile(spec, env, tools, output, stack, result)
        elif any(output.rglob('*.profraw')): raise ValueError('held-out validation unexpectedly generated profiles')
        result['passed'] = True
    except BaseException as error:
        result['error'] = repr(error); raise
    finally:
        if h:
            try: result['cleanup'] = h.close()
            except BaseException as error: result.update(passed=False, cleanup_error=repr(error))
            if service and service.domain.path.exists(): result['passed'] = False
        save(output/'receipt.json', result)
    return result


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--manifest', type=Path, required=True)
    parser.add_argument('--output', type=Path, required=True)
    parser.add_argument('--tools', type=Path, default=Path('/opt/ku-bench/tools'))
    parser.add_argument('--stack', choices=('ku','go','rust','cpp'), required=True)
    parser.add_argument('--mode', choices=('train','validate'), required=True)
    parser.add_argument('--transport', choices=('batch','http'), default='batch')
    args = parser.parse_args()
    if os.geteuid()!=0 or sys.platform!='linux':
        parser.error('Linux cgroup coordinator root required; workload drops to manifest uid/gid')
    result=(run_http if args.transport=='http' else run)(args.manifest.resolve(strict=True), args.output.resolve(), args.stack, args.mode, args.tools.resolve(strict=True))
    print(json.dumps({'passed':result['passed'],'receipt':str(args.output/'receipt.json')}))
    if args.transport=='http' and not result['passed']: raise SystemExit(2)


if __name__=='__main__':
    main()
