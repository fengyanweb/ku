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
import signal, struct, subprocess, sys, tarfile, shutil

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




def generated_function_span(source, signature):
    """Locate one complete C body, ignoring braces in literals/comments."""
    if source.count(signature) != 1:
        raise ValueError('expected one generated function: '+signature)
    start=source.index(signature);i=start+len(signature)-1;depth=0
    while i<len(source):
        if source.startswith('//',i):
            end=source.find('\n',i+2)
            if end<0:break
            i=end+1;continue
        if source.startswith('/*',i):
            end=source.find('*/',i+2)
            if end<0:break
            i=end+2;continue
        char=source[i]
        if char in ('"', "'"):
            quote=char;i+=1
            while i<len(source):
                if source[i]=='\\':i+=2;continue
                if source[i]==quote:i+=1;break
                i+=1
            continue
        if char=='{':depth+=1
        elif char=='}':
            depth-=1
            if depth==0:return start,i+1
        i+=1
    raise ValueError('cannot locate complete generated function')

def instrument_phases(source, allocation_ledger=True):
    """Diagnostic-only exclusive phase clocks + the existing allocation hook.

    This wraps the actual generated Transform, never a reimplemented JSON loop.
    Thread-CPU reads and header allocation hooks perturb this binary; its CPU
    is not eligible for HTTP scoring or a precise ordinary-product percentage.
    """
    hook_source = (HERE.parent.parent/'support/native_allocation_harness.rs').read_text()
    match = re.search(r'pub const ALLOCATION_HOOK: &str = r#"(.*?)"#;', hook_source, re.S)
    if not match:
        raise ValueError('existing native allocation hook unavailable')
    hook = match.group(1)
    prefix = r"""
/* Single-thread dense diagnosis. No server/event-loop is started. */
enum { KU_DENSE_OFF, KU_DENSE_REQUEST, KU_DENSE_OBJECT, KU_DENSE_DECODE,
       KU_DENSE_ENCODE, KU_DENSE_DROP, KU_DENSE_VALIDATE, KU_DENSE_PHASES };
typedef struct {
  uint64_t cpu_ns, segments, allocations, requested_bytes, reallocations;
  uint64_t explicit_copy_calls, explicit_copy_bytes, frees, live_at_last_exit;
} KuDensePhase;
static KuDensePhase ku_dense_phases[KU_DENSE_PHASES];
static unsigned ku_dense_phase;
static uint64_t ku_dense_phase_started,ku_dense_phase_reads;
static uint64_t ku_dense_calibration_ns,ku_dense_calibration_reads;
static size_t ku_dense_live_baseline;
static uint64_t ku_dense_phase_clock(void) {
  struct timespec t={0};
  if(clock_gettime(CLOCK_THREAD_CPUTIME_ID,&t) || t.tv_sec<0 || t.tv_nsec<0
      || t.tv_nsec>=1000000000L || (uint64_t)t.tv_sec>UINT64_MAX/1000000000u) {
    fputs("dense phase clock failed\n",stderr);exit(3);
  }
  return (uint64_t)t.tv_sec*UINT64_C(1000000000)+(uint64_t)t.tv_nsec;
}
static unsigned ku_dense_change_phase(unsigned next);
"""
    # Count each concrete request made by the shared hook, including realloc;
    # requested bytes include old/new capacity overlap, not unique live bytes.
    hook = hook.replace('  ku_perf_calls++;',
        '  ku_perf_calls++;\n  ku_dense_phases[ku_dense_phase].allocations++;\n'
        '  ku_dense_phases[ku_dense_phase].requested_bytes += size;')
    hook = hook.replace('  KuPerfAllocation* replacement =',
        '  ku_dense_phases[ku_dense_phase].reallocations++;\n  KuPerfAllocation* replacement =')
    hook = hook.replace('  ku_perf_live_allocations--;',
        '  ku_dense_phases[ku_dense_phase].frees++;\n  ku_perf_live_allocations--;')
    helpers = r"""
static unsigned ku_dense_change_phase(unsigned next) {
  uint64_t now=ku_dense_phase_clock();ku_dense_phase_reads++;
  unsigned old=ku_dense_phase;
  if(old) {
    if(now<ku_dense_phase_started) {fputs("dense phase clock reversed\n",stderr);exit(3);}
    ku_dense_phases[old].cpu_ns+=now-ku_dense_phase_started;
    ku_dense_phases[old].live_at_last_exit=ku_perf_live_bytes;
  }
  ku_dense_phase=next;ku_dense_phase_started=now;
  if(next)ku_dense_phases[next].segments++;
  return old;
}
static void* ku_dense_memcpy(void* d,const void* s,size_t n) {
  ku_dense_phases[ku_dense_phase].explicit_copy_calls++;
  ku_dense_phases[ku_dense_phase].explicit_copy_bytes+=n;
  return memcpy(d,s,n);
}
static void* ku_dense_memmove(void* d,const void* s,size_t n) {
  ku_dense_phases[ku_dense_phase].explicit_copy_calls++;
  ku_dense_phases[ku_dense_phase].explicit_copy_bytes+=n;
  return memmove(d,s,n);
}
#define memcpy ku_dense_memcpy
#define memmove ku_dense_memmove
static void ku_dense_reset_phases(void) {
  ku_dense_change_phase(KU_DENSE_OFF);
  memset(ku_dense_phases,0,sizeof(ku_dense_phases));ku_dense_phase_reads=0;
  ku_dense_live_baseline=ku_perf_live_bytes;ku_perf_peak_bytes=ku_perf_live_bytes;
  uint64_t begin=ku_dense_phase_clock(),last=begin;
  /* Report calibration, do not silently subtract it. Allocator/copy-hook
   * perturbation and compiler layout cannot be removed by clock subtraction. */
  for(size_t i=0;i<20000;i++)last=ku_dense_phase_clock();
  ku_dense_calibration_ns=last-begin;ku_dense_calibration_reads=20000;
}
static void ku_dense_write_phases(void) {
  static const char* names[]={"off","request_projection","object_and_control",
      "decode","encode","drop","validation"};
  fputs(",\"phase_ledger\":{\"diagnostic_only\":true,\"normal_score_eligible\":false,\"exclusive\":true,\"rows\":[",stdout);
  for(size_t i=1;i<KU_DENSE_PHASES;i++) {
    KuDensePhase* p=&ku_dense_phases[i];if(i>1)fputc(',',stdout);
    fprintf(stdout,"{\"phase\":\"%s\",\"gross_thread_cpu_ns\":%llu,\"segments\":%llu,\"allocation_requests\":%llu,\"requested_bytes\":%llu,\"reallocations\":%llu,\"explicit_copy_calls\":%llu,\"explicit_copy_bytes\":%llu,\"frees\":%llu,\"live_bytes_at_last_exit\":%llu}",names[i],
      (unsigned long long)p->cpu_ns,(unsigned long long)p->segments,
      (unsigned long long)p->allocations,(unsigned long long)p->requested_bytes,
      (unsigned long long)p->reallocations,(unsigned long long)p->explicit_copy_calls,
      (unsigned long long)p->explicit_copy_bytes,(unsigned long long)p->frees,
      (unsigned long long)p->live_at_last_exit);
  }
  fprintf(stdout,"],\"phase_clock_reads\":%llu,\"clock_calibration_ns\":%llu,\"clock_calibration_reads\":%llu,\"live_baseline_bytes\":%zu,\"peak_live_bytes\":%zu,\"live_after_cleanup_bytes\":%zu,\"allocator_retained_bytes\":null,\"allocator_retained_status\":\"unknown; hook live bytes are not allocator retained pages or RSS\",\"copy_scope\":\"explicit runtime memcpy/memmove calls only; allocator realloc moves and compiler scalar copies not counted\"}",
      (unsigned long long)ku_dense_phase_reads,(unsigned long long)ku_dense_calibration_ns,
      (unsigned long long)ku_dense_calibration_reads,ku_dense_live_baseline,
      ku_perf_peak_bytes,ku_perf_live_bytes);
}
"""
    helpers = helpers.replace(r'\"exclusive\":true',
        r'\"exclusive\":true,\"allocation_hook_enabled\":'+str(allocation_ledger).lower())
    if not allocation_ledger:
        # Clock-only uses ordinary allocation/copy calls. Unknown allocation
        # values are omitted, never reported as measured zero.
        needle = "    KuDensePhase* p=&ku_dense_phases[i];if(i>1)fputc(',',stdout);"
        replacement = needle+r"""
    fprintf(stdout,"{\"phase\":\"%s\",\"gross_thread_cpu_ns\":%llu,\"segments\":%llu}",
        names[i],(unsigned long long)p->cpu_ns,(unsigned long long)p->segments);continue;
"""
        helpers = helpers.replace(needle,replacement)
        tail_start=helpers.index(r'  fprintf(stdout,"],\"phase_clock_reads')
        tail_end=helpers.index('ku_perf_peak_bytes,ku_perf_live_bytes);',tail_start)+len('ku_perf_peak_bytes,ku_perf_live_bytes);')
        clock_tail=r"""  fprintf(stdout,"],\"phase_clock_reads\":%llu,\"clock_calibration_ns\":%llu,\"clock_calibration_reads\":%llu,\"live_baseline_bytes\":null,\"peak_live_bytes\":null,\"live_after_cleanup_bytes\":null,\"allocator_retained_bytes\":null,\"allocator_retained_status\":\"unknown; allocation hook disabled for clock-only diagnosis\"}",
      (unsigned long long)ku_dense_phase_reads,(unsigned long long)ku_dense_calibration_ns,
      (unsigned long long)ku_dense_calibration_reads);"""
        helpers=helpers[:tail_start]+clock_tail+helpers[tail_end:]
        helpers += '\n#undef malloc\n#undef calloc\n#undef realloc\n#undef free\n#undef memcpy\n#undef memmove\n'
    declaration = 'typedef struct KuString {'
    if source.count(declaration) != 1:
        raise ValueError('cannot locate generated allocation boundary')
    source = source.replace(declaration, prefix+hook+helpers+declaration, 1)
    signature = 'KuResult_struct___ku_http_response encode(KuStruct___ku_http_request req) {'
    start,end=generated_function_span(source,signature)
    body = source[start:end]
    if body.count('ku_json_parse(') != 1 or body.count('ku_json_stringify(') != 1:
        raise ValueError('Transform decode/encode boundaries changed')
    wrappers = r"""
static KuResult_kuvalue ku_dense_decode(KuString text) {
  unsigned old=ku_dense_change_phase(KU_DENSE_DECODE);
  KuResult_kuvalue result=ku_json_parse(text);ku_dense_change_phase(old);return result;
}
static KuResult_str ku_dense_encode(KuValue value) {
  unsigned old=ku_dense_change_phase(KU_DENSE_ENCODE);
  KuResult_str result=ku_json_stringify(value);ku_dense_change_phase(old);return result;
}
static void ku_dense_request_drop(KuStruct___ku_http_request* value) {
  unsigned old=ku_dense_change_phase(KU_DENSE_DROP);
  ku_drop_struct___ku_http_request(value);ku_dense_change_phase(old);
}
static void ku_dense_object_drop(KuObject* value) {
  if(!value) { ku_object_drop(value);return; }
  unsigned old=ku_dense_change_phase(KU_DENSE_DROP);
  ku_object_drop(value);ku_dense_change_phase(old);
}
"""
    body = body.replace(signature, signature+'\n  unsigned ku_dense_outer=ku_dense_change_phase(KU_DENSE_OBJECT);', 1)
    body = body.replace('ku_json_parse(', 'ku_dense_decode(').replace('ku_json_stringify(', 'ku_dense_encode(')
    body = body.replace('ku_drop_struct___ku_http_request(', 'ku_dense_request_drop(')
    # Only top-level generated cleanup is wrapped: nested drops stay in decode/
    # encode/object, so phase CPU is exclusive rather than overlapping inclusive.
    body = body.replace('ku_object_drop(', 'ku_dense_object_drop(')
    body, returns = re.subn(r'(?m)^(  )(return [^\n]+)', r'\1ku_dense_change_phase(ku_dense_outer);\n\1\2', body)
    if returns < 2:
        raise ValueError('generated Transform return coverage changed')
    return source[:start]+wrappers+body+source[end:]


