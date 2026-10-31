#!/usr/bin/env python3
"""Bounded Linux thread attribution, diagnostic only; no compiler/service launcher.

Prepare from an ordinary CLI Release emission, then link separately:
  python3 http_cost_diagnostic.py prepare server.c server-roles.c --source-commit SHA
  clang-18 server-roles.c -std=c11 -O2 -g -pthread /absolute/libpq.so \
      -Wl,--wrap=pthread_create -o server-roles
Keep the normal CLI binary for before/after controls and all scoring. The footer
can retain functions/change layout; its prefix identity is not .text identity.

Optional flow ledger (diagnostic only): prepare_ledger_source(normal, ledger),
then prepare_source(ledger, roles_ledger); normal controls remain uninstrumented.
The ledger emits one bounded KU_FLOW_DIAG JSON at normal joined shutdown.

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
    source, destination = Path(source), Path(destination)
    if source.resolve() == destination.resolve():
        raise ValueError('diagnostic output must not overwrite the normal emission')
    identity_path = destination.with_suffix(destination.suffix + '.identity.json')
    if destination.exists() or identity_path.exists():
        raise FileExistsError('preserve prior diagnostic files; choose a fresh destination')
    with source.open('rb') as stream:
        original = stream.read(32 * 1024 * 1024 + 1)
    if len(original) > 32 * 1024 * 1024:
        raise ValueError('generated C exceeds diagnostic input bound')
    text = original.decode('utf-8')
    if '\n/* BEGIN DIAGNOSTIC FOOTER: NOT A SCORE */\n' in text:
        raise ValueError('thread-role footer is already present')
    definitions = {}
    for key, (symbol, code, role) in ROLE_ENTRIES.items():
        pattern = r'\bstatic\s+void\s*\*\s*' + re.escape(symbol) + r'\s*\([^;{}]*\)\s*\{'
        definitions[key] = {'symbol': symbol, 'comm_code': code, 'role': role,
                            'definition_present': re.search(pattern, text) is not None}
    footer = Path(__file__).with_name('http_thread_roles.c.inc').read_bytes()
    if re.search(r'^#define KU_FLOW_LEDGER 1$', text, re.M):
        footer = ledger_role_footer(footer)
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


FLOW_SOURCES = ('other', 'queue_pop', 'job_begin', 'job_complete', 'job_cancelled',
                'worker_release', 'final_release', 'other_release', 'task_owner_self',
                'task_worker', 'task_other', 'task_coordinator', 'group_notify', 'net_register')

FLOW_SOURCE_METRICS = ('queue_notify', 'wake_notify', 'owner_local', 'cross_thread', 'pending_skip',
                       'write_attempt', 'write_success', 'write_eagain', 'write_eintr')

FLOW_COUNTERS = (
    'http_queue_pop', 'http_queue_push_signal', 'http_queue_wait',
    'http_job_begin', 'http_job_complete', 'http_job_cancelled',
    'http_release_final', 'http_release_nonfinal', 'http_release_worker',
    'http_owner_pending_already', 'http_owner_pending_take',
    'wake_notify', 'wake_pending_skip', 'wake_write_attempt', 'wake_write_success',
    'wake_write_eagain', 'wake_write_eintr', 'wake_write_error',
    'wake_drain', 'wake_drain_read_attempt', 'wake_drain_positive', 'wake_drain_bytes',
    'wake_drain_eagain', 'wake_drain_eintr', 'wake_drain_error', 'wake_drain_clear',
    'task_event_owner_self', 'task_event_worker', 'task_event_other', 'task_event_coordinator',
    'task_target_owner', 'task_target_coordinator', 'task_target_owner_absent',
    'net_snapshot', 'net_snapshot_items', 'net_same_position_same_interest',
    'net_move_only', 'net_move_and_interest_change', 'net_same_fd_identity_change',
    'net_new_fd', 'net_interest_change', 'net_same_fd_other_region', 'net_lookup_invalid', 'net_snapshot_detach',
    'epoll_add_attempt', 'epoll_add_success', 'epoll_mod_attempt', 'epoll_mod_success',
    'epoll_del_attempt', 'epoll_del_success', 'epoll_eintr', 'epoll_enoent', 'epoll_ebadf', 'epoll_other_error',
    'net_epoll_add_attempt', 'net_epoll_add_success', 'net_epoll_mod_attempt', 'net_epoll_mod_success',
    'net_epoll_del_attempt', 'net_epoll_del_success',
    'pg_step', 'pg_step_done', 'pg_step_not_done', 'pg_step_cancelled',
    'pg_existing_progress', 'pg_no_existing_progress', 'pg_phase_changed', 'pg_fd_changed',
    'pg_interest_changed', 'pg_row_cursor_changed', 'pg_result_changed',
    'pg_input_read', 'pg_input_write', 'pg_input_no_events', 'pg_input_other_events',
    'pg_interest_before_none', 'pg_interest_before_read', 'pg_interest_before_write',
    'pg_interest_before_read_write', 'pg_interest_before_other',
    'pg_interest_after_none', 'pg_interest_after_read', 'pg_interest_after_write',
    'pg_interest_after_read_write', 'pg_interest_after_other',
    'pg_completion', 'pg_completion_broadcast', 'pg_connect_broadcast',
    'blocking_worker_wake_no_work', 'blocking_signal_queued_zero', 'blocking_signal_queued_positive',
    'blocking_actual_broadcast', 'blocking_signal_skipped',
    'pg_phase_before_unknown', 'pg_phase_after_unknown',
) + tuple(f'pg_phase_{side}_{phase}' for side in ('before', 'after') for phase in range(1, 11))

FLOW_COUNTER_NAMES = FLOW_COUNTERS + tuple(f'{metric}_source_{source}'
    for source in FLOW_SOURCES for metric in FLOW_SOURCE_METRICS)

def _flow_c_prefix() -> str:
    constants = '\n'.join('  KUF_' + name.upper() + ',' for name in FLOW_COUNTERS)
    sources = '\n'.join('  KUF_SRC_' + name.upper() + ',' for name in FLOW_SOURCES)
    metrics = '\n'.join('  KUF_SM_' + name.upper() + ',' for name in FLOW_SOURCE_METRICS)
    return ('/* DIAGNOSTIC ONLY: TLS flow counters; not a scoring artifact. */\n'
            '#define KU_FLOW_LEDGER 1\n'
            '#if !defined(__linux__)\n#error "flow ledger is Linux-only"\n#endif\n'
            'enum {\n' + constants + '\n  KUF_SOURCE_BASE,\n  KUF_COUNT = KUF_SOURCE_BASE + '
            + str(len(FLOW_SOURCES) * len(FLOW_SOURCE_METRICS)) + '\n};\n'
            'enum {\n' + sources + '\n  KUF_SOURCE_COUNT\n};\n'
            'enum {\n' + metrics + '\n  KUF_SM_COUNT\n};\n' + r'''
static void ku_flow_count(unsigned int counter);
static void ku_flow_add(unsigned int counter,unsigned long long value);
static unsigned int ku_flow_source_push(unsigned int source);
static void ku_flow_source_restore(unsigned int previous);
static void ku_flow_source_count(unsigned int metric);
static int ku_flow_is_worker(void);
static unsigned int ku_flow_task_source(void* group,unsigned long long owner);
static void ku_flow_epoll(int operation,int status,int saved_errno);
static void ku_flow_net_snapshot(void* reactor,unsigned long long base,void* polls,
    unsigned long long net_base,void* interests,unsigned long long count);
static void ku_flow_thread_enter(const char* role,long tid);
static unsigned int ku_flow_thread_reserve(void);
static void ku_flow_thread_bind(unsigned int index,const char* role,long tid);
static void ku_flow_thread_abandon(unsigned int index);
static void ku_flow_thread_leave(void);
static void ku_flow_dump(void);
''')

FLOW_FOOTER = r'''
/* TLS-owned bank. No hot-path clock, allocation, logging or shared counter. */
#include <sys/syscall.h>
#include <stdarg.h>
#include <limits.h>
#include <stdatomic.h>
extern long syscall(long number,...);
#define KU_FLOW_SLOT_CAP 256u
typedef struct KuFlowSlot {
  _Alignas(64) unsigned long long values[KUF_COUNT];
  long tid;
  unsigned int role,overflow,creation_failed;
  _Atomic unsigned int ready,finished;
} KuFlowSlot;
static KuFlowSlot ku_flow_slots[KU_FLOW_SLOT_CAP] = {0};
static _Atomic unsigned long long ku_flow_next_slot = ATOMIC_VAR_INIT(0ULL);
static _Thread_local KuFlowSlot* ku_flow_local;
static _Thread_local unsigned int ku_flow_registered,ku_flow_source;
static unsigned int ku_flow_role(const char* role) {
  if(!strcmp(role,"kuS"))return 1u;
  if(!strcmp(role,"kuO"))return 2u;
  if(!strcmp(role,"kuB"))return 3u;
  if(!strcmp(role,"kuD"))return 4u;
  if(!strcmp(role,"kuW"))return 5u;
  if(!strcmp(role,"main"))return 6u;
  return 0u;
}
/* Reserve in the creating thread, before pthread_create can publish success.
 * A created but unscheduled child must not disappear from the final snapshot.
 * Exactly one registration RMW per attempt; the child only binds its slot. */
static unsigned int ku_flow_thread_reserve(void) {
  unsigned long long index=atomic_fetch_add_explicit(&ku_flow_next_slot,1ULL,memory_order_relaxed);
  return index<KU_FLOW_SLOT_CAP?(unsigned int)index:KU_FLOW_SLOT_CAP;
}
static void ku_flow_thread_bind(unsigned int index,const char* role,long tid) {
  if(ku_flow_registered)return;
  ku_flow_registered=1u;
  if(index>=KU_FLOW_SLOT_CAP)return;
  KuFlowSlot* slot=&ku_flow_slots[index];
  slot->tid=tid>0?tid:syscall(SYS_gettid);
  slot->role=ku_flow_role(role);ku_flow_local=slot;
  atomic_store_explicit(&slot->ready,1u,memory_order_release);
}
static void ku_flow_thread_enter(const char* role,long tid) {
  if(!ku_flow_registered)ku_flow_thread_bind(ku_flow_thread_reserve(),role,tid);
}
static void ku_flow_thread_abandon(unsigned int index) {
  if(index>=KU_FLOW_SLOT_CAP)return;
  KuFlowSlot* slot=&ku_flow_slots[index];slot->creation_failed=1u;
  atomic_store_explicit(&slot->ready,1u,memory_order_release);
  atomic_store_explicit(&slot->finished,1u,memory_order_release);
}
static void ku_flow_ensure(void) {
  if(!ku_flow_registered) {int saved_errno=errno;ku_flow_thread_enter("unknown",0);errno=saved_errno;}
}
static void ku_flow_add(unsigned int counter,unsigned long long value) {
  ku_flow_ensure();KuFlowSlot* slot=ku_flow_local;
  if(!slot || counter>=KUF_COUNT)return;
  unsigned long long previous=slot->values[counter];
  if(ULLONG_MAX-previous<value) {slot->values[counter]=ULLONG_MAX;slot->overflow=1u;}
  else slot->values[counter]=previous+value;
}
static void ku_flow_count(unsigned int counter) {ku_flow_add(counter,1u);}
static unsigned int ku_flow_source_push(unsigned int source) {
  unsigned int previous=ku_flow_source;
  ku_flow_source=source<KUF_SOURCE_COUNT?source:KUF_SRC_OTHER;return previous;
}
static void ku_flow_source_restore(unsigned int previous) {ku_flow_source=previous;}
static void ku_flow_source_count(unsigned int metric) {
  ku_flow_count(KUF_SOURCE_BASE+ku_flow_source*KUF_SM_COUNT+metric);
}
static int ku_flow_is_worker(void) {
  ku_flow_ensure();return ku_flow_local && (ku_flow_local->role==1u || ku_flow_local->role==3u);
}
static unsigned int ku_flow_task_source(void* raw,unsigned long long owner) {
  unsigned int source,counter;ku_flow_ensure();
  if(ku_http_current_loop && ku_http_current_loop->group==(KuHttpLoopGroup*)raw
      && (unsigned long long)ku_http_current_loop->index==owner) {
    source=KUF_SRC_TASK_OWNER_SELF;counter=KUF_TASK_EVENT_OWNER_SELF;
  } else if(ku_flow_is_worker()) {source=KUF_SRC_TASK_WORKER;counter=KUF_TASK_EVENT_WORKER;}
  else if(ku_flow_local && ku_flow_local->role==6u) {
    source=KUF_SRC_TASK_COORDINATOR;counter=KUF_TASK_EVENT_COORDINATOR;
  } else {source=KUF_SRC_TASK_OTHER;counter=KUF_TASK_EVENT_OTHER;}
  ku_flow_count(counter);return source;
}
static void ku_flow_epoll(int operation,int status,int saved_errno) {
  unsigned int counter=operation==EPOLL_CTL_ADD?KUF_EPOLL_ADD_ATTEMPT:
      operation==EPOLL_CTL_MOD?KUF_EPOLL_MOD_ATTEMPT:KUF_EPOLL_DEL_ATTEMPT;
  ku_flow_count(counter);if(!status)ku_flow_count(counter+1u);
  if(ku_flow_source==KUF_SRC_NET_REGISTER) {
    counter=operation==EPOLL_CTL_ADD?KUF_NET_EPOLL_ADD_ATTEMPT:
        operation==EPOLL_CTL_MOD?KUF_NET_EPOLL_MOD_ATTEMPT:KUF_NET_EPOLL_DEL_ATTEMPT;
    ku_flow_count(counter);if(!status)ku_flow_count(counter+1u);
  }
  if(status<0)ku_flow_count(saved_errno==EINTR?KUF_EPOLL_EINTR:
      saved_errno==ENOENT?KUF_EPOLL_ENOENT:saved_errno==EBADF?KUF_EPOLL_EBADF:KUF_EPOLL_OTHER_ERROR);
}
static void ku_flow_net_snapshot(void* raw,unsigned long long base,void* raw_polls,
    unsigned long long net_base,void* raw_interests,unsigned long long count) {
#if defined(KU_HTTP_HAS_TASK_NET) && defined(KU_HTTP_PERSISTENT_EVENT_LOOP)
  KuHttpReactor* r=(KuHttpReactor*)raw;KuHttpPollFd* polls=(KuHttpPollFd*)raw_polls;
  KuTaskNetPollV1* interests=(KuTaskNetPollV1*)raw_interests;
  ku_flow_count(KUF_NET_SNAPSHOT);ku_flow_add(KUF_NET_SNAPSHOT_ITEMS,count);
  for(size_t i=0;i<(size_t)count;i++) {
    KuHttpSocket fd=polls[(size_t)net_base+i].fd;
    size_t start=ku_http_reactor_fd_start(r,fd),found=SIZE_MAX;
    int invalid=0;
    /* Read-only equivalent lookup: do not set reactor fault or mutate its hash. */
    for(size_t n=0;n<r->fd_capacity;n++) {
      uint32_t stored=r->fd_entries[(start+n)&(r->fd_capacity-1u)];
      if(!stored)break;
      size_t at=(size_t)stored-1u;
      if(at>=r->capacity || !r->registrations[at].registered) {invalid=1;break;}
      if(r->registrations[at].fd==fd) {found=at;break;}
    }
    if(invalid) {ku_flow_count(KUF_NET_LOOKUP_INVALID);continue;}
    if(found==SIZE_MAX) {ku_flow_count(KUF_NET_NEW_FD);continue;}
    if(found<(size_t)base || found>=(size_t)base+r->old_net_count) {
      ku_flow_count(KUF_NET_SAME_FD_OTHER_REGION);continue;
    }
    KuHttpRegistration* old=&r->registrations[found];
    if(old->resource_slot!=interests[i].slot || old->generation!=interests[i].generation) {
      ku_flow_count(KUF_NET_SAME_FD_IDENTITY_CHANGE);continue;
    }
    int same_events=old->events==polls[(size_t)net_base+i].events;
    if(found!=(size_t)base+i)ku_flow_count(same_events?KUF_NET_MOVE_ONLY:KUF_NET_MOVE_AND_INTEREST_CHANGE);
    else ku_flow_count(same_events?KUF_NET_SAME_POSITION_SAME_INTEREST:KUF_NET_INTEREST_CHANGE);
  }
#else
  (void)raw;(void)base;(void)raw_polls;(void)net_base;(void)raw_interests;(void)count;
#endif
}
static void ku_flow_thread_leave(void) {
  KuFlowSlot* slot=ku_flow_local;ku_flow_local=NULL;
  if(slot)atomic_store_explicit(&slot->finished,1u,memory_order_release);
}
/* Only normal service exit calls this, after output finish has joined. Read
 * counters only after release/acquire completion publication; live slots are
 * skipped and make the receipt incomplete. Strong termination yields no ledger. */
static void ku_flow_dump(void) {
  static unsigned int printed;
  if(printed)return;printed=1u;
  static char buffer[KU_FLOW_SLOT_CAP*(KUF_COUNT*22u+512u)+32768u];
  size_t used=0;int failed=0;
#define KU_FLOW_APPEND(...) do { \
  int k=snprintf(buffer+used,sizeof(buffer)-used,__VA_ARGS__); \
  if(k<0 || (size_t)k>=sizeof(buffer)-used) {failed=1;goto flow_dump_done;} \
  used+=(size_t)k; \
} while(0)
  unsigned long long registered=atomic_load_explicit(&ku_flow_next_slot,memory_order_relaxed);
  unsigned int count=registered<KU_FLOW_SLOT_CAP?(unsigned int)registered:KU_FLOW_SLOT_CAP;
  unsigned int unfinished=0,unpublished=0,creation_failures=0,overflow=registered>KU_FLOW_SLOT_CAP;
  for(unsigned int i=0;i<count;i++) {
    KuFlowSlot* slot=&ku_flow_slots[i];
    if(!atomic_load_explicit(&slot->ready,memory_order_acquire)) {unpublished++;continue;}
    if(slot!=ku_flow_local && !atomic_load_explicit(&slot->finished,memory_order_acquire))unfinished++;
    else {if(slot->overflow)overflow=1u;if(slot->creation_failed)creation_failures++;}
  }
  KU_FLOW_APPEND("KU_FLOW_DIAG {\"schema\":\"ku-http-pg-flow-v1\",\"diagnostic_only\":true,\"complete\":%s,\"registered\":%llu,\"capacity\":%u,\"unfinished\":%u,\"unpublished\":%u,\"creation_failures\":%u,\"counter_bank_bytes\":%zu,\"dump_buffer_bytes\":%zu,\"counter_names\":[",
      !unfinished&&!unpublished&&!creation_failures&&!overflow?"true":"false",registered,KU_FLOW_SLOT_CAP,
      unfinished,unpublished,creation_failures,sizeof(ku_flow_slots),sizeof(buffer));
  /* COUNTER_NAMES */
  KU_FLOW_APPEND("],\"threads\":[");unsigned int emitted=0;
  for(unsigned int i=0;i<count;i++) {
    KuFlowSlot* slot=&ku_flow_slots[i];
    if(!atomic_load_explicit(&slot->ready,memory_order_acquire))continue;
    if(slot!=ku_flow_local && !atomic_load_explicit(&slot->finished,memory_order_acquire))continue;
    KU_FLOW_APPEND("%s{\"slot\":%u,\"tid\":%ld,\"role\":%u,\"finished\":%s,\"current_snapshot\":%s,\"creation_failed\":%s,\"saturated\":%s,\"values\":[",
        emitted++?",":"",i,slot->tid,slot->role,slot==ku_flow_local?"false":"true",
        slot==ku_flow_local?"true":"false",slot->creation_failed?"true":"false",slot->overflow?"true":"false");
    if(slot->overflow)overflow=1u;
    for(unsigned int n=0;n<KUF_COUNT;n++)KU_FLOW_APPEND("%s%llu",n?",":"",slot->values[n]);
    KU_FLOW_APPEND("]}");
  }
  KU_FLOW_APPEND("],\"overflow\":%s,\"role_names\":[\"unknown\",\"http_sync_handler\",\"http_owner\",\"task_blocking\",\"task_driver\",\"output\",\"process_main\"]}\n",overflow?"true":"false");
flow_dump_done:
  if(!failed)(void)fwrite(buffer,1,used,stderr);
  else {const char failure[]="KU_FLOW_DIAG {\"complete\":false,\"dump_overflow\":true}\n";(void)fwrite(failure,1,sizeof(failure)-1u,stderr);}
#undef KU_FLOW_APPEND
}
'''

def ledger_role_footer(footer: bytes) -> bytes:
    """Existing prepare_source calls this only for an input carrying the marker."""
    replacements = (
        (b'  const char* role;\n', b'  const char* role;\n  unsigned int flow_slot;\n'),
        (b'  errno = previous_errno;\n  return start.entry(start.argument);\n',
         b'  ku_flow_thread_bind(start.flow_slot,start.role,tid);\n  errno = previous_errno;\n'
         b'  void* result = start.entry(start.argument);\n'
         b'  ku_flow_thread_leave();\n  return result;\n'),
        (b'  *start = (KuCostThreadStart){entry, argument, ku_cost_thread_role(entry)};\n',
         b'  *start = (KuCostThreadStart){entry, argument, ku_cost_thread_role(entry),ku_flow_thread_reserve()};\n'),
        (b'  if (status) free(start);\n',
         b'  if (status) {ku_flow_thread_abandon(start->flow_slot);free(start);}\n'),
    )
    for old, new in replacements:
        if footer.count(old) != 1:
            raise ValueError('role footer registration/lifecycle anchor is not unique')
        footer = footer.replace(old, new, 1)
    return footer

def prepare_ledger_source(source: Path, destination: Path, source_commit: str | None = None) -> dict:
    source, destination = Path(source), Path(destination)
    if source.resolve() == destination.resolve():
        raise ValueError('ledger must not overwrite the normal C emission')
    identity = destination.with_suffix(destination.suffix + '.identity.json')
    if destination.exists() or identity.exists():
        raise FileExistsError('choose a fresh ledger destination')
    with source.open('rb') as stream:
        original = stream.read(32 * 1024 * 1024 + 1)
    if len(original) > 32 * 1024 * 1024:
        raise ValueError('generated C exceeds 32MiB input limit')
    text = original.decode('utf-8')
    normal_text = text
    if (re.search(r'^#define KU_FLOW_LEDGER 1$', text, re.M)
            or '\n/* BEGIN DIAGNOSTIC FOOTER: NOT A SCORE */\n' in text):
        raise ValueError('input must be a normal CLI emission, not a diagnostic C')
    anchors = []
    def replace(name: str, old: str, new: str, optional: bool = False):
        nonlocal text
        count = text.count(old)
        if count != 1:
            if optional and count == 0:
                anchors.append({'name': name, 'present': False}); return False
            raise ValueError(f'{name}: expected exactly one anchor, found {count}')
        normal_offset = normal_text.find(old)
        if normal_offset < 0 or normal_text.count(old) != 1:
            raise ValueError(f'{name}: normal-input location is not unique')
        anchors.append({'name': name, 'present': True, 'normal_line': normal_text[:normal_offset].count('\n')+1,
                        'anchor_sha256': hashlib.sha256(old.encode()).hexdigest()})
        text = text.replace(old, new, 1); return True
    def tagged(call: str, source_name: str, counter: str = '') -> str:
        return ('{ unsigned int ku_flow_saved=ku_flow_source_push(KUF_SRC_' + source_name + '); '
                + ('ku_flow_count(KUF_' + counter + '); ' if counter else '')
                + call + ' ku_flow_source_restore(ku_flow_saved); }')

    replace('main-register', 'int main(void) {\n', 'int main(void) {\n  ku_flow_thread_enter("main",0);\n')
    replace('output-joined-dump', '  if (ku_service_output_snapshot().failures) _Exit(code);\n  return code;',
            '  if (ku_service_output_snapshot().failures) _Exit(code);\n  ku_flow_dump();\n  return code;')
    replace('queue-notify', '  if(ku_http_queue_is_current_owner(queue)) queue->owner_work_pending=1;\n  else ku_http_wake_notify(queue->wake);',
            '  ku_flow_source_count(KUF_SM_QUEUE_NOTIFY);\n'
            '  if(ku_http_queue_is_current_owner(queue)) {\n'
            '    ku_flow_source_count(KUF_SM_OWNER_LOCAL);\n'
            '    if(queue->owner_work_pending)ku_flow_count(KUF_HTTP_OWNER_PENDING_ALREADY);\n'
            '    queue->owner_work_pending=1;\n'
            '  } else {ku_flow_source_count(KUF_SM_CROSS_THREAD);ku_http_wake_notify(queue->wake);}')
    replace('owner-pending-take', '  int pending=queue->owner_work_pending;queue->owner_work_pending=0;return pending;',
            '  int pending=queue->owner_work_pending;queue->owner_work_pending=0;\n'
            '  if(pending)ku_flow_count(KUF_HTTP_OWNER_PENDING_TAKE);return pending;')
    gated_queue='static void ku_http_queue_publish_locked(' in text
    pop_call='ku_http_queue_publish_locked(queue,KU_HTTP_QUEUE_CLAIM,0);' if gated_queue else 'ku_http_queue_notify(queue);'
    begin_call='ku_http_queue_publish(job->queue,KU_HTTP_QUEUE_BEGIN,job->handler_deadline);' if gated_queue else 'ku_http_queue_notify(job->queue);'
    complete_call='ku_http_queue_publish(job->queue,KU_HTTP_QUEUE_COMPLETE,0);' if gated_queue else 'ku_http_queue_notify(job->queue);'
    release_call='ku_http_queue_publish(queue,KU_HTTP_QUEUE_RETIRE,0);' if gated_queue else 'ku_http_queue_notify(queue);'
    if gated_queue:
        replace('queue-pop','  if (job) '+pop_call,'  if (job) '+tagged(pop_call,'QUEUE_POP','HTTP_QUEUE_POP'))
    else:
        replace('queue-pop', '  if (job) '+pop_call+'\n  return job;',
                '  if (job) ' + tagged(pop_call, 'QUEUE_POP', 'HTTP_QUEUE_POP') + '\n  return job;')
    replace('queue-push-condition-signal', '    ku_http_condition_signal(&queue->not_empty);',
            '    ku_flow_count(KUF_HTTP_QUEUE_PUSH_SIGNAL);ku_http_condition_signal(&queue->not_empty);')
    replace('queue-pop-condition-wait', '  while (queue->count == 0 && !queue->closed) ku_http_condition_wait(&queue->not_empty, &queue->lock);',
            '  while (queue->count == 0 && !queue->closed) {ku_flow_count(KUF_HTTP_QUEUE_WAIT);ku_http_condition_wait(&queue->not_empty, &queue->lock);}')
    replace('job-begin', '  job->started = 1;\n  ku_http_mutex_unlock(&job->lock);\n  '+begin_call,
            '  job->started = 1;\n  ku_http_mutex_unlock(&job->lock);\n  '
            + tagged(begin_call, 'JOB_BEGIN', 'HTTP_JOB_BEGIN'))
    replace('job-cancelled', '  if (cancelled) { ku_http_input_drop(&job->input); '+complete_call+' return; }',
            '  if (cancelled) { ku_http_input_drop(&job->input); '
            + tagged(complete_call, 'JOB_CANCELLED', 'HTTP_JOB_CANCELLED') + ' return; }')
    replace('job-complete', '  job->completed = 1;\n  ku_http_mutex_unlock(&job->lock);\n  '+complete_call,
            '  job->completed = 1;\n  ku_http_mutex_unlock(&job->lock);\n  '
            + tagged(complete_call, 'JOB_COMPLETE', 'HTTP_JOB_COMPLETE'))
    replace('job-release-result', '  if (ku_refcount_release(&job->owners, "http job")) {',
            '  int ku_flow_final=ku_refcount_release(&job->owners, "http job");\n'
            '  int ku_flow_worker=ku_flow_is_worker();\n'
            '  if(ku_flow_worker)ku_flow_count(KUF_HTTP_RELEASE_WORKER);\n'
            '  ku_flow_count(ku_flow_final?KUF_HTTP_RELEASE_FINAL:KUF_HTTP_RELEASE_NONFINAL);\n'
            '  if (ku_flow_final) {')
    replace('job-release-notify', '  /* Release publication precedes wake, including the non-final worker owner.\n'
            '     ClosedWaitingJob can now see the actual remaining owner count. */\n  '+release_call,
            '  /* Original release has completed; only the cached queue is used. */\n'
            '  unsigned int ku_flow_saved=ku_flow_source_push(ku_flow_final?KUF_SRC_FINAL_RELEASE:\n'
            '      ku_flow_worker?KUF_SRC_WORKER_RELEASE:KUF_SRC_OTHER_RELEASE);\n'
            '  '+release_call+'ku_flow_source_restore(ku_flow_saved);')
    replace('wake-notify-entry', 'static void ku_http_wake_notify(KuHttpWake* wake) {\n',
            'static void ku_http_wake_notify(KuHttpWake* wake) {\n'
            '  ku_flow_count(KUF_WAKE_NOTIFY);ku_flow_source_count(KUF_SM_WAKE_NOTIFY);\n')
    replace('pending-exchange-once', '  if (ku_http_wake_pending_exchange(wake,1)) return;',
            '  if (ku_http_wake_pending_exchange(wake,1)) {\n'
            '    ku_flow_count(KUF_WAKE_PENDING_SKIP);ku_flow_source_count(KUF_SM_PENDING_SKIP);return;\n  }')
    wake_counter_transport='write(wake->write_socket, marker, marker_size)' in text
    wake_write='write(wake->write_socket, marker, marker_size)' if wake_counter_transport else 'write(wake->write_socket, &byte, 1)'
    wake_write_size='(int)marker_size' if wake_counter_transport else '1'
    replace('wake-physical-write', '    ssize_t sent = '+wake_write+';',
            '    ku_flow_count(KUF_WAKE_WRITE_ATTEMPT);ku_flow_source_count(KUF_SM_WRITE_ATTEMPT);\n'
            '    ssize_t sent = '+wake_write+';')
    replace('wake-write-result', '    if (sent == '+wake_write_size+') return;\n'
            '    int error = ku_http_socket_last_error();\n'
            '    if (sent < 0 && ku_http_socket_would_block(error)) return;\n'
            '    if (sent < 0 && ku_http_socket_error_interrupted(error)) continue;',
            '    if (sent == '+wake_write_size+') {ku_flow_count(KUF_WAKE_WRITE_SUCCESS);ku_flow_source_count(KUF_SM_WRITE_SUCCESS);return;}\n'
            '    int error = ku_http_socket_last_error();\n'
            '    if (sent < 0 && ku_http_socket_would_block(error)) {ku_flow_count(KUF_WAKE_WRITE_EAGAIN);ku_flow_source_count(KUF_SM_WRITE_EAGAIN);return;}\n'
            '    if (sent < 0 && ku_http_socket_error_interrupted(error)) {ku_flow_count(KUF_WAKE_WRITE_EINTR);ku_flow_source_count(KUF_SM_WRITE_EINTR);continue;}\n'
            '    ku_flow_count(KUF_WAKE_WRITE_ERROR);')
    replace('wake-drain-entry', 'static void ku_http_wake_drain(KuHttpWake* wake) {\n',
            'static void ku_http_wake_drain(KuHttpWake* wake) {\n  ku_flow_count(KUF_WAKE_DRAIN);\n')
    wake_read='read(wake->read_socket, bytes, read_size)' if wake_counter_transport else 'read(wake->read_socket, bytes, sizeof(bytes))'
    replace('wake-drain-read', '    ssize_t received = '+wake_read+';',
            '    ku_flow_count(KUF_WAKE_DRAIN_READ_ATTEMPT);\n'
            '    ssize_t received = '+wake_read+';')
    replace('wake-drain-positive', '    if (received > 0) continue;',
            '    if (received > 0) {ku_flow_count(KUF_WAKE_DRAIN_POSITIVE);ku_flow_add(KUF_WAKE_DRAIN_BYTES,(unsigned long long)received);continue;}')
    replace('wake-counter-read-width-error','      fputs("http reactor event counter read size failed\\n",stderr);exit(1);',
            '      ku_flow_count(KUF_WAKE_DRAIN_ERROR);fputs("http reactor event counter read size failed\\n",stderr);exit(1);',optional=True)
    replace('wake-drain-result', '    if (ku_http_socket_would_block(error)) break;\n'
            '    if (ku_http_socket_error_interrupted(error)) continue;\n'
            '    fputs("http reactor wake read failed\\n", stderr); exit(1);',
            '    if (ku_http_socket_would_block(error)) {ku_flow_count(KUF_WAKE_DRAIN_EAGAIN);break;}\n'
            '    if (ku_http_socket_error_interrupted(error)) {ku_flow_count(KUF_WAKE_DRAIN_EINTR);continue;}\n'
            '    ku_flow_count(KUF_WAKE_DRAIN_ERROR);\n'
            '    fputs("http reactor wake read failed\\n", stderr); exit(1);')
    replace('wake-drain-clear-once', '  (void)ku_http_wake_pending_exchange(wake,0);',
            '  (void)ku_http_wake_pending_exchange(wake,0);ku_flow_count(KUF_WAKE_DRAIN_CLEAR);')
    replace('group-notify-source', 'static void ku_http_group_notify(KuHttpLoopGroup* group) {\n',
            'static void ku_http_group_notify(KuHttpLoopGroup* group) {\n'
            '  unsigned int ku_flow_saved=ku_flow_source_push(KUF_SRC_GROUP_NOTIFY);\n')
    replace('group-notify-restore', '  ku_http_wake_notify(&group->wake);\n}\n\n\n#if defined(KU_HTTP_HAS_TASK_RUNTIME)',
            '  ku_http_wake_notify(&group->wake);\n  ku_flow_source_restore(ku_flow_saved);\n}\n\n\n#if defined(KU_HTTP_HAS_TASK_RUNTIME)')
    replace('task-event-source', 'static void ku_http_task_event_wake(void* context, size_t owner) {\n',
            'static void ku_http_task_event_wake(void* context, size_t owner) {\n'
            '  unsigned int ku_flow_saved=ku_flow_source_push(ku_flow_task_source(context,(unsigned long long)owner));\n')
    owner_targets = (
        ('cross_thread_wake',
         '  if (owner < (size_t)group->created && group->loops[owner].wake) ku_http_wake_notify(group->loops[owner].wake);',
         '  if (owner < (size_t)group->created && group->loops[owner].wake) {\n'
         '    ku_flow_count(KUF_TASK_TARGET_OWNER);ku_http_wake_notify(group->loops[owner].wake);\n'
         '  } else ku_flow_count(KUF_TASK_TARGET_OWNER_ABSENT);'),
        ('owner_local_pending',
         '  if (owner < (size_t)group->created && group->loops[owner].wake) {\n'
         '    KuHttpLoopOwner* target=&group->loops[owner];\n'
         '    if(ku_http_current_loop==target && ku_http_current_loop->group==group\n'
         '        && ku_http_current_loop->index==(int)owner) target->task_work_pending=1;\n'
         '    else ku_http_wake_notify(target->wake);\n  }',
         '  if (owner < (size_t)group->created && group->loops[owner].wake) {\n'
         '    ku_flow_count(KUF_TASK_TARGET_OWNER);\n'
         '    KuHttpLoopOwner* target=&group->loops[owner];\n'
         '    if(ku_http_current_loop==target && ku_http_current_loop->group==group\n'
         '        && ku_http_current_loop->index==(int)owner) target->task_work_pending=1;\n'
         '    else ku_http_wake_notify(target->wake);\n  } else ku_flow_count(KUF_TASK_TARGET_OWNER_ABSENT);'),
    )
    if sum(text.count(old) for _, old, _ in owner_targets) != 1:
        raise ValueError('task-owner-target: expected exactly one supported legacy/owner-local target anchor')
    task_owner_notify_mode, old_target, new_target = next(
        (mode, old, new) for mode, old, new in owner_targets if old in text)
    replace('task-owner-target', old_target, new_target)
    anchors[-1]['source_variant'] = task_owner_notify_mode
    replace('task-coordinator-target', '  if (notify_coordinator) ku_http_wake_notify(&group->wake);\n}',
            '  if (notify_coordinator) {ku_flow_count(KUF_TASK_TARGET_COORDINATOR);ku_http_wake_notify(&group->wake);}\n'
            '  ku_flow_source_restore(ku_flow_saved);\n}')
    replace('epoll-actual-attempt', '  do { status=epoll_ctl(r->descriptor,operation,fd,remove?NULL:&event); }',
            '  do { status=epoll_ctl(r->descriptor,operation,fd,remove?NULL:&event);\n'
            '    int ku_flow_saved_errno=errno;ku_flow_epoll(operation,status,ku_flow_saved_errno);errno=ku_flow_saved_errno;\n  }')
    replace('net-snapshot-before-detach', '    for(size_t i=0;i<reactor.old_net_count;i++) {',
            '    ku_flow_net_snapshot(&reactor,(unsigned long long)slot_count+3u,poll_items,\n'
            '        (unsigned long long)net_base,net_interests,(unsigned long long)net_count);\n'
            '    unsigned int ku_flow_register_saved=ku_flow_source_push(KUF_SRC_NET_REGISTER);\n'
            '    for(size_t i=0;i<reactor.old_net_count;i++) {')
    replace('net-snapshot-detach', '          || entry->generation!=net_interests[i].generation || entry->resource_slot!=net_interests[i].slot)\n'
            '        ku_http_reactor_delete(&reactor,(size_t)slot_count+3u+i);',
            '          || entry->generation!=net_interests[i].generation || entry->resource_slot!=net_interests[i].slot) {\n'
            '        ku_flow_count(KUF_NET_SNAPSHOT_DETACH);\n'
            '        ku_http_reactor_delete(&reactor,(size_t)slot_count+3u+i);\n      }')
    replace('net-snapshot-restore', '    reactor.old_net_count=net_count;',
            '    reactor.old_net_count=net_count;ku_flow_source_restore(ku_flow_register_saved);')

    blocking = 'static void ku_task_blocking_worker(KuTaskBlockingWorkerV1* worker) {' in text
    if blocking:
        gated_signal='  if (!pool->queued && !pool->closing && !pool->state_waiters) return;' in text
        replace('blocking-signal-queue-state', 'static void ku_task_blocking_signal(KuTaskBlockingPoolV1* pool) {\n',
                'static void ku_task_blocking_signal(KuTaskBlockingPoolV1* pool) {\n'
                '  ku_flow_count(pool->queued?KUF_BLOCKING_SIGNAL_QUEUED_POSITIVE:KUF_BLOCKING_SIGNAL_QUEUED_ZERO);\n'
                +('' if gated_signal else '  ku_flow_count(KUF_BLOCKING_ACTUAL_BROADCAST);\n'))
        if gated_signal:
            replace('blocking-signal-actual','  if (!pool->queued && !pool->closing && !pool->state_waiters) return;',
                '  if (!pool->queued && !pool->closing && !pool->state_waiters) {ku_flow_count(KUF_BLOCKING_SIGNAL_SKIPPED);return;}\n'
                '  ku_flow_count(KUF_BLOCKING_ACTUAL_BROADCAST);')
        worker_wait='ku_task_blocking_wait_inner' if 'static int ku_task_blocking_wait_inner(' in text else 'ku_task_blocking_wait'
        replace('blocking-worker-wake-no-work', '      if ('+worker_wait+'(pool,UINT64_MAX)<0) {\n'
                '        pool->fault=KU_TASK_DRIVER_INTERNAL; ku_task_blocking_set_closing_locked(pool,1u); break;\n      }\n'
                '    }\n    if (!pool->queued && pool->closing) break;',
                '      if ('+worker_wait+'(pool,UINT64_MAX)<0) {\n'
                '        pool->fault=KU_TASK_DRIVER_INTERNAL; ku_task_blocking_set_closing_locked(pool,1u); break;\n      }\n'
                '      if(!pool->queued && !pool->closing)ku_flow_count(KUF_BLOCKING_WORKER_WAKE_NO_WORK);\n'
                '    }\n    if (!pool->queued && pool->closing) break;')

    pg = 'typedef struct KuPgAsyncOp {' in text
    if pg:
        replace('pg-step-before', '  op->diagnostic_progress=0;\n  int done=ku_pg_async_step_inner(op,events,now,cancelled);', r'''  int ku_flow_before_phase=op->phase,ku_flow_before_fd=op->conn?PQsocket(op->conn):-1;
  unsigned int ku_flow_before_interest=op->interest;
  uintptr_t ku_flow_before_row=(uintptr_t)op->row,ku_flow_before_output=(uintptr_t)op->output.value;
  size_t ku_flow_before_col=op->row_col,ku_flow_before_offset=op->row_offset,ku_flow_before_bytes=op->row_bytes;
  size_t ku_flow_before_rows=op->query.value?op->query.value->rows:0;
  size_t ku_flow_before_result_bytes=op->query.value?op->query.value->bytes_len:0;
  int ku_flow_before_row_phase=op->row_phase,ku_flow_before_ok=op->output.ok;
  ku_flow_count(KUF_PG_STEP);if(cancelled)ku_flow_count(KUF_PG_STEP_CANCELLED);
  ku_flow_count(ku_flow_before_interest<=3u?KUF_PG_INTEREST_BEFORE_NONE+ku_flow_before_interest:KUF_PG_INTEREST_BEFORE_OTHER);
  ku_flow_count(ku_flow_before_phase>=1&&ku_flow_before_phase<=10?
      KUF_PG_PHASE_BEFORE_1+(unsigned int)ku_flow_before_phase-1u:KUF_PG_PHASE_BEFORE_UNKNOWN);
  if(!events)ku_flow_count(KUF_PG_INPUT_NO_EVENTS);
  /* Existing PG step ABI uses 1u=read,2u=write, before Task ABI declarations. */
  if(events&1u)ku_flow_count(KUF_PG_INPUT_READ);
  if(events&2u)ku_flow_count(KUF_PG_INPUT_WRITE);
  if(events&~3u)ku_flow_count(KUF_PG_INPUT_OTHER_EVENTS);
  op->diagnostic_progress=0;
  int done=ku_pg_async_step_inner(op,events,now,cancelled);
  ku_flow_count(done?KUF_PG_STEP_DONE:KUF_PG_STEP_NOT_DONE);
  ku_flow_count(op->diagnostic_progress?KUF_PG_EXISTING_PROGRESS:KUF_PG_NO_EXISTING_PROGRESS);
  ku_flow_count(op->interest<=3u?KUF_PG_INTEREST_AFTER_NONE+op->interest:KUF_PG_INTEREST_AFTER_OTHER);
  ku_flow_count(op->phase>=1&&op->phase<=10?KUF_PG_PHASE_AFTER_1+(unsigned int)op->phase-1u:KUF_PG_PHASE_AFTER_UNKNOWN);
  if(op->phase!=ku_flow_before_phase)ku_flow_count(KUF_PG_PHASE_CHANGED);
  if((op->conn?PQsocket(op->conn):-1)!=ku_flow_before_fd)ku_flow_count(KUF_PG_FD_CHANGED);
  if(op->interest!=ku_flow_before_interest)ku_flow_count(KUF_PG_INTEREST_CHANGED);
  if((uintptr_t)op->row!=ku_flow_before_row || op->row_col!=ku_flow_before_col || op->row_offset!=ku_flow_before_offset
      || op->row_bytes!=ku_flow_before_bytes || op->row_phase!=ku_flow_before_row_phase)ku_flow_count(KUF_PG_ROW_CURSOR_CHANGED);
  if(op->output.ok!=ku_flow_before_ok || (uintptr_t)op->output.value!=ku_flow_before_output
      || (op->query.value?op->query.value->rows:0)!=ku_flow_before_rows
      || (op->query.value?op->query.value->bytes_len:0)!=ku_flow_before_result_bytes)ku_flow_count(KUF_PG_RESULT_CHANGED);''')
        replace('pg-connect-broadcast', '    if(pool->queued>pool->peak_queued)pool->peak_queued=pool->queued;\n'
                '    ku_task_blocking_signal(pool);\n  }\n#endif\n  if(done) {',
                '    if(pool->queued>pool->peak_queued)pool->peak_queued=pool->queued;\n'
                '    ku_flow_count(KUF_PG_CONNECT_BROADCAST);ku_task_blocking_signal(pool);\n  }\n#endif\n  if(done) {')
        replace('pg-completion-broadcast', '    if(status!=KU_TASK_DRIVER_OK)pool->fault=KU_TASK_DRIVER_INTERNAL;\n'
                '    ku_task_blocking_signal(pool);\n  }\n  ku_task_blocking_unlock(pool);return KU_TASK_DRIVER_OK;',
                '    if(status!=KU_TASK_DRIVER_OK)pool->fault=KU_TASK_DRIVER_INTERNAL;\n'
                '    if(job->kind==9u) {ku_flow_count(KUF_PG_COMPLETION);ku_flow_count(KUF_PG_COMPLETION_BROADCAST);}\n'
                '    ku_task_blocking_signal(pool);\n  }\n  ku_task_blocking_unlock(pool);return KU_TASK_DRIVER_OK;')
    footer = FLOW_FOOTER.replace('  /* COUNTER_NAMES */',
        '\n'.join('  KU_FLOW_APPEND("' + (',' if i else '') + '\\\"' + name + '\\\"");'
                  for i, name in enumerate(FLOW_COUNTER_NAMES)))
    output = (_flow_c_prefix() + text + '\n/* BEGIN FLOW LEDGER FOOTER: NOT A SCORE */\n' + footer).encode()
    receipt = {'schema': 'ku-http-pg-flow-emission-v1', 'diagnostic_only': True,
        'source_commit': source_commit, 'normal_generated_c': str(source),
        'normal_generated_c_bytes': len(original), 'normal_generated_c_sha256': hashlib.sha256(original).hexdigest(),
        'ledger_c_sha256': hashlib.sha256(output).hexdigest(), 'pg_present': pg, 'blocking_present': blocking,
        'task_owner_notify_mode': task_owner_notify_mode,
        'slot_capacity': 256, 'counter_count': len(FLOW_COUNTER_NAMES),
        'counter_names': FLOW_COUNTER_NAMES, 'anchors': anchors,
        'required_next_step': 'existing prepare_source with marker-gated ledger_role_footer; then --wrap=pthread_create',
        'scope': 'whole service lifetime including startup/readiness/warmup/drain, not per-request timing or scoring',
        'limitations': ['Counters do not measure CPU time or causality; zero/many calls alone cannot establish a hotspot.',
            'NET snapshot counters cover Task NET interests, not exclusively PG. PG workload identity is external.',
            'Snapshot adds one read-only bounded fd-hash lookup per interest; PG adds two PQsocket accessors per step.',
            'No dump on signal/forced exit/output failure; normal joined shutdown required. Live counter slots are never read.',
            'Wrapped entry roles only; DSO-internal threads can be unknown. Slot/saturation/unfinished flags invalidate closure.',
            'Ledger bank/footer change memory and layout; use independent normal controls and do not score diagnostic ELF.']}
    destination.parent.mkdir(parents=True, exist_ok=True)
    destination.write_bytes(output)
    identity.write_text(json.dumps(receipt, ensure_ascii=False, indent=2)+'\n')
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
