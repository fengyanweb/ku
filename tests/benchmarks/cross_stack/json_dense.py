#!/usr/bin/env python3
"""Bounded actual-generated-handler diagnosis; no default timed benchmark.

The source must come from ordinary Ku CLI Release emission. No Transform is
reimplemented here. Dense CPU includes owned request construction and byte
validation, and is never substituted for HTTP service CPU or Rust acceptance.
"""
from __future__ import annotations
import argparse
import copy
import hashlib
import json
import math
import os
from pathlib import Path
import platform
import re
import shlex
import time

from compile_variants import Commands, digest, save

HERE = Path(__file__).resolve().parent
BATCH = 2048
FAMILIES = ('http-object', 'http-object32', 'ascii', 'chinese', 'emoji', 'escape', 'variants')
MAX_REPEAT = 50_000_000
SCHEMA = 'ku-json-dense-v1'
# The ordinary shared fixture sets handler_timeout_ms=45000. Its service_env has
# no request-timeout override, so c_http_runtime.c's fallback is 30000. These are
# durations, not absolute timestamps. Explicit flags bind a differently configured
# HTTP comparison; neither context infers deadlines from the ambient environment.
REQUEST_TIMEOUT_MS = 30000
HANDLER_TIMEOUT_MS = 45000


def encoded(value):
    return json.dumps(value, ensure_ascii=False, sort_keys=True, separators=(',', ':'),
                      allow_nan=False).encode('utf-8')


def corpus_inputs(directory=HERE):
    protocol = json.loads((directory/'protocol.json').read_text())
    result = {}
    for name in ('object', 'array', 'escape'):
        spec = protocol['corpora'][name]
        raw = (directory/spec['path']).read_bytes()
        if len(raw) != spec['bytes'] or hashlib.sha256(raw).hexdigest() != spec['sha256']:
            raise ValueError('frozen corpus mismatch: '+name)
        value = json.loads(raw)
        if (set(value) != {'counter', 'data'} or type(value['counter']) is not int
                or value['counter'] != spec['counter']):
            raise ValueError('invalid frozen corpus envelope: '+name)
        result[name] = (raw, value)
    return result


def make_cases(directory=HERE):
    """Frozen HTTP object plus independent character families and prior20 variants."""
    corpora = corpus_inputs(directory)
    rows = []

    def add(family, body, rid):
        value = json.loads(body)
        if type(value['counter']) is not int or not -(1 << 63) <= value['counter'] < (1 << 63)-1:
            raise ValueError('dense counter outside incrementable int64')
        rows.append({'family': family, 'input': body.decode('utf-8'), 'rid': rid,
                     'expected': {'id': rid, 'counter': value['counter']+1, 'data': value['data']}})

    # Exact HTTP bytes, including its final LF; only the numeric16-byte RID varies.
    for family, width in (('http-object', 16), ('http-object32', 32)):
        for index in range(16):
            add(family, corpora['object'][0], str(1_234_567_890_000_000+index).zfill(width))
    texts = {'ascii': 'plain ASCII 0123456789 '*40, 'chinese': '业务数据中文'*48,
             'emoji': '🙂😃🚀'*80, 'escape': 'A"\\\n\t\b\f\0'*64}
    for family, text in texts.items():
        for index in range(8):
            value = {'counter': index*37-123,
                     'data': {'text': text+str(index), 'items': [True, False, None, index, -index], 'empty': ''}}
            body = json.dumps(value, ensure_ascii=False, separators=(',', ':')).encode('utf-8')
            add(family, body, str(1_234_567_890_001_000+index))
    # Recreates the20 retained earlier inputs without archiving a duplicate fixture.
    for index in range(20):
        kind = 'object' if index < 16 else 'array' if index < 18 else 'escape'
        value = copy.deepcopy(corpora[kind][1]);value['counter'] = index*37-123
        if kind == 'object':
            value['data']['diagnostic_text'] = '中文🙂-'+str(index)+'-\\"\n'
        elif kind == 'array':
            value['data'].append({'edge': -(1 << 63), 'text': '中\0🙂'+str(index)})
        else:
            value['data'] = str(index)+'\t中文'+value['data']+'🙂\0\\"'
        body = json.dumps(value, ensure_ascii=False, separators=(',', ':')).encode('utf-8')
        add('variants', body, 'r'+str(index).zfill((16 if index%2 == 0 else 32)-1))
    return rows


