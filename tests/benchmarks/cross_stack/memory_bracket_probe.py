"""Temporary diagnostic observer; preserves every original Domain.snapshot field.

Usage in the existing ordinary baseline diagnostic harness:
    with MemoryBracketProbe(linux_harness, output) as probe:
        row = focus_cycle.window(existing_plan, 'B', 'db', 200, 3, 15, ...)

The service/ELF/SQL/limits remain unchanged. This observer adds file reads and
optional private tracefs collection, so the window is NOT a performance score.
No reclaim, cache drop, stock flush, process freeze, or memory-limit change.
"""
import hashlib
import json
import os
from pathlib import Path
import platform
import threading
import time


def _save(path, value):
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(value, indent=2, sort_keys=True) + '\n')


class MemoryBracketProbe:
    MAX_SNAPSHOTS = 512
    MAX_DOMAINS = 8
    MAX_THREADS = 8
    MAX_TRACE_BYTES = 512 << 10
    MAX_TRACE_EVENTS = 2048

    def __init__(self, module, output, trace=True):
        self.module, self.output = module, Path(output)
        self.trace_requested = trace
        self.local = threading.local()
        self.lock = threading.Lock()
        self.count = self.overflow = 0
        self.domains, self.threads = {}, set()
        self.trace_path = None
        self.trace_info = {'requested': trace, 'available': False}

    def _plain_read(self, path, limit):
        before = time.monotonic_ns()
        text = self.original_read_text(path)
        after = time.monotonic_ns()
        if len(text.encode()) > limit:
            raise ValueError('diagnostic file exceeds bound: ' + path.name)
        return {'before_ns': before, 'after_ns': after, 'text': text}

    def _current(self, context):
        try:
            result = self._plain_read(context['current_path'], 128)
            result['value'] = int(result.pop('text').strip())
            return result
        except (OSError, ValueError) as error:
            return {'error': repr(error), 'after_ns': time.monotonic_ns()}

    def _set_trace_filter(self):
        if self.trace_path is None:
            return
        terms = ['force == 1'] + ['common_pid == ' + str(tid) for tid in sorted(self.threads)]
        try:
            (self.trace_path / 'events/memcg/memcg_flush_stats/filter').write_text(' || '.join(terms))
        except OSError as error:
            self.trace_info['filter_error'] = repr(error)

    def _start_trace(self):
        if not self.trace_requested:
            return
        if (os.cpu_count() or 0) > 32:
            self.trace_info['unavailable_reason'] = 'more than32 online CPUs; refuse oversized perCPU trace buffers'
            return
        # A private instance only. Never mount tracefs or alter the global tracer.
        for base in (Path('/sys/kernel/tracing'), Path('/sys/kernel/debug/tracing')):
            if not (base / 'events/memcg/memcg_flush_stats/format').is_file():
                continue
            try:
                instance = base / 'instances' / ('ku-memory-bracket-' + str(os.getpid()))
                instance.mkdir()
                self.trace_path = instance
                fmt = self.original_read_text(base / 'events/memcg/memcg_flush_stats/format')
                if not all(name in fmt for name in ('stats_updates', 'needs_flush', 'force')):
                    raise ValueError('unexpected memcg_flush_stats format')
                self.trace_info.update(format=fmt, format_sha256=hashlib.sha256(fmt.encode()).hexdigest())
                (instance / 'tracing_on').write_text('0')
                (instance / 'buffer_size_kb').write_text('16')
                clocks = self.original_read_text(instance / 'trace_clock')
                if 'mono' in clocks.replace('[', '').replace(']', '').split():
                    (instance / 'trace_clock').write_text('mono')
                self.trace_info['clock'] = self.original_read_text(instance / 'trace_clock').strip()
                self._set_trace_filter()
                (instance / 'events/memcg/memcg_flush_stats/enable').write_text('1')
                (instance / 'tracing_on').write_text('1')
                self.trace_info.update(available=True, private_instance=str(instance), buffer_kib_per_CPU=16)
                return
            except (OSError, ValueError) as error:
                self.trace_info['unavailable_reason'] = repr(error)
                self._stop_trace()
                return
        self.trace_info['unavailable_reason'] = 'memcg_flush_stats tracefs event not readable; no mount/permission change attempted'

    def _stop_trace(self):
        instance = self.trace_path
        if instance is None:
            return
        try:
            (instance / 'tracing_on').write_text('0')
            (instance / 'events/memcg/memcg_flush_stats/enable').write_text('0')
            with (instance / 'trace').open('rb') as stream:
                data = stream.read(self.MAX_TRACE_BYTES + 1)
            self.trace_info['output_truncated'] = len(data) > self.MAX_TRACE_BYTES
            kept=[];events=0
            for line in data[:self.MAX_TRACE_BYTES].splitlines(keepends=True):
                if not line.startswith(b'#'):
                    events+=1
                    if events>self.MAX_TRACE_EVENTS:continue
                kept.append(line)
            self.trace_info.update(events_seen_in_bounded_read=events,
                event_limit=self.MAX_TRACE_EVENTS,event_limit_exceeded=events>self.MAX_TRACE_EVENTS)
            (self.output / 'memcg-flush-trace.txt').write_bytes(b''.join(kept))
            stats = {}
            for path in sorted((instance / 'per_cpu').glob('cpu*/stats'))[:256]:
                stats[path.parent.name] = self.original_read_text(path)[:4096]
            self.trace_info['per_CPU_stats'] = stats
        except OSError as error:
            self.trace_info['stop_or_read_error'] = repr(error)
        finally:
            try:
                instance.rmdir()
                self.trace_info['private_instance_removed'] = True
            except OSError as error:
                self.trace_info['private_instance_removed'] = False
                self.trace_info['remove_error'] = repr(error)
            self.trace_path = None

    def _read_text(self, path, *args, **kwargs):
        context = getattr(self.local, 'context', None)
        if context is None or path.parent != context['domain_path']:
            return self.original_read_text(path, *args, **kwargs)
        if path.name == 'memory.stat':
            context['stat_current_before'] = self._current(context)
            before = time.monotonic_ns()
            result = self.original_read_text(path, *args, **kwargs)
            after = time.monotonic_ns()
            context['stat_read'] = {'before_ns': before, 'after_ns': after, 'thread_id': threading.get_native_id()}
            context['stat_current_after'] = self._current(context)
            return result
        if path.name == 'memory.current':
            before = time.monotonic_ns()
            result = self.original_read_text(path, *args, **kwargs)
            after = time.monotonic_ns()
            context['original_primary_read'] = {'before_ns': before, 'after_ns': after, 'value': int(result.strip())}
            return result
        return self.original_read_text(path, *args, **kwargs)

    def _snapshot(self, domain):
        # Do not expand to database/driver/all domains. This is one service question.
        if domain.label != 'service' and not domain.label.startswith('service-'):
            return self.original_snapshot(domain)
        key = str(domain.path)
        with self.lock:
            if self.count >= self.MAX_SNAPSHOTS or (key not in self.domains and len(self.domains) >= self.MAX_DOMAINS):
                self.overflow += 1
                return self.original_snapshot(domain)
            self.count += 1
            if key not in self.domains:
                self.domains[key] = {'path': key, 'inode': domain.path.stat().st_ino,
                                     'limits': getattr(domain, 'settings', {}), 'label': domain.label}
            tid = threading.get_native_id()
            if tid not in self.threads and len(self.threads) < self.MAX_THREADS:
                self.threads.add(tid)
                self._set_trace_filter()
        context = {'domain_path': domain.path, 'current_path': domain.path / 'memory.current',
                   'snapshot_enter_ns': time.monotonic_ns(), 'thread_id': tid}
        previous = getattr(self.local, 'context', None)
        self.local.context = context
        try:
            result = self.original_snapshot(domain)
            context['snapshot_end_ns'] = time.monotonic_ns()
            context['current_after_snapshot'] = self._current(context)
            primary = context.get('original_primary_read', {}).get('value')
            assert primary == result['memory_current'], 'original primary field changed'
            stats = result.get('memory_stat', {})
            selected = sum(stats.get(k, 0) for k in ('anon', 'file', 'kernel', 'sock'))
            current_values = [v['value'] for k, v in context.items() if isinstance(v, dict) and 'value' in v]
            context.update(selected_stat_fields=['anon', 'file', 'kernel', 'sock'],
                selected_stat_sum=selected,
                primary_minus_selected_stat_sum=primary-selected if isinstance(primary,int) else None,
                current_bracket_min=min(current_values) if current_values else None,
                current_bracket_max=max(current_values) if current_values else None,
                diagnostic_only=True, normal_score_eligible=False,
                interpretation='Selected-stat remainder is not an identified allocation; primary metric retained unchanged.')
            context.pop('domain_path');context.pop('current_path')
            result['memory_observation'] = context
            return result
        finally:
            self.local.context = previous

    def __enter__(self):
        self.output.mkdir(parents=True, exist_ok=True)
        self.original_snapshot = self.module.Domain.snapshot
        self.original_read_text = Path.read_text
        self._start_trace()
        owner = self
        Path.read_text = lambda path, *args, **kwargs: owner._read_text(path, *args, **kwargs)
        self.module.Domain.snapshot = lambda domain: owner._snapshot(domain)
        identity = {'kernel_release': platform.release(), 'uname': list(os.uname()),
                    'page_bytes': os.sysconf('SC_PAGESIZE'), 'observer_pid': os.getpid(),
                    'probe_sha256': hashlib.sha256(Path(__file__).read_bytes()).hexdigest(),
                    'diagnostic_only': True, 'normal_score_eligible': False,
                    'maximum_snapshots': self.MAX_SNAPSHOTS, 'trace': self.trace_info}
        for name in ('online', 'possible', 'present'):
            path = Path('/sys/devices/system/cpu') / name
            try:identity['CPU_' + name] = self.original_read_text(path).strip()
            except OSError as error:identity['CPU_' + name] = {'error': repr(error)}
        config = Path('/boot') / ('config-' + platform.release())
        try:
            text = self.original_read_text(config)
            identity['kernel_config_sha256'] = hashlib.sha256(text.encode()).hexdigest()
            identity['kernel_config_relevant'] = [line for line in text.splitlines() if line.startswith(('CONFIG_MEMCG', 'CONFIG_SMP=', 'CONFIG_NR_CPUS=', 'CONFIG_HZ=', 'CONFIG_FTRACE=', 'CONFIG_TRACEPOINTS='))]
        except OSError as error:identity['kernel_config_missing'] = repr(error)
        _save(self.output / 'memory-observer-identity.json', identity)
        return self

    def __exit__(self, *unused):
        self.module.Domain.snapshot = self.original_snapshot
        Path.read_text = self.original_read_text
        self._stop_trace()
        _save(self.output / 'memory-observer-finish.json', {'snapshots': self.count, 'overflow': self.overflow,
            'domains': list(self.domains.values()), 'threads': sorted(self.threads), 'trace': self.trace_info,
            'hooks_restored': True, 'metric_changed': False, 'diagnostic_only': True})


