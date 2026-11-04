"""Two adjacent original d05 windows; passive owned-lifecycle/drain observation.

No candidate score, forced stock flush, reclaim, freeze, added cleanup wait or
resource-limit change. First scripts/evidence remain immutable. Raw trace and
perCPU overrun/drop counters are required before claiming a negative finding.
"""
import argparse
import copy
import hashlib
import json
import os
from pathlib import Path
import platform
import signal
import threading
import time


def save(path,value):
    path.parent.mkdir(parents=True,exist_ok=True)
    path.write_text(json.dumps(value,indent=2,sort_keys=True)+'\n')


class OwnedDrainTrace:
    FUNCTIONS=('drain_local_memcg_stock','drain_local_obj_stock')
    ENTRY_FUNCTIONS=('mem_cgroup_css_offline','drain_all_stock',*FUNCTIONS)
    EVENTS=('workqueue_queue_work','workqueue_execute_start','workqueue_execute_end')
    MAX_LINES=2048
    MAX_BYTES=2<<20
    MAX_MARKERS=256

    def __init__(self,lh,output):
        self.lh,self.output=lh,Path(output)
        self.instance=None;self.enabled=[];self.markers=[];self.roots=set();self.lock=threading.Lock()
        self.info={'diagnostic_only':True,'normal_score_eligible':False,'available':False,'functions':{}}

    def mark(self,event,**values):
        record={'event':event,'monotonic_ns':time.monotonic_ns(),'tid':threading.get_native_id(),**values}
        with self.lock:
            if len(self.markers)>=self.MAX_MARKERS:
                self.info['marker_overflow']=True;return
            self.markers.append(record)
        if self.instance is not None:
            try:(self.instance/'trace_marker').write_text('KUMEM '+json.dumps(record,sort_keys=True)+'\n')
            except OSError as error:self.info['marker_error']=repr(error)

    def _prepare(self):
        if (os.cpu_count() or 0)>32:raise RuntimeError('refuse trace buffers on more than32 onlineCPUs')
        with Path('/proc/kallsyms').open() as stream:
            for line in stream:
                fields=line.split()
                if len(fields)<3:continue
                name=fields[2]
                if any(name==wanted or name.startswith(wanted+'.') for wanted in self.FUNCTIONS):
                    address=int(fields[0],16)
                    self.info['functions'][name]=hex(address)
        symbols=self.info['functions']
        for wanted in self.FUNCTIONS:
            if not any((name==wanted or name.startswith(wanted+'.')) and int(address,16)
                       for name,address in symbols.items()):
                raise RuntimeError('required permitted nonzero kallsyms symbol missing: '+wanted)
        addresses=sorted({v for v in symbols.values() if int(v,16)})
        expression=' || '.join('function == '+address for address in addresses)
        base=next((p for p in (Path('/sys/kernel/tracing'),Path('/sys/kernel/debug/tracing'))
                   if (p/'events/workqueue/workqueue_queue_work/format').is_file()),None)
        if base is None:raise RuntimeError('tracefs workqueue events unavailable; no mount attempted')
        instance=base/'instances'/('ku-owned-drain-'+str(os.getpid()));instance.mkdir();self.instance=instance
        (instance/'tracing_on').write_text('0');(instance/'buffer_size_kb').write_text('64')
        (instance/'trace_clock').write_text('mono')
        available=set()
        with (base/'available_filter_functions').open() as stream:
            for line in stream:
                name=line.split()[0] if line.split() else ''
                if any(name==wanted or name.startswith(wanted+'.') for wanted in self.ENTRY_FUNCTIONS):
                    available.add(name)
        for wanted in self.ENTRY_FUNCTIONS:
            if not any(name==wanted or name.startswith(wanted+'.') for name in available):
                raise RuntimeError('required limited function-trace symbol unavailable: '+wanted)
        (instance/'set_ftrace_filter').write_text('\n'.join(sorted(available))+'\n')
        (instance/'current_tracer').write_text('function')
        self.info['limited_function_entries']={'symbols':sorted(available),
            'tracer':(instance/'current_tracer').read_text().strip(),
            'boundary':'Entry and caller/CPU only, no memcg argument or return interval. '
                'Inline copies can lack a local-helper entry; drain_all_stock entry remains observed. '
                'Do not attribute unrelated cgroup activity to the previous service or exclude every offline effect from absence of overlap.'}
        formats={}
        for event in self.EVENTS:
            directory=instance/'events/workqueue'/event;fmt=(directory/'format').read_text()
            if 'function' not in fmt or 'work' not in fmt:raise RuntimeError('unexpected workqueue format: '+event)
            formats['workqueue/'+event]=fmt
            (directory/'filter').write_text(expression);(directory/'enable').write_text('1');self.enabled.append(directory)
        # Conditional stack trigger: never trace stacks for unrelated queue_work.
        try:
            (instance/'events/workqueue/workqueue_queue_work/trigger').write_text('stacktrace if '+expression)
            self.info['conditional_queue_stacks']=True
        except OSError as error:self.info['conditional_queue_stacks']={'error':repr(error)}
        for event in ('cgroup_mkdir','cgroup_rmdir','cgroup_release'):
            directory=instance/'events/cgroup'/event
            if not (directory/'format').is_file():continue
            fmt=(directory/'format').read_text()
            if 'path' not in fmt:continue
            try:
                (directory/'filter').write_text('path ~ "*ku-hosted-*"')
                (directory/'enable').write_text('1');self.enabled.append(directory);formats['cgroup/'+event]=fmt
            except OSError as error:self.info['cgroup_'+event+'_error']=repr(error)
        self.info.update(available=True,private_instance=str(instance),clock=(instance/'trace_clock').read_text(),
            formats=formats,buffer_kib_per_CPU=64,maximum_trace_lines=self.MAX_LINES,filter=expression)
        (instance/'tracing_on').write_text('1')

    def _cleanup_trace(self):
        if self.instance is None:return
        instance=self.instance
        try:
            (instance/'tracing_on').write_text('0')
            for directory in self.enabled:(directory/'enable').write_text('0')
            with (instance/'trace').open('rb') as stream:data=stream.read(self.MAX_BYTES+1)
            lines=data[:self.MAX_BYTES].splitlines(keepends=True)
            self.info.update(byte_limit_exceeded=len(data)>self.MAX_BYTES,line_limit_exceeded=len(lines)>self.MAX_LINES,
                             lines_seen_in_bounded_read=len(lines))
            (self.output/'stock-workqueue-trace.txt').write_bytes(b''.join(lines[:self.MAX_LINES]))
            self.info['per_CPU_stats']={p.parent.name:p.read_text()[:4096]
                for p in sorted((instance/'per_cpu').glob('cpu*/stats'))[:32]}
        except OSError as error:self.info['stop_read_error']=repr(error)
        finally:
            try:(instance/'current_tracer').write_text('nop')
            except OSError as error:self.info['tracer_stop_error']=repr(error)
            try:instance.rmdir();self.info['private_instance_removed']=True
            except OSError as error:self.info.update(private_instance_removed=False,remove_error=repr(error))
            self.instance=None

    def __enter__(self):
        self.output.mkdir(parents=True,exist_ok=True)
        try:self._prepare()
        except BaseException as error:
            self.info['prepare_error']=repr(error);self._cleanup_trace();save(self.output/'drain-trace-identity.json',self.info)
            raise
        self.old_init=self.lh.Domain.__init__;self.old_remove=self.lh.Domain.remove
        self.old_start=self.lh.Harness.start;self.old_rmdir=Path.rmdir
        owner=self
        def init(domain,*a,**kw):
            result=owner.old_init(domain,*a,**kw)
            owner.roots.add(str(domain.harness.cgroup_root))
            owner.mark('domain-created',label=domain.label,path=str(domain.path),inode=domain.path.stat().st_ino,
                       settings=domain.settings)
            return result
        def remove(domain):
            owner.mark('domain-remove-begin',label=domain.label,path=str(domain.path))
            try:
                result=owner.old_remove(domain)
                owner.mark('domain-remove-end',label=domain.label,path=str(domain.path),removed=result)
                return result
            except BaseException as error:
                owner.mark('domain-remove-error',label=domain.label,path=str(domain.path),error=repr(error));raise
        def start(harness,*a,**kw):
            owner.roots.add(str(harness.cgroup_root))
            owner.mark('service-start-begin',harness_root=str(harness.cgroup_root))
            result=owner.old_start(harness,*a,**kw)
            owner.mark('service-ready',path=str(result.domain.path),inode=result.domain.path.stat().st_ino,
                       pid=result.process.pid)
            return result
        def rmdir(path):
            name=str(path)
            if not any(name==root or name.startswith(root+'/') for root in owner.roots):
                return owner.old_rmdir(path)
            try:inode=path.stat().st_ino
            except OSError:inode=None
            owner.mark('owned-rmdir-begin',path=name,inode=inode)
            try:
                result=owner.old_rmdir(path);owner.mark('owned-rmdir-end',path=name,inode=inode);return result
            except BaseException as error:
                owner.mark('owned-rmdir-error',path=name,inode=inode,error=repr(error));raise
        self.lh.Domain.__init__=init;self.lh.Domain.remove=remove;self.lh.Harness.start=start;Path.rmdir=rmdir
        try:
            self.mark('trace-ready',functions=self.info['functions'])
            save(self.output/'drain-trace-identity.json',self.info)
        except BaseException:
            self.lh.Domain.__init__=self.old_init;self.lh.Domain.remove=self.old_remove
            self.lh.Harness.start=self.old_start;Path.rmdir=self.old_rmdir;self._cleanup_trace()
            raise
        return self

    def __exit__(self,*unused):
        self.lh.Domain.__init__=self.old_init;self.lh.Domain.remove=self.old_remove
        self.lh.Harness.start=self.old_start;Path.rmdir=self.old_rmdir
        # An observation tail, not a cleanup/response timeout or a wait-for-drain fix.
        self.mark('natural-tail-begin',seconds=2);time.sleep(2);self.mark('natural-tail-end')
        self._cleanup_trace()
        save(self.output/'owned-lifecycle.json',{'markers':self.markers,'roots':sorted(self.roots),'info':self.info,
            'hooks_restored':True,'no_stock_flush':True,'metric_changed':False,'diagnostic_only':True})