def case_identity(rows):
    return hashlib.sha256(encoded(rows)).hexdigest()


def emit(generated, cases, template):
    source = generated.read_text(encoding='utf-8')
    signature = 'KuResult_struct___ku_http_response encode(KuStruct___ku_http_request req) {'
    if source.count(signature) != 1 or len(re.findall(r'^int main\(void\) \{', source, re.M)) != 1:
        raise ValueError('expected ordinary generated default text JSON handler/main')
    for symbol in ('ku_object_try_set_owned', 'ku_drop_struct___ku_http_request',
                   'ku_result_drop_struct___ku_http_response', '__ku_handler_timeout_begin',
                   'ku_http_deadline_ms', 'ku_http_now_ms', '__ku_sync_reset', '__ku_sync_finish_request'):
        if symbol not in source:
            raise ValueError('generated runtime missing '+symbol)
    source = re.sub(r'^int main\(void\) \{', 'static int ku_json_dense_original_main(void) {',
                    source, count=1, flags=re.M)
    arrays = ['\n/* Permanent diagnostic wrapper: original handler/runtime above unchanged. */',
              'typedef struct { const char* family; const uint8_t* body; size_t body_length; '
              'const uint8_t* rid; size_t rid_length; } KuJsonDenseCase;']
    for index, row in enumerate(cases):
        for name, data in (('body', row['input'].encode('utf-8')), ('rid', row['rid'].encode('utf-8'))):
            arrays.append('static const uint8_t ku_json_dense_%s_%d[]={%s};' %
                          (name, index, ','.join('0x%02x' % x for x in data)))
    arrays.append('static const KuJsonDenseCase ku_json_dense_cases[]={')
    for index, row in enumerate(cases):
        arrays.append('{"%s",ku_json_dense_body_%d,sizeof(ku_json_dense_body_%d),'
                      'ku_json_dense_rid_%d,sizeof(ku_json_dense_rid_%d)},' %
                      (row['family'], index, index, index, index))
    arrays += ['};', '#define KU_JSON_DENSE_CASE_COUNT (sizeof(ku_json_dense_cases)/sizeof(ku_json_dense_cases[0]))']
    return source+'\n'+'\n'.join(arrays)+'\n'+template.read_text(encoding='utf-8')


def same_json(actual, expected):
    if expected is None or type(expected) in (bool, str):
        return type(actual) is type(expected) and actual == expected
    if type(expected) in (int, float):
        return type(actual) in (int, float) and actual == expected
    if isinstance(expected, list):
        return isinstance(actual, list) and len(actual) == len(expected) and all(
            same_json(a, b) for a, b in zip(actual, expected))
    return isinstance(actual, dict) and set(actual) == set(expected) and all(
        same_json(actual[key], value) for key, value in expected.items())