def main(argv=None):
    import argparse
    import copy
    import signal
    import sys
    import focus_cycle as fc
    import hosted_cycle as h
    import linux_harness as lh
    import reuse_build
    import typed_pg_self as pg

    parser=argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--root',type=Path,required=True)
    parser.add_argument('--normal-manifest',type=Path,required=True)
    parser.add_argument('--typed-manifest',type=Path,required=True)
    parser.add_argument('--tools',type=Path,required=True)
    parser.add_argument('--no-trace',action='store_true')
    args=parser.parse_args(argv)
    args.root=args.root.resolve();args.tools=args.tools.resolve()
    args.normal_manifest=args.normal_manifest.resolve(strict=True)
    args.typed_manifest=args.typed_manifest.resolve(strict=True)
    if args.root.exists():parser.error('fresh diagnostic root required; never replace a prior window')
    if any(args.root==p.parent or p.parent in args.root.parents or args.root in p.parents
           for p in (args.normal_manifest,args.typed_manifest)):
        parser.error('output must be independent of frozen inputs')
    plan,bindings,manifest=pg.identify(args.normal_manifest,args.typed_manifest,current_product=h.PG_CURRENT_BASELINE)
    for name,expected in (('B',reuse_build.CURRENT_PG_FILES),('BP',reuse_build.CURRENT_TYPED_PG_FILES)):
        if {key:value['sha256'] for key,value in bindings[name]['files'].items()}!=expected:
            parser.error('exact frozen normal d05 Rust1.99.1 input required: '+name)
    plan.update(schema='ku-normal-memory-read-bracket-v1',rate=200,warmup_seconds=3,score_seconds=15,
        order=[['B','memory-observation']],maximum_windows=1,maximum_replacements=0,
        diagnostic_only=True,normal_score_eligible=False,formal_qualified=False,event=None,
        purpose='One original normal d05 ordinary PG window. Preserve primary and all service semantics; '
                'add read brackets and optional private flush trace only. No perf, score, adoption or retry.')
    args.root.mkdir(parents=True)
    h.save(args.root/'frozen-memory-observer-plan.json',dict(plan,bindings=bindings,entry_sha256=h.sha(__file__)))
    fixture_root=args.root/'fixture';fixture_root.mkdir()
    fixture_manifest=copy.deepcopy(manifest);fixture_manifest['stacks']={}
    h.save(fixture_root/'manifest.json',fixture_manifest)
    fixture=h.db.Database(fixture_root,manifest['uid'],manifest['gid'])
    original_harness=fc.Harness;private=None
    class MemoryHarness(original_harness):
        def __init__(self,*a,**kw):
            super().__init__(*a,**kw);self.manifest=copy.deepcopy(self.manifest)
            self.manifest['db_url']=private['db_url'];self.db_domain=h.ExternalDomain(private['db_cgroup'])
    result=dict(schema=plan['schema'],execution_complete=False,diagnostic_only=True,
        normal_score_eligible=False,candidate_accepted=False,maximum_windows=1,maximum_replacements=0)
    old_alarm=signal.getsignal(signal.SIGALRM)
    def expired(*unused):raise TimeoutError('fixed memory-observer operation bound120seconds')
    try:
        signal.signal(signal.SIGALRM,expired);signal.alarm(120)
        fixture.start();private=h.load(fixture.manifest_path);fc.Harness=MemoryHarness
        with MemoryBracketProbe(lh,args.root/'observer',trace=not args.no_trace):
            row=fc.window(plan,'B','db',200,3,15,args.root/'normal-memory-window',
                binding=bindings['B'],job={'phase':'diagnostic','contract':'pg-strict'})
        finish=h.load(args.root/'observer/memory-observer-finish.json')
        result['observer_finish']=finish
        if not finish['snapshots'] or finish['overflow']:
            raise ValueError('service snapshot observer missing or overflowed')
        result['window']=row;valid,reasons=h.normal_diagnostic_quality(row)
        result.update(window_qualified=valid,quality_reasons=reasons)
        if fc.version_identity(plan,'B')!=bindings['B']:raise ValueError('normal input identity changed')
    except BaseException as error:
        result['observer_error']=repr(error)
        if not isinstance(error,Exception):raise
    finally:
        fc.Harness=original_harness
        try:cleanup=fixture.stop()
        except Exception as error:cleanup={'error':repr(error)}
        result['database_cleanup']=cleanup
        dbok=cleanup.get('forced') is False and cleanup.get('cgroup_removed') is True and cleanup.get('after',{}).get('process_count')==0
        result['execution_complete']=result.get('window_qualified') is True and dbok and 'observer_error' not in result
        result['boundary']=('One instrumentation-only memory observation. Original metric/threshold unchanged; '
            'the added reads and trace make CPU/memory values ineligible for candidate scoring.')
        h.save(args.root/'summary.json',result)
        signal.alarm(0);signal.signal(signal.SIGALRM,old_alarm)
    return 0 if result['execution_complete'] else 2


if __name__=='__main__':
    raise SystemExit(main())