def make_http_control_source(source, cases):
    """Same HTTP body/output sizes and sync worker, without Transform business.

    Deliberately a frozen-corpus diagnostic control, NOT a product fast path or
    a correct general JSON handler. The real dynamic16-digit RID is copied into
    each newly allocated response. No cached result enters a scored manifest.
    """
    signature = 'KuResult_struct___ku_http_response encode(KuStruct___ku_http_request req) {'
    if source.count(signature) != 1:
        raise ValueError('expected one actual synchronous JSON handler')
    start,end=generated_function_span(source,signature)
    case = next(row for row in cases if row['family'] == 'http-object')
    expected = copy.deepcopy(case['expected']);expected['id'] = '0'*16
    body = encoded(expected)
    needle = b'"id":"'+b'0'*16+b'"'
    if body.count(needle) != 1:
        raise ValueError('control RID slot is not unique')
    offset = body.index(needle)+len(b'"id":"')
    code = signature+r"""
  if (++__ku_call_depth > KU_MAX_CALL_DEPTH) { fputs("maximum call depth exceeded\n",stderr);exit(1); }
  static const uint8_t template_body[]={BODY};
  KuString body={0},rid=ku_http_map_get(req.query,ku_string_static((const uint8_t*)"rid",3));
  KuResult_struct___ku_http_response result={0};
  int valid=rid.len==16u;
  for(size_t i=0;valid && i<rid.len;i++)if(rid.ptr[i]<'0' || rid.ptr[i]>'9')valid=0;
  if(!valid) {
    result.error=ku_error_make(ku_string_static((const uint8_t*)"diagnostic",10),
        ku_string_static((const uint8_t*)"control_input",13),
        ku_string_static((const uint8_t*)"control requires numeric16 RID",30));
  } else if(!(body.ptr=(uint8_t*)malloc(sizeof(template_body)))) {
    result.error=ku_object_out_of_memory_error();
  } else {
    body.len=body.capacity=sizeof(template_body);body.storage=KU_STRING_OWNED;
    memcpy(body.ptr,template_body,sizeof(template_body));
    memcpy(body.ptr+OFFSET,rid.ptr,16u);
    result.ok=true;result.value.status=200;
    result.value.content_type=ku_string_static((const uint8_t*)"text/plain; charset=utf-8",25);
    result.value.body=ku_string_move(&body);
  }
  ku_string_drop(&body);ku_string_drop(&rid);ku_drop_struct___ku_http_request(&req);
  __ku_call_depth--;return result;
}
"""
    code = code.replace('BODY', ','.join(str(x) for x in body)).replace('OFFSET', str(offset))
    result = source[:start]+code+source[end:]
    return result, dict(schema='ku-json-http-control-v1',diagnostic_only=True,normal_score_eligible=False,
        input_body_bytes=len(case['input'].encode()),output_body_bytes=len(body),rid_bytes=16,rid_offset=offset,
        input_body_sha256=hashlib.sha256(case['input'].encode()).hexdigest(),
        output_template_sha256=hashlib.sha256(body).hexdigest(),thread_model='original synchronous HTTP worker',
        preserved='ordinary transport/routing/UTF8/projection/job/deadline/budget/response write/drop;dynamicRID and response allocation',
        omitted='JSON decode/object Transform/encode;frozen corpus result template;not general JSON semantics',
        scope='diagnostic attribution only;not HTTP/Rust formal ranking or a product optimization')