def validate_output(text, cases, family, repeat=0, cpu_seconds=0, context='http',
                    request_ms=REQUEST_TIMEOUT_MS, handler_ms=HANDLER_TIMEOUT_MS):
    lines = [line for line in text.splitlines() if line.startswith('{"schema":"'+SCHEMA+'"')]
    if len(lines) != 1:
        raise ValueError('expected exactly one bounded dense result')
    value = json.loads(lines[0], parse_constant=lambda x: (_ for _ in ()).throw(ValueError(x)))
    selected = {index: row for index, row in enumerate(cases) if row['family'] == family}
    seen = set()
    if (value.get('schema') != SCHEMA or value.get('family') != family
            or value.get('batch_size') != BATCH or value.get('context') != context
            or value.get('request_timeout_ms') != request_ms
            or value.get('handler_timeout_ms') != handler_ms):
        raise ValueError('dense schema/family/batch/deadline context mismatch')
    for row in value.get('validation', []):
        index = row.get('case')
        if type(index) is not int or index not in selected or index in seen:
            raise ValueError('duplicate/unknown dense validation case')
        body = bytes.fromhex(row['body_hex'])
        actual = json.loads(body, parse_constant=lambda x: (_ for _ in ()).throw(ValueError(x)))
        if not same_json(actual, selected[index]['expected']):
            raise ValueError('dense semantic mismatch at case '+str(index))
        seen.add(index)
    if seen != set(selected):
        raise ValueError('incomplete dense semantic validation')
    correct, iterations = value.get('correct'), value.get('iterations')
    if (type(correct) is not int or type(iterations) is not int
            or not 0 <= correct <= MAX_REPEAT or correct != iterations):
        raise ValueError('dense iteration accounting mismatch')
    for key in ('thread_cpu_ns', 'wall_ns', 'setup_thread_cpu_ns', 'setup_wall_ns',
                'golden_cleanup_thread_cpu_ns', 'golden_cleanup_wall_ns'):
        if type(value.get(key)) is not int or value[key] < 0:
            raise ValueError('invalid dense clock field: '+key)
    if repeat and (correct != repeat or value['stop_reason'] != 'iterations'):
        raise ValueError('dense fixed repeat incomplete')
    if cpu_seconds and (value['thread_cpu_ns'] < cpu_seconds*1e9
                        or value['stop_reason'] != 'thread_cpu_budget'):
        raise ValueError('dense CPU budget incomplete')
    if not repeat and not cpu_seconds and (correct or value['stop_reason'] != 'validation_only'):
        raise ValueError('unexpected timed work in validation-only mode')
    value['semantic_preflight_correct'] = len(seen)
    value['ns_per_correct'] = value['thread_cpu_ns']/correct if correct else None
    value['performance_measured'] = bool(repeat or cpu_seconds)
    value['scope'] = ('dense diagnosis only; owned body/query/RID preparation, actual encode handler, '
                      'byte comparison and output cleanup; excludes socket, routing, queue, job locks/wakes '
                      'and service buffer accounting; not HTTP scoring')
    value['deadline_context'] = ('relative handler duration limited by remaining request deadline; active '
                                 'original guards; synthetic admission has no network/queue age' if context == 'http'
                                 else 'inactive HTTP deadline TLS; mechanism-only diagnostic, never default or service score')
    return value


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--generated-c', type=Path, required=True)
    parser.add_argument('--output', type=Path, required=True, help='fresh emission/evidence directory')
    parser.add_argument('--family', choices=FAMILIES, default='http-object')
    parser.add_argument('--source-commit', required=True)
    parser.add_argument('--cli-sha256', required=True)
    parser.add_argument('--cc', default='clang-18', help='one compiler executable, no shell flags')
    parser.add_argument('--link', type=Path, action='append', default=[], help='exact shared/import library file, e.g.libpq.so')
    parser.add_argument('--debug-symbols', action='store_true')
    parser.add_argument('--emit-only', action='store_true', help='do not compile or execute')
    parser.add_argument('--binary', type=Path, help='externally compiled wrapper; same emitted C identity required')
    mode = parser.add_mutually_exclusive_group()
    mode.add_argument('--cpu-seconds', type=float, default=0)
    mode.add_argument('--iterations', type=int, default=0)
    parser.add_argument('--wall-seconds', type=float, default=90)
    parser.add_argument('--context', choices=('http', 'none'), default='http',
                        help='http preserves active worker deadline context; none is separate mechanism-only diagnosis')
    parser.add_argument('--request-timeout-ms', type=int, default=REQUEST_TIMEOUT_MS,
                        help='ordinary HTTP fixture default request duration; set explicitly if comparison overrides it')
    parser.add_argument('--handler-timeout-ms', type=int, default=HANDLER_TIMEOUT_MS,
                        help='ordinary server.ku configured handler duration, clamped to remaining request deadline')
    parser.add_argument('--runner-prefix', default='', help='explicit diagnostic prefix, e.g.taskset/perf; parsed without shell')
    args = parser.parse_args(argv)
    if (not re.fullmatch('[0-9a-f]{40}', args.source_commit)
            or not re.fullmatch('[0-9a-f]{64}', args.cli_sha256)):
        parser.error('full lowercase source commit and CLI SHA256 required')
    if (not math.isfinite(args.cpu_seconds) or not 0 <= args.cpu_seconds <= 20
            or not 0 <= args.iterations <= MAX_REPEAT or not math.isfinite(args.wall_seconds)
            or not 1 <= args.wall_seconds <= 180
            or not 1 <= args.request_timeout_ms <= 300000 or not 1 <= args.handler_timeout_ms <= 300000):
        parser.error('bounded CPU/repeat/wall/deadline arguments required')
    if args.emit_only and args.binary:
        parser.error('--emit-only and --binary are separate operations')
    source = args.generated_c.resolve(strict=True)
    template = HERE/'json_dense.c.inc';cases = make_cases()
    wrapper = emit(source, cases, template)
    identity = {'schema': SCHEMA, 'source_commit': args.source_commit, 'cli_sha256': args.cli_sha256,
                'generated_c_sha256': digest(source), 'template_sha256': digest(template),
                'cases_sha256': case_identity(cases), 'wrapper_sha256': hashlib.sha256(wrapper.encode()).hexdigest(),
                'protocol_sha256': digest(HERE/'protocol.json'),
                'corpora_sha256': {name: digest(HERE/'corpora'/str(name+'.json')) for name in ('object', 'array', 'escape')}}
    out = args.output.resolve();emission = out/'emission.json';dense_c = out/'json-dense.c'
    libraries = [str(path.resolve(strict=True)) for path in args.link]
    command = [args.cc, str(dense_c), '-std=c11', '-O2', '-pthread', '-lm', *libraries]
    if args.debug_symbols:
        command.append('-g')
    command += ['-o', str(out/'json-dense')]
    if out.exists():
        receipt = json.loads(emission.read_text()) if emission.is_file() else {}
        if receipt.get('identity') != identity or receipt.get('expected_compile_command') != command:
            raise ValueError('output already exists with another emission identity')
        if not dense_c.is_file() or digest(dense_c) != identity['wrapper_sha256']:
            raise ValueError('emitted C changed after identity freeze')
    else:
        out.mkdir(parents=True)
        dense_c.write_text(wrapper, encoding='utf-8')
        save(out/'cases.json', cases)
        save(emission, {'identity': identity, 'expected_compile_command': command,
                       'note': 'actual generated Transform preserved; appended diagnostic wrapper is not normal service ELF or an HTTP score'})
    if args.emit_only:
        print(json.dumps({'emission': str(emission), 'compile_command': command, 'executed': False}))
        return
    if platform.system() not in ('Linux', 'Darwin'):
        raise ValueError('dense execution requires POSIX thread CPU clock; emission-only is portable')
    environment = dict(os.environ)
    for key in ('CFLAGS', 'CXXFLAGS', 'LDFLAGS', 'LLVM_PROFILE_FILE'):
        environment.pop(key, None)
    logs = Commands(out/('commands-'+str(time.time_ns())), environment)
    binary = args.binary.resolve(strict=True) if args.binary else out/'json-dense'
    report = {'identity': identity, 'family': args.family, 'context': args.context,
              'request_timeout_ms': args.request_timeout_ms, 'handler_timeout_ms': args.handler_timeout_ms,
              'passed': False, 'performance_measured': False, 'source_input_unchanged': False,
              'compile_mode': 'external wrapper binary; expected command is declared, verify actual build receipt separately' if args.binary else 'diagnostic C11 O2 compile',
              'expected_compile_command': command, 'scope': 'dense mechanism diagnosis, not HTTP/Rust acceptance'}
    result_path = out/('result-'+args.family+'-'+args.context+'-'+str(time.time_ns())+'.json')
    try:
        if not args.binary:
            logs.run('compile', command, out, timeout=180)
        report['binary_sha256'] = digest(binary)
        run_command = [*shlex.split(args.runner_prefix), str(binary), args.family,
                       str(args.cpu_seconds), str(args.iterations), str(args.wall_seconds), args.context,
                       str(args.request_timeout_ms), str(args.handler_timeout_ms)]
        raw = logs.run('dense-'+args.family, run_command, out, timeout=args.wall_seconds+2)
        report['measurement'] = validate_output(raw, cases, args.family, args.iterations, args.cpu_seconds,
                                                args.context, args.request_timeout_ms, args.handler_timeout_ms)
        report['performance_measured'] = report['measurement']['performance_measured']
        report['source_input_unchanged'] = digest(source) == identity['generated_c_sha256']
        if not report['source_input_unchanged']:
            raise ValueError('normal generated input C changed during diagnosis')
        report['passed'] = True
    except Exception as error:
        report['error'] = repr(error)
        raise
    finally:
        report['commands'] = logs.rows
        save(result_path, report)
        print(json.dumps({'result': str(result_path), 'passed': report['passed'],
                          'performance_measured': report['performance_measured']}))


if __name__ == '__main__':
    main()