def main(argv=None):
    import focus_cycle as fc
    import hosted_cycle as h
    import linux_harness as lh
    import reuse_build
    import typed_pg_self as pg
    from memory_bracket_probe import MemoryBracketProbe
    p=argparse.ArgumentParser(description=__doc__)
    p.add_argument('--root',type=Path,required=True);p.add_argument('--normal-manifest',type=Path,required=True)
    p.add_argument('--typed-manifest',type=Path,required=True);p.add_argument('--tools',type=Path,required=True)
    a=p.parse_args(argv);a.root=a.root.resolve();a.normal_manifest=a.normal_manifest.resolve(strict=True)
    a.typed_manifest=a.typed_manifest.resolve(strict=True);a.tools=a.tools.resolve()
    if a.root.exists():p.error('fresh output required; no repeat or overwriting')
    if any(a.root==m.parent or m.parent in a.root.parents or a.root in m.parents for m in (a.normal_manifest,a.typed_manifest)):
        p.error('output must be independent of frozen inputs')
    plan,bindings,manifest=pg.identify(a.normal_manifest,a.typed_manifest,current_product=h.PG_CURRENT_BASELINE)
    for n,expected in (('B',reuse_build.CURRENT_PG_FILES),('BP',reuse_build.CURRENT_TYPED_PG_FILES)):
        if {k:v['sha256'] for k,v in bindings[n]['files'].items()}!=expected:p.error('exact d05 normal cache required')
    plan.update(schema='ku-normal-stock-drain-workqueue-v1',rate=200,warmup_seconds=3,score_seconds=15,
        order=[['B','first'],['B','next']],maximum_windows=2,maximum_replacements=0,
        normal_score_eligible=False,formal_qualified=False,event=None,
        purpose='Only queue/execute and owned remove/new-start ordering; not candidate performance or stock-pressure test.')
    a.root.mkdir(parents=True);save(a.root/'frozen-drain-plan.json',dict(plan,bindings=bindings,entry_sha256=h.sha(__file__)))
    fixture_root=a.root/'fixture';fixture_root.mkdir();m=copy.deepcopy(manifest);m['stacks']={}
    h.save(fixture_root/'manifest.json',m);fixture=h.db.Database(fixture_root,m['uid'],m['gid'])
    original=fc.Harness;private=None;result={'schema':plan['schema'],'execution_complete':False,'rows':[],
        'diagnostic_only':True,'normal_score_eligible':False,'candidate_accepted':False,'stopped':[]}
    class DrainHarness(original):
        def __init__(self,*args,**kw):
            super().__init__(*args,**kw);self.manifest=copy.deepcopy(self.manifest)
            self.manifest['db_url']=private['db_url'];self.db_domain=h.ExternalDomain(private['db_cgroup'])
    old_alarm=signal.getsignal(signal.SIGALRM)
    def expired(*unused):raise TimeoutError('fixed180s drain-observer bound')
    try:
        signal.signal(signal.SIGALRM,expired);signal.alarm(180)
        # Refuse to run windows if the required passive workqueue observation is unavailable.
        with OwnedDrainTrace(lh,a.root/'drain-observer'):
            fixture.start();private=h.load(fixture.manifest_path);fc.Harness=DrainHarness
            with MemoryBracketProbe(lh,a.root/'observer'):
                for turn in ('first','next'):
                    row=fc.window(plan,'B','db',200,3,15,a.root/('window-'+turn),binding=bindings['B'],
                                  job={'phase':'diagnostic','contract':'pg-strict'})
                    row['turn']=turn;valid,reasons=h.normal_diagnostic_quality(row);result['rows'].append(row)
                    save(a.root/'results.json',result)
                    if not valid:
                        result['stopped'].append({'turn':turn,'reasons':reasons,'no_replacement':True});break
                    if fc.version_identity(plan,'B')!=bindings['B']:raise ValueError('frozen normal identity changed')
    except BaseException as error:
        result['observer_error']=repr(error)
        if not isinstance(error,Exception):raise
    finally:
        fc.Harness=original
        try:cleanup=fixture.stop()
        except Exception as error:cleanup={'error':repr(error)}
        result['database_cleanup']=cleanup
        dbok=cleanup.get('forced') is False and cleanup.get('cgroup_removed') is True and cleanup.get('after',{}).get('process_count')==0
        raw_complete=False
        lifecycle=a.root/'drain-observer/owned-lifecycle.json'
        if lifecycle.is_file():
            trace=h.load(lifecycle)['info'];loss={}
            for cpu,text in trace.get('per_CPU_stats',{}).items():
                values={line.split(':',1)[0]:line.split(':',1)[1].strip() for line in text.splitlines() if ':' in line}
                loss[cpu]={key:int(values[key]) for key in ('overrun','commit overrun','dropped events') if key in values}
            raw_complete=(trace.get('available') is True and bool(loss)
                and not any(trace.get(key) for key in ('byte_limit_exceeded','line_limit_exceeded','marker_overflow','marker_error','stop_read_error'))
                and all(not n for values in loss.values() for n in values.values())
                and trace.get('private_instance_removed') is True)
            result['trace_loss_counters']=loss
        result['observation_complete']=raw_complete
        result['execution_complete']=len(result['rows'])==2 and not result['stopped'] and dbok and 'observer_error' not in result and raw_complete
        result['scope']=('No scoring, bootstrap or adoption. Match queue/work/CPU to actual execute intervals and owned lifecycle markers, '
            'plus limited function entry/caller records for the synchronous branch. Entry records lack memcg arguments and returns; '
            'other cgroup activity must not be attributed to the old service. No observed queued-drain overlap in complete records '
            'rejects that observed path for these two windows, not every offline influence. Natural waiting is justified only by '
            'an actually linked old teardown-to-new startup overlap, never by a call count or an unrelated drain.')
        save(a.root/'summary.json',result);signal.alarm(0);signal.signal(signal.SIGALRM,old_alarm)
    return 0 if result['execution_complete'] else 2


if __name__=='__main__':
    raise SystemExit(main())