def emit(generated, cases, template, phase_ledger=False, allocation_ledger=True):
    source = generated.read_text(encoding='utf-8')
    signature = 'KuResult_struct___ku_http_response encode(KuStruct___ku_http_request req) {'
    if source.count(signature) != 1 or len(re.findall(r'^int main\(void\) \{', source, re.M)) != 1:
        raise ValueError('expected ordinary generated default text JSON handler/main')
    for symbol in ('ku_object_try_set_owned', 'ku_drop_struct___ku_http_request',
                   'ku_result_drop_struct___ku_http_response', '__ku_handler_timeout_begin',
                   'ku_http_deadline_ms', 'ku_http_now_ms', '__ku_sync_reset', '__ku_sync_finish_request'):
        if symbol not in source:
            raise ValueError('generated runtime missing '+symbol)
    if phase_ledger:
        source = instrument_phases(source, allocation_ledger)
    source = re.sub(r'^int main\(void\) \{', 'static int ku_json_dense_original_main(void) {',
                    source, count=1, flags=re.M)
    arrays = ['\n/* Permanent diagnostic wrapper: original handler/runtime above unchanged. */',
              '#define KU_JSON_DENSE_PHASE_LEDGER '+str(int(phase_ledger)),
              '#define KU_JSON_DENSE_HAS_COLD_INPUTS '+str(int('typedef struct KuTaskBlockingInputsV1 {' in source)),
              '#define KU_JSON_DENSE_HAS_FREE_INDEX '+str(int('uint32_t* free_indices;' in source)),
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


def validate_layout(layout):
    """Check compiled sizeof/offsetof evidence, never infer residency from it."""
    if (not isinstance(layout, dict) or layout.get('schema') != 'ku-json-dense-layout-v1'
            or layout.get('resident_memory_measurement') is not False
            or type(layout.get('available')) is not bool or layout.get('fixed_capacity') != 2048):
        raise ValueError('missing or invalid compiled dense layout metadata')
    sizes = layout.get('sizeof', {})
    for key in ('void_pointer', 'KuString', 'KuValue'):
        if type(sizes.get(key)) is not int or sizes[key] <= 0:
            raise ValueError('invalid compiled dense sizeof: '+key)
    if not layout['available']:
        if layout.get('fixed_bytes') is not None:
            raise ValueError('unavailable dense layout has fixed bytes')
        return layout
    features = layout.get('features', {})
    for key in ('FILES', 'DIRECTORY', 'NETWORK', 'PG'):
        value = features.get('KU_TASK_BLOCKING_'+key)
        if type(value) is not int or value not in (0, 1):
            raise ValueError('invalid compiled dense feature: '+key)
    for key in ('KuTaskBlockingJobV1', 'KuTaskBlockingPoolV1', 'ring_cell'):
        if type(sizes.get(key)) is not int or sizes[key] <= 0:
            raise ValueError('invalid compiled dense sizeof: '+key)
    if features['KU_TASK_BLOCKING_NETWORK']:
        if type(sizes.get('net_config')) is not int or sizes['net_config'] <= 0:
            raise ValueError('invalid compiled dense net config size')
    elif sizes.get('net_config') is not None:
        raise ValueError('disabled dense network has net config size')
    cold = sizes.get('KuTaskBlockingInputsV1', 0)
    free_index = sizes.get('free_index_cell', 0)
    if type(cold) is not int or cold < 0 or type(free_index) is not int or free_index not in (0, 4):
        raise ValueError('invalid compiled dense cold/free-index size')
    expected = sizes['KuTaskBlockingPoolV1']+2048*(sizes['KuTaskBlockingJobV1']+cold+sizes['ring_cell']+free_index)
    if type(layout.get('fixed_bytes')) is not int or layout['fixed_bytes'] != expected:
        raise ValueError('compiled dense fixed byte formula mismatch')
    offsets = layout.get('offsetof', {})
    required = ['generation', 'output_charge', 'ticket', 'result', 'exists_value']
    required += ['inputs'] if cold else ['path', 'content']
    if features['KU_TASK_BLOCKING_PG']:
        required += ['pg_operation', 'pg_pool', 'pg_ready']
    if features['KU_TASK_BLOCKING_NETWORK']:
        required += ['net_deadline', 'net_requested', 'net_client', 'net_write', 'net_owner', 'net_addresses']
        if not cold:
            required += ['net_config', 'net_present']
    for key in required:
        if type(offsets.get(key)) is not int or not 0 <= offsets[key] < sizes['KuTaskBlockingJobV1']:
            raise ValueError('invalid compiled dense offsetof: '+key)
    if cold:
        inputs = layout.get('input_offsetof', {})
        required = ['path', 'content']+(['net_config', 'net_present'] if features['KU_TASK_BLOCKING_NETWORK'] else [])
        for key in required:
            if type(inputs.get(key)) is not int or not 0 <= inputs[key] < cold:
                raise ValueError('invalid compiled dense cold input offsetof: '+key)
    return layout


def validate_output(text, cases, family, repeat=0, cpu_seconds=0, context='http',
                    request_ms=REQUEST_TIMEOUT_MS, handler_ms=HANDLER_TIMEOUT_MS, phase_mode=None):
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
    ledger=value.get('phase_ledger')
    if phase_mode is not None:
        if (phase_mode not in ('allocation','clocks') or not isinstance(ledger,dict)
                or ledger.get('diagnostic_only') is not True or ledger.get('normal_score_eligible') is not False
                or ledger.get('exclusive') is not True
                or ledger.get('allocation_hook_enabled') is not (phase_mode=='allocation')):
            raise ValueError('missing or mismatched diagnostic phase ledger')
        rows=ledger.get('rows',[])
        if (len(rows)!=6 or {r.get('phase') for r in rows} !=
                {'request_projection','object_and_control','decode','encode','drop','validation'}):
            raise ValueError('incomplete diagnostic phase coverage')
        for row in rows:
            for key in ('gross_thread_cpu_ns','segments'):
                if type(row.get(key)) is not int or row[key]<0:
                    raise ValueError('invalid diagnostic phase counter')
            if row['phase'] in ('decode','encode') and row['segments']!=correct:
                raise ValueError('phase Transform denominator mismatch')
        if sum(r['gross_thread_cpu_ns'] for r in rows)>value['thread_cpu_ns']:
            raise ValueError('exclusive phase CPU exceeds whole loop CPU')
        if (type(ledger.get('phase_clock_reads')) is not int or ledger['phase_clock_reads']<0
                or ledger.get('clock_calibration_reads')!=20000
                or type(ledger.get('clock_calibration_ns')) is not int or ledger['clock_calibration_ns']<0):
            raise ValueError('invalid phase clock calibration')
        if phase_mode=='allocation' and ledger.get('live_after_cleanup_bytes')!=0:
            raise ValueError('dense allocation hook did not return to zero after cleanup')
        if phase_mode=='clocks' and any(ledger.get(k) is not None for k in
                ('live_baseline_bytes','peak_live_bytes','live_after_cleanup_bytes')):
            raise ValueError('clock-only mode must not invent live allocation measurements')
        ledger['clock_read_calibration_ns_per_read']=ledger['clock_calibration_ns']/20000
        ledger['interpretation']='exclusive instrumented CPU only; calibration not silently subtracted; hook/layout/clock perturbation not ordinary service cost'
    elif ledger is not None:
        raise ValueError('instrumented output cannot be accepted as plain dense output')
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



# One isolated plain-ELF diagnostic; none of these gates change ordinary main().
PLAIN_RUN='41182804290'
PLAIN_ARCHIVE='5976cad0c051b684823214f898cf746e2c7d63cd54d3116b04b509eba4c9dac6'
PLAIN_PRODUCT='4e5f5d94da277654862d8679f7ae4a7fda3f05ff'
PLAIN_C='5d65826ab66bb24039aa256339ae20c314337c9379711a047a934f408b6166d1'
PLAIN_CLI='25182f1d307952451e93c6c4074fff55d058b770d9ccbb0c90cc4e4c8f1fc74d'
PLAIN_TEMPLATE='35dbe8a9dc0765a7545ac704ded648e702352af7a8680ea1ea8c18bdfe1ada85'
PLAIN_N=1572864


def plain_run(command,output,timeout,*,environment=None,raw=None):
    """Ordinary-user child group, finite time/log/raw; failure never retries as root."""
    output=Path(output);began=time.monotonic();process=None
    receipt=dict(argv=list(map(str,command)),uid=os.getuid(),timeout_seconds=timeout,log_limit_bytes=1<<20,raw_limit_bytes=16<<20)
    try:
        with output.open('xb') as stream:
            process=subprocess.Popen(receipt['argv'],stdout=stream,stderr=subprocess.STDOUT,
                stdin=subprocess.DEVNULL,env=environment,start_new_session=True)
            while process.poll() is None:
                if time.monotonic()-began>timeout or output.stat().st_size>1<<20 or (raw and Path(raw).exists() and Path(raw).stat().st_size>16<<20):
                    raise RuntimeError('plain command time/output bound')
                time.sleep(.025)
        if process.returncode or output.stat().st_size>1<<20:raise RuntimeError('plain command failed: '+str(output))
        if raw and not 0<Path(raw).stat().st_size<=16<<20:raise ValueError('bounded nonempty perf raw required')
        return output.read_text(errors='replace')
    finally:
        if process is not None and process.poll() is None:
            os.killpg(process.pid,signal.SIGKILL);process.wait(timeout=2);receipt['forced_cleanup']=True
        receipt.update(exit_code=process.returncode if process else None,seconds=time.monotonic()-began)
        save(output.with_suffix(output.suffix+'.json'),receipt)


def plain_raw_counts(path):
    """Raw record modes/loss only; exact target-PID/PC attribution remains offline."""
    raw=Path(path).read_bytes()
    if not 104<=len(raw)<=16<<20 or raw[:8]!=b'PERFILE2':raise ValueError('bounded PERFILE2 required')
    offset,size=struct.unpack_from('<QQ',raw,40)
    if not 0<offset<=len(raw) or size>len(raw)-offset:raise ValueError('perf data bounds')
    end=offset+size;counts=dict(all=0,user=0,kernel=0,other=0)
    while offset<end:
        if offset+8>end:raise ValueError('truncated perf record')
        kind,misc,n=struct.unpack_from('<IHH',raw,offset)
        if n<8 or offset+n>end:raise ValueError('perf record bounds')
        if kind in (2,5,6,13):raise ValueError('perf loss/throttle invalidates this capture')
        if kind==9:
            counts['all']+=1;counts['kernel' if misc&7==1 else 'user' if misc&7==2 else 'other']+=1
        offset+=n
    if not counts['all']:raise ValueError('no CPU samples')
    return counts


def plain_inputs(archive,output):
    """Fixed archive, selected immutable inputs only; no old measurement or build launcher."""
    if digest(archive)!=PLAIN_ARCHIVE:raise ValueError('fixed normal archive SHA required')
    output=Path(output);output.mkdir()
    prefix='${EVIDENCE_ROOT}/'
    with tarfile.open(archive,'r:gz') as tar:
        members=tar.getmembers();names=[m.name for m in members]
        if len(names)>20000 or len(set(names))!=len(names):raise ValueError('archive member identity')
        index=json.load(tar.extractfile('portable-file-map.json'));mapping={r['path']:r for r in index['files']}
        if index.get('path_root')!='${EVIDENCE_ROOT}' or len(mapping)!=len(index['files']):raise ValueError('portable provenance required')
        selected={}
        def take(name):
            member=tar.getmember(name)
            if not member.isfile() or not 0<member.size<=4<<20:raise ValueError('bounded regular input required')
            data=tar.extractfile(member).read();entry=mapping[name]
            if entry['bytes']!=len(data) or hashlib.sha256(data).hexdigest()!=entry['archived_sha256']:raise ValueError('archive input SHA mismatch')
            selected[name]=dict(entry);return data
        manifest_bytes=take('B/manifest.json');manifest=json.loads(manifest_bytes);build=manifest['stacks']['ku']['build'];native=build['native']['server']
        if manifest['candidate_base']!=PLAIN_PRODUCT or build['cli_sha256']!=PLAIN_CLI or native['generated_c_sha256']!=PLAIN_C:raise ValueError('normal source/CLI/C identity mismatch')
        source=take(native['generated_c'].removeprefix(prefix))
        if hashlib.sha256(source).hexdigest()!=PLAIN_C:raise ValueError('normal C bytes mismatch')
        receipt_bytes=take(manifest['build_receipt'].removeprefix(prefix));receipt=json.loads(receipt_bytes)
        (output/'original-build.json').write_bytes(receipt_bytes)
        if receipt.get('candidate_base')!=PLAIN_PRODUCT or receipt.get('failure') or not receipt.get('commands') or any(r.get('exit_code')!=0 for r in receipt.get('commands',[])):raise ValueError('original build not completed')
        (output/'original-compiler.json').write_bytes(take(native['compiler_receipt'].removeprefix(prefix)))
        libraries=native['compiler_invocation']['library_inputs']
        if native['compiler_invocation']['flags']!=['-std=c11','-O2','-pthread'] or len(libraries)!=1 or libraries[0]['role']!='libpq':raise ValueError('original normal compiler contract mismatch')
        library=libraries[0]['selected_input'];lib=take(library['path'].removeprefix(prefix))
        if hashlib.sha256(lib).hexdigest()!=library['sha256']:raise ValueError('exact libpq mismatch')
        for name in ['protocol.json','corpora/object.json','corpora/array.json','corpora/escape.json']:
            path=HERE/name;entry=mapping['B/src/tests/benchmarks/cross_stack/'+name]
            if digest(path)!=entry['original_sha256']:raise ValueError('frozen JSON input changed')
            target=output/'inputs'/name;target.parent.mkdir(parents=True,exist_ok=True);shutil.copyfile(path,target)
        (output/'normal.c').write_bytes(source);(output/'libpq.so').write_bytes(lib)
        os.link(output/'libpq.so',output/'libpq.so.5')
        (output/'manifest.json').write_bytes(manifest_bytes);save(output/'provenance.json',dict(source_run=PLAIN_RUN,archive_sha256=PLAIN_ARCHIVE,
            original_product=PLAIN_PRODUCT,CLI_sha256=PLAIN_CLI,CLI_executed=False,selected=selected,imported_measurement_windows=0))
    return output/'normal.c',output/'libpq.so'


def plain_profile(archive,output):
    if os.geteuid()==0 or platform.system()!='Linux' or platform.machine() not in ('aarch64','arm64'):raise ValueError('ordinary Linux ARM64 runner required; no root fallback')
    if digest(HERE/'json_dense.c.inc')!=PLAIN_TEMPLATE:raise ValueError('frozen plain template changed')
    checkout=HERE.parents[2]
    subprocess.run(['git','-C',str(checkout),'diff','--exit-code',PLAIN_PRODUCT,'--','src','std','build.rs','Cargo.toml','Cargo.lock','crates'],check=True,timeout=10)
    affinity=sorted(os.sched_getaffinity(0))
    if not affinity:raise ValueError('nonempty allowed CPU affinity required')
    out=Path(output).resolve();out.mkdir(parents=True,exist_ok=False);deadline=time.monotonic()+300
    summary=dict(complete=False,diagnostic_only=True,normal_score_eligible=False,source_product=PLAIN_PRODUCT,source_runtime='f181894173c7bd722091f6525ac04466bdc1cded',
        allowed_affinity=affinity,pinned_core=affinity[0],thread_core_distribution='unknown without scheduler evidence; raw COMM/TID retained',tool_commit=subprocess.check_output(['git','-C',str(checkout),'rev-parse','HEAD'],text=True,timeout=10).strip(),uid=os.getuid(),iterations=PLAIN_N,order=['before','sample','after'],rows=[],
        scope='perf SELF is COMPLETE process lifetime: loader/setup/loop/printing/cleanup/exit. Dense internal intervals do not sum to that denominator; not HTTP score or subtraction.')
    def run(command,name,seconds=10,raw=None):
        remaining=min(seconds,deadline-time.monotonic())
        if remaining<=0:raise TimeoutError('plain total wall bound')
        return plain_run(command,out/(name+'.log'),remaining,environment=environment,raw=raw)
    environment=dict(os.environ)
    for key in ('CFLAGS','CXXFLAGS','LDFLAGS','LLVM_PROFILE_FILE','LD_PRELOAD'):environment.pop(key,None)
    try:
        source,lib=plain_inputs(archive,out/'reuse');cases=make_cases();wrapper=emit(source,cases,HERE/'json_dense.c.inc',False)
        dense=out/'plain.c';dense.write_text(wrapper);binary=out/'plain';environment['LD_LIBRARY_PATH']=str(lib.parent.resolve())
        run(['clang-18',str(dense),'-std=c11','-O2','-g','-pthread','-lm',str(lib),'-o',str(binary)],'compile',120)
        summary.update(ELF_sha256=digest(binary),C_sha256=digest(dense),normal_C_sha256=digest(source),template_sha256=PLAIN_TEMPLATE,cases_sha256=case_identity(cases),phase_ledger=False,phase_clocks=False)
        rawdir=out/'raw';rawdir.mkdir();perf=shutil.which('perf')
        if not perf:raise ValueError('perf unavailable')
        summary.update(perf_sha256=digest(perf),compiler_sha256=digest(shutil.which('clang-18')),whole_loop_correct=3*PLAIN_N,setup_encode_calls_each_window=16,process_total_encode_calls_each_window=PLAIN_N+16)
        run(['clang-18','--version'],'compiler-version')
        pre=rawdir/'preflight.perf.data'
        run(['taskset','-c',str(affinity[0]),perf,'record','--clockid','mono','-e','cpu-clock','-F','499','-o',str(pre),'--',sys.executable,'-c',
            'import time\nend=time.process_time()+.25\nwhile time.process_time()<end: pass'],'perf-exec-preflight',10,pre)
        summary['preflight']=plain_raw_counts(pre)
        for turn in summary['order']:
            if digest(binary)!=summary['ELF_sha256']:raise ValueError('same plain ELF changed')
            command=[str(binary),'http-object','0',str(PLAIN_N),'45','http','30000','45000'];raw=rawdir/'sample.perf.data'
            if turn=='sample':command=[perf,'record','--clockid','mono','-e','cpu-clock','-F','499','-o',str(raw),'--',*command]
            result=validate_output(run(['taskset','-c',str(affinity[0]),*command],turn,45,raw if turn=='sample' else None),cases,'http-object',PLAIN_N,0,'http',30000,45000,None)
            summary['rows'].append(dict(turn=turn,measurement=result,ELF_sha256=digest(binary)))
            if turn=='sample':
                summary['raw_record_mode_counts_not_final_PC_denominator']=plain_raw_counts(raw)
                summary['raw_sha256']=digest(raw)
                run([perf,'report','-i',str(raw),'--header','--stdio','--no-children','--show-nr-samples','--percent-limit','0','--sort','symbol,dso,comm'],'SELF',20)
                run([perf,'script','-i',str(raw),'--ns','--show-lost-events','--show-task-events','-F','comm,pid,tid,time,ip,sym,dso,symoff,misc'],'PC',20)
                run([perf,'buildid-list','-i',str(raw)],'buildIDs')
        run(['readelf','-n',str(binary)],'ELF-buildID');modules=run(['ldd',str(binary)],'modules')
        directory=out/'modules';directory.mkdir();copies=[];paths=set(re.findall(r'(/[^\s()]+)',modules))
        paths.update(re.findall(r'^[0-9a-f]+\s+(/[^\n]+)$',(out/'buildIDs.log').read_text(),re.M))
        total=0
        for path in sorted(paths):
            file=Path(path)
            if file==binary or not file.is_file():continue
            total+=file.stat().st_size
            if len(copies)>=32 or total>128<<20:raise ValueError('module evidence bound')
            target=directory/(digest(file)[:16]+'-'+file.name);shutil.copyfile(file,target)
            if digest(target)!=digest(file):raise ValueError('module identity changed')
            notes=run(['readelf','-n',str(target)],'module-'+str(len(copies)))
            copies.append(dict(mapped_path=path,file=target.name,sha256=digest(target),build_ids=re.findall(r'Build ID: ([0-9a-f]+)',notes)))
        save(directory/'identities.json',copies)
        run(['llvm-objcopy-18','-O','binary','--only-section=.text',str(binary),str(out/'plain.text')],'instructions')
        summary['instruction_sha256']=digest(out/'plain.text');summary['complete']=True
        summary['cpu_drift_after_over_before']=summary['rows'][2]['measurement']['thread_cpu_ns']/summary['rows'][0]['measurement']['thread_cpu_ns']
        summary['sampling_CPU_over_controls_geomean']=summary['rows'][1]['measurement']['thread_cpu_ns']/math.sqrt(summary['rows'][0]['measurement']['thread_cpu_ns']*summary['rows'][2]['measurement']['thread_cpu_ns'])
    except Exception as error:summary['blocked']=repr(error);raise
    finally:save(out/'summary.json',summary)
    return summary


def plain_profile_main(argv):
    parser=argparse.ArgumentParser(description='Fixed uninstrumented JSON plain SELF diagnostic only')
    parser.add_argument('--plain-profile',action='store_true',required=True);parser.add_argument('--archive',type=Path,required=True);parser.add_argument('--output',type=Path,required=True)
    a=parser.parse_args(argv);plain_profile(a.archive,a.output)


def main(argv=None):
    selected=sys.argv[1:] if argv is None else argv
    if '--plain-profile' in selected:return plain_profile_main(selected)
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--generated-c', type=Path, required=True)
    parser.add_argument('--output', type=Path, required=True, help='fresh emission/evidence directory')
    parser.add_argument('--family', choices=FAMILIES, default='http-object')
    parser.add_argument('--source-commit', required=True)
    parser.add_argument('--cli-sha256', required=True)
    parser.add_argument('--cc', default='clang-18', help='one compiler executable, no shell flags')
    parser.add_argument('--link', type=Path, action='append', default=[], help='exact shared/import library file, e.g.libpq.so')
    parser.add_argument('--debug-symbols', action='store_true')
    parser.add_argument('--phase-ledger', action='store_true', help='diagnostic-only exclusive phase clocks/allocation hook; never HTTP scoring')
    parser.add_argument('--phase-clocks', action='store_true', help='phase threadCPU clocks only; allocation/copy probes disabled')
    parser.add_argument('--http-control', action='store_true', help='emit/build same-size sync HTTP control without JSON business; never starts service or enters formal scores')
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
    if args.phase_ledger and args.phase_clocks:
        parser.error('choose phase clocks or allocation ledger, not both')
    if args.http_control and (args.phase_ledger or args.phase_clocks or args.binary or args.cpu_seconds or args.iterations):
        parser.error('--http-control is an independent build-only diagnostic')
    if args.emit_only and args.binary:
        parser.error('--emit-only and --binary are separate operations')
    source = args.generated_c.resolve(strict=True)
    template = HERE/'json_dense.c.inc';cases = make_cases()
    if args.http_control:
        original_hash=digest(source)
        wrapper, control = make_http_control_source(source.read_text(encoding='utf-8'), cases)
        out = args.output.resolve();out.mkdir(parents=True,exist_ok=False)
        path=out/'http-control.c';path.write_text(wrapper,encoding='utf-8')
        libraries=[str(path.resolve(strict=True)) for path in args.link]
        command=[args.cc,str(path),'-std=c11','-O2','-pthread','-lm',*libraries,'-o',str(out/'http-control')]
        control.update(source_commit=args.source_commit,cli_sha256=args.cli_sha256,
            generated_c_sha256=original_hash,control_c_sha256=digest(path),expected_compile_command=command,
            source_input_unchanged=digest(source)==original_hash)
        if not args.emit_only:
            environment=dict(os.environ)
            for key in ('CFLAGS','CXXFLAGS','LDFLAGS','LLVM_PROFILE_FILE'):environment.pop(key,None)
            logs=Commands(out/'logs',environment);logs.run('compile',command,out,timeout=180)
            control.update(binary_sha256=digest(out/'http-control'),commands=logs.rows)
        save(out/'emission.json',control)
        print(json.dumps({'emission':str(out/'emission.json'),'diagnostic_only':True,'normal_score_eligible':False}))
        return
    wrapper = emit(source, cases, template, args.phase_ledger or args.phase_clocks, not args.phase_clocks)
    identity = {'phase_clocks': args.phase_clocks, 'phase_ledger': args.phase_ledger, 'schema': SCHEMA, 'source_commit': args.source_commit, 'cli_sha256': args.cli_sha256,
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
                                                args.context, args.request_timeout_ms, args.handler_timeout_ms,
                                                'allocation' if args.phase_ledger else 'clocks' if args.phase_clocks else None)
        report['compiled_layout'] = validate_layout(report['measurement'].get('layout'))
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
