"""One original d05 PG window with BTF-verified, owned dynamic stock probes.

Reuses memory_bracket_probe.main and its fixed cached inputs/Database/window.
No BPF loader, kernel-memory reader, vmlinux download, stock flush, pressure,
reclaim, freeze, product timeout/limit change, candidate score or replacement.
"""
import argparse
import decimal
import errno
import hashlib
import json
import os
from pathlib import Path
import platform
import re
import shutil
import subprocess
import threading
import time
import traceback


def save(path,value):
    path.parent.mkdir(parents=True,exist_ok=True)
    path.write_text(json.dumps(value,indent=2,sort_keys=True)+'\n')


def trace_readback(data,names):
    """Bounded offline association; an unpaired return has unknown target."""
    records=[];stacks={};pairs=[];unknown_returns=0;errors=[]
    numbers=lambda value:int(value,16) if value.lower().startswith('0x') else int(value)
    for line in data.decode(errors='replace').splitlines():
        match=re.search(r'-(\d+)\s+\[(\d+)\].*?\s(\d+\.\d+):\s+(\S+):',line)
        if not match:continue
        key=next((key for key,name in names.items() if match[4]==name),None)
        if key is None:continue
        fields={name:numbers(value) for name,value in re.findall(r'\b([a-z][a-z0-9]*)=(-?(?:0x[0-9a-fA-F]+|\d+))\b',line)}
        row={'kind':key,'tid':int(match[1]),'CPU':int(match[2]),
             'ns':int(decimal.Decimal(match[3])*10**9),'fields':fields}
        records.append(row)
        family={'ce':'current','cr':'current','de':'drain','dr':'drain','fe':'refill','fr':'refill'}[key]
        identity=(row['tid'],family,fields.get('stock') if family=='drain' else fields.get('css') if family=='current' else fields.get('memcg'),
                  fields.get('index') if family=='drain' else None)
        if key in ('ce','de','fe'):stacks.setdefault(identity,[]).append(row)
        elif stacks.get(identity):
            entry=stacks[identity].pop();pair={'entry':entry,'return':row,'target_attribution':'target-filtered entry'}
            if family=='drain':
                index=entry['fields'].get('index');selected='n'+str(index)
                pair.update(pages_before=entry['fields'].get(selected),pages_after=row['fields'].get(selected),
                            cached_before=entry['fields'].get('c'+str(index)),same_CPU=entry['CPU']==row['CPU'])
                if pair['pages_before'] is None or pair['pages_after'] is None or not pair['same_CPU']:
                    errors.append('drain field/CPU pair incomplete')
            pairs.append(pair)
        elif family=='drain':unknown_returns+=1
        else:errors.append('unmatched target '+family+' return')
    pending=[entry for entries in stacks.values() for entry in entries]
    return {'records':records,'pairs':pairs,'pending_target_entries':pending,'unknown_target_drain_returns':unknown_returns,
            'association_errors':errors,'target_pairs_complete':not pending and not errors,
            'boundary':'Only entry-filtered pairs are target attributed. Unmatched compact drain returns have unknown target. '
                       'No full perCPU target-stock total follows: untouched slots and inlined consumes are not read.'}


class DirectStock:
    SYMBOLS=('memory_current_read','drain_stock')
    MAX_LINES=2048
    MAX_BYTES=2<<20
    MAX_MARKERS=128

    def __init__(self,lh,output):
        self.lh,self.output=lh,Path(output)
        self.base=self.instance=None
        self.definitions=[];self.paths={};self.markers=[];self.target=None
        self.names={key:'s'+str(os.getpid())+'_'+key for key in ('ce','cr','de','dr')}
        self.group='ku_stock_'+str(os.getpid())
        self.info={'diagnostic_only':True,'normal_score_eligible':False,'available':False,
            'return_boundary':'drain_return is compact all-cgroup metadata: stock/index and7post-page counts only. '
                'Only pairs linked to target-filtered drain_entry are target-attributed; unmatched returns stay unknown target.',
            'refill_boundary':'refill_stock is deliberately not probed in this four-probe observer. '
                'Its absence cannot exclude refill work; the original six-probe records remain separate.'}

    def mark(self,event,**values):
        record={'event':event,'monotonic_ns':time.monotonic_ns(),'tid':threading.get_native_id(),**values}
        if len(self.markers)>=self.MAX_MARKERS:
            self.info['marker_overflow']=True;return
        self.markers.append(record)
        if self.instance is not None:
            try:(self.instance/'trace_marker').write_text('KUSTOCK '+json.dumps(record,sort_keys=True)+'\n')
            except OSError as error:self.info['marker_error']=repr(error)

    def _btf_layout(self):
        pahole=shutil.which('pahole')
        if not pahole:raise RuntimeError('pahole not installed; caller may install this small standard tool once')
        btf=Path('/sys/kernel/btf/vmlinux')
        if not btf.is_file():raise RuntimeError('running kernel BTF unavailable; no guessed layout or vmlinux download')
        outputs={}
        for name in ('memcg_stock_pcp','mem_cgroup'):
            command=[pahole,'-F','btf','-C',name,str(btf)]
            result=subprocess.run(command,capture_output=True,text=True,timeout=20)
            if len(result.stdout.encode())>128<<10 or len(result.stderr.encode())>16<<10:
                raise RuntimeError('pahole output exceeds fixed bounds')
            (self.output/(name+'.pahole.txt')).write_text(result.stdout+result.stderr)
            if result.returncode:raise RuntimeError('pahole cannot verify '+name)
            outputs[name]=result.stdout
        def member(text,name,array=None):
            suffix=(r'\s*\[\s*'+str(array)+r'\s*\]') if array is not None else ''
            pattern=r'(?m)^([^\n]*\b'+name+suffix+r'\s*;)\s*/\*\s*(\d+)\s+(\d+)\s*\*/'
            matches=list(re.finditer(pattern,text))
            if len(matches)!=1:raise RuntimeError('unambiguous BTF member offset unavailable: '+name)
            m=matches[0];return {'offset':int(m[2]),'size':int(m[3]),'declaration':m[1].strip()}
        def size(text):
            matches=re.findall(r'/\*\s*size:\s*(\d+),',text)
            if len(matches)!=1:raise RuntimeError('unambiguous BTF struct size unavailable')
            return int(matches[0])
        nr=member(outputs['memcg_stock_pcp'],'nr_pages',7)
        cached=member(outputs['memcg_stock_pcp'],'cached',7)
        css=member(outputs['mem_cgroup'],'css')
        stock_size=size(outputs['memcg_stock_pcp']);memcg_size=size(outputs['mem_cgroup'])
        if nr['size']!=7 or cached['size']!=56 or '*' not in cached['declaration']:
            raise RuntimeError('actual BTF is not seven byte-counts plus seven64bit memcg pointers')
        if 'struct cgroup_subsys_state' not in css['declaration']:
            raise RuntimeError('BTF css field has unexpected type')
        if nr['offset']+nr['size']>stock_size or cached['offset']+cached['size']>stock_size or css['offset']+css['size']>memcg_size:
            raise RuntimeError('BTF offsets outside actual struct bounds')
        digest=hashlib.sha256();total=0
        with btf.open('rb') as stream:
            for block in iter(lambda:stream.read(65536),b''):
                total+=len(block)
                if total>64<<20:raise RuntimeError('running BTF exceeds fixed64MiB hashing bound')
                digest.update(block)
        version=subprocess.run([pahole,'--version'],capture_output=True,text=True,timeout=5)
        self.info['BTF']={'pahole':pahole,'version':version.stdout.strip(),'pahole_sha256':hashlib.sha256(Path(pahole).read_bytes()).hexdigest(),
            'BTF_sha256':digest.hexdigest(),'BTF_bytes':total,'nr_pages':nr,'cached':cached,'css':css,
            'stock_size':stock_size,'memcg_size':memcg_size,'pointer_bytes':8,'slots':7}
        return nr,cached,css

    def _append_definition(self,text):
        # Never truncate kprobe_events or remove an event not created here.
        payload=(text+'\n').encode('ascii')
        if '\n' in text or '\r' in text or len(payload)>=4096:
            raise ValueError('one definition line strictly below4096bytes required')
        def errors():
            result={}
            for root in (self.base,self.instance):
                if root is None:continue
                path=root/'error_log'
                try:
                    with path.open('r') as stream:raw=stream.read(16385)
                    result[str(path)]={'text':raw[:16384],'truncated':len(raw)>16384}
                except OSError as error:result[str(path)]={'error':repr(error)}
            return result
        interface=getattr(self,'interface',self.base/'kprobe_events')
        attempt={'definition':text,'interface':str(interface),
                 'before_ns':time.monotonic_ns(),'error_log_before':errors()}
        self.info['last_definition_attempt']=attempt
        try:
            descriptor=None
            try:
                # FileIO append performs SEEK_END at open: tracefs can reject
                # that with EINVAL before any definition reaches the kernel.
                attempt.update(stage='os.open',bytes_expected=len(payload))
                descriptor=os.open(interface,os.O_WRONLY|os.O_APPEND|getattr(os,'O_CLOEXEC',0))
                attempt['stage']='os.write'
                written=os.write(descriptor,payload);attempt['bytes_written']=written
                if written!=len(payload):
                    attempt['short_write']=True
                    raise OSError(errno.EIO,'short tracefs definition write; stop without retrying the remainder')
            finally:
                if descriptor is not None:
                    attempt['close_attempted']=True
                    os.close(descriptor);attempt['descriptor_closed']=True
        except OSError as error:
            attempt.update(after_ns=time.monotonic_ns(),errno=error.errno,error=repr(error),
                           traceback=traceback.format_exc()[-16384:],error_log_after=errors())
            failures=self.info.setdefault('definition_failures',[])
            if len(failures)<16:failures.append(attempt)
            else:self.info['definition_failure_overflow']=True
            save(self.output/'probe-definition-failure.json',{'failures':failures,
                'boundary':'error_log is read-only and may include other writers. Match the exact failed command/caret; '
                           'do not attribute older/unrelated lines or EINVAL alone to a cause.'})
            (self.output/'probe-definition-failure.txt').write_text(json.dumps(attempt,indent=2)+'\n')
            raise

    def _profile(self):
        if self.info.get('backend')=='fprobe':return {}
        result={}
        for line in (self.base/'kprobe_profile').read_text().splitlines():
            fields=line.split()
            if len(fields)<3:continue
            for key,name in self.names.items():
                if name in fields[0]:result[key]={'hits':int(fields[-2]),'missed':int(fields[-1]),'raw':line}
        return result

    def _clear_definitions(self):
        for directory in self.paths.values():(directory/'enable').write_text('0')
        for name in reversed(self.definitions):self._append_definition('-:'+self.group+'/'+name)
        self.definitions.clear();self.paths.clear()

    def _prepare(self):
        if platform.machine()!='aarch64':raise RuntimeError('this fixed observation is for the actual ARM64 cache')
        if (os.cpu_count() or 0)>32:raise RuntimeError('refuse oversized perCPU trace buffers')
        nr,cached,css=self._btf_layout();self.css_offset=css['offset']
        symbols={}
        with Path('/proc/kallsyms').open() as stream:
            for line in stream:
                fields=line.split()
                if len(fields)>=3 and fields[2] in self.SYMBOLS:symbols[fields[2]]=fields[0]
        if any(not int(symbols.get(name,'0'),16) for name in self.SYMBOLS):
            raise RuntimeError('exact callable symbols/ABI unavailable; do not guess optimizer-clone arguments')
        self.info['symbols']=symbols
        self.base=next((p for p in (Path('/sys/kernel/tracing'),Path('/sys/kernel/debug/tracing'))
            if (p/'kprobe_events').is_file()),None)
        if self.base is None:raise RuntimeError('kprobe tracefs interface unavailable; no mount attempted')
        self.instance=self.base/'instances'/('ku-direct-stock-'+str(os.getpid()));self.instance.mkdir()
        (self.instance/'tracing_on').write_text('0');(self.instance/'buffer_size_kb').write_text('64')
        (self.instance/'trace_clock').write_text('mono')
        pages=' '.join('n'+str(i)+'=+'+str(nr['offset']+i)+'($arg1):u8' for i in range(7))
        pointers=' '.join('c'+str(i)+'=+'+str(cached['offset']+8*i)+'($arg1):x64' for i in range(7))
        specifications={
            'ce':('p','memory_current_read','css=$arg1:x64'),
            'cr':('r64','memory_current_read','css=$arg1:x64 value=$retval:u64'),
            'de':('p','drain_stock','stock=$arg1:x64 index=$arg2:s32 '+pages+' '+pointers),
            'dr':('r64','drain_stock','stock=$arg1:x64 index=$arg2:s32 '+pages)}
        def define(backend):
            self.info.update(backend=backend,probe_definitions={},formats={})
            self.interface=self.base/('dynamic_events' if backend=='fprobe' else 'kprobe_events')
            for key,(kind,function,fields) in specifications.items():
                if backend=='fprobe':
                    returning=kind.startswith('r');kind='f64' if returning else 'f'
                    if returning:function+='%return'
                definition=kind+':'+self.group+'/'+self.names[key]+' '+function+' '+fields
                directory=self.instance/'events'/self.group/self.names[key]
                if directory.exists():raise RuntimeError('probe name already exists; no preexisting definition may be changed')
                try:self._append_definition(definition)
                except BaseException:
                    # A short/error return must not leak a definition which
                    # the kernel nevertheless created. Never retry its tail.
                    if directory.is_dir():
                        self.definitions.append(self.names[key]);self.paths[key]=directory
                    raise
                self.definitions.append(self.names[key])
                fmt=(directory/'format').read_text()
                required=('css','value') if key=='cr' else ('css',) if key=='ce' else ('stock','index','n0','n6','c0','c6') if key=='de' else ('stock','index','n0','n6') if key=='dr' else ('memcg','pages')
                if not all(re.search(r'\b'+name+r'\b',fmt) for name in required):raise RuntimeError('probe fields missing: '+key)
                self.paths[key]=directory;self.info['probe_definitions'][key]=definition;self.info['formats'][key]=fmt
        try:define('kprobe')
        except OSError as error:
            if self.info.get('last_definition_attempt',{}).get('short_write'):
                raise RuntimeError('partial definition write; stop and clean owned events without backend retry') from error
            self.info['kprobe_create_failed']=repr(error)
            self._clear_definitions()
            if not (self.base/'dynamic_events').is_file():raise RuntimeError('official fprobe dynamic_events unavailable') from error
            define('fprobe')
            self.info.update(miss_counts_unknown=True,
                miss_count_boundary='No verified per-owned-fprobe miss-count interface. '
                'Captured target entry/return pairs can be positive evidence; missing events cannot exclude a path, '
                'establish full charge totals or establish observation_complete.')
        self.info['profile_before']=self._profile()
        if self.info['backend']=='kprobe' and len(self.info['profile_before'])!=len(self.names):
            raise RuntimeError('all owned kprobe profile counters required')
        self.info.update(available=True,private_instance=str(self.instance),group=self.group,
                         kernel=platform.release(),uname=list(os.uname()),script_sha256=hashlib.sha256(Path(__file__).read_bytes()).hexdigest())
        (self.instance/'tracing_on').write_text('1')

    def _trace_bytes(self):
        with (self.instance/'trace').open('rb') as stream:data=stream.read(self.MAX_BYTES+1)
        if len(data)>self.MAX_BYTES:raise RuntimeError('bounded direct trace exceeded2MiB')
        return data

    def _map_target(self,domain):
        if self.target is not None:raise RuntimeError('one owned normal service only; no second target')
        tid=threading.get_native_id()
        for key in ('ce','cr'):
            (self.paths[key]/'filter').write_text('common_pid == '+str(tid))
            (self.paths[key]/'enable').write_text('1')
        mappings=[]
        for _ in range(2):
            before=time.monotonic_ns();value=int(self.original_read_text(domain.path/'memory.current').strip());after=time.monotonic_ns()
            lines=self._trace_bytes().decode().splitlines();entries=[];returns=[]
            for line in lines:
                m=re.search(r'-(\d+)\s+\[\d+\].*?\s(\d+\.\d+):',line)
                if not m or int(m[1])!=tid:continue
                ns=int(decimal.Decimal(m[2])*10**9)
                if not before-1000<=ns<=after+1000:continue
                css=re.search(r'\bcss=(0x[0-9a-fA-F]+)',line)
                if not css:continue
                if self.names['ce']+':' in line:entries.append(int(css[1],16))
                if self.names['cr']+':' in line:
                    val=re.search(r'\bvalue=(\d+)',line)
                    if val:returns.append((int(css[1],16),int(val[1])))
            if not entries or not returns or len(set(entries+[r[0] for r in returns]))!=1 or value not in [r[1] for r in returns]:
                raise RuntimeError('CSS entry/return/file-read mapping incomplete; no service requests started')
            mappings.append({'before_ns':before,'after_ns':after,'value':value,'css':entries[0],'entries':entries,'returns':returns})
        if mappings[0]['css']!=mappings[1]['css'] or mappings[0]['css']<=self.css_offset:raise RuntimeError('target CSS mapping unstable')
        css=mappings[0]['css'];memcg=css-self.css_offset
        self.target={'path':str(domain.path),'inode':domain.path.stat().st_ino,'css':hex(css),'memcg':hex(memcg),'mappings':mappings}
        for key in ('ce','cr'):(self.paths[key]/'filter').write_text('css == '+hex(css))
        predicate=' || '.join('(index == '+str(i)+' && c'+str(i)+' == '+hex(memcg)+')' for i in range(7))
        (self.paths['de']/'filter').write_text(predicate)
        # The cached target has been cleared by return. Do not mislabel this as target-only.
        (self.paths['dr']/'filter').write_text('index >= 0 && index < 7')
        for key in ('de','dr'):(self.paths[key]/'enable').write_text('1')
        self.info['target']=self.target;self.mark('target-mapped',**self.target)
        (self.output/'target-mapping-trace.txt').write_bytes(self._trace_bytes())
        save(self.output/'target-mapping.json',self.target)

    def __enter__(self):
        self.output.mkdir(parents=True,exist_ok=True)
        self.original_read_text=Path.read_text
        try:self._prepare()
        except BaseException as error:
            self.info.update(prepare_error=repr(error),prepare_traceback=traceback.format_exc()[-16384:])
            self._cleanup();save(self.output/'direct-stock-identity.json',self.info);raise
        self.old_init=self.lh.Domain.__init__;self.old_remove=self.lh.Domain.remove;owner=self
        def init(domain,*args,**kwargs):
            result=owner.old_init(domain,*args,**kwargs)
            owner.mark('domain-created',label=domain.label,path=str(domain.path),inode=domain.path.stat().st_ino)
            if domain.label=='service' or domain.label.startswith('service-'):
                try:owner._map_target(domain)
                except BaseException:
                    # No process has been spawned yet. Remove only this owned empty domain.
                    try:domain.path.rmdir()
                    except OSError as error:owner.info['mapping_domain_cleanup_error']=repr(error)
                    raise
            return result
        def remove(domain):
            owner.mark('domain-remove-begin',label=domain.label,path=str(domain.path))
            result=owner.old_remove(domain)
            owner.mark('domain-remove-end',label=domain.label,path=str(domain.path),removed=result)
            return result
        self.lh.Domain.__init__=init;self.lh.Domain.remove=remove
        try:save(self.output/'direct-stock-identity.json',self.info)
        except BaseException:
            self.lh.Domain.__init__=self.old_init;self.lh.Domain.remove=self.old_remove
            self._cleanup();raise
        return self

    def _cleanup(self):
        if self.instance is not None:
            try:
                (self.instance/'tracing_on').write_text('0')
                for directory in self.paths.values():(directory/'enable').write_text('0')
                data=self._trace_bytes();lines=data.splitlines(keepends=True)
                self.info['line_limit_exceeded']=len(lines)>self.MAX_LINES
                (self.output/'direct-stock-trace.txt').write_bytes(b''.join(lines[:self.MAX_LINES]))
                readback=trace_readback(b''.join(lines[:self.MAX_LINES]),self.names)
                save(self.output/'direct-stock-association.json',readback)
                self.info['association']={key:value for key,value in readback.items() if key not in ('records','pairs')}
                self.info['pair_counts']={family:sum(p['entry']['kind']==family for p in readback['pairs']) for family in ('ce','de')}
                self.info['per_CPU_stats']={p.parent.name:self.original_read_text(p)[:4096]
                    for p in sorted((self.instance/'per_cpu').glob('cpu*/stats'))[:32]}
                self.info['profile_after']=self._profile()
            except (OSError,RuntimeError,ValueError) as error:self.info['capture_error']=repr(error)
            finally:
                try:self.instance.rmdir();self.info['private_instance_removed']=True
                except OSError as error:self.info.update(private_instance_removed=False,instance_remove_error=repr(error))
                self.instance=None
        errors=[]
        for name in reversed(self.definitions):
            try:self._append_definition('-:'+self.group+'/'+name)
            except OSError as error:errors.append({'event':name,'error':repr(error)})
        self.info['owned_definition_remove_errors']=errors

    def __exit__(self,*exc):
        self.lh.Domain.__init__=self.old_init;self.lh.Domain.remove=self.old_remove
        try:
            self.mark('natural-tail-begin',seconds=2);time.sleep(2);self.mark('natural-tail-end')
        finally:
            self._cleanup()
            failures=[]
            for key in ('capture_error','marker_overflow','marker_error','line_limit_exceeded','instance_remove_error'):
                if self.info.get(key):failures.append(key)
            if self.info.get('owned_definition_remove_errors'):failures.append('owned_definition_remove_errors')
            if not self.target:failures.append('target_not_mapped')
            after=self.info.get('profile_after',{})
            if self.info.get('backend')=='fprobe':failures.append('fprobe_miss_counts_unknown')
            elif set(after)!=set(self.names) or any(value['missed'] for value in after.values()):failures.append('kprobe_profile_missing_or_missed')
            stats=self.info.get('per_CPU_stats',{})
            if not stats:failures.append('per_CPU_stats_missing')
            for text in stats.values():
                for field in ('overrun','commit overrun','dropped events'):
                    matches=re.findall(r'(?m)^\s*'+re.escape(field)+r':\s*(\d+)\s*$',text)
                    if len(matches)!=1 or int(matches[0]):failures.append('trace_'+field.replace(' ','_'))
            if not self.info.get('association',{}).get('target_pairs_complete'):failures.append('target_entry_return_incomplete')
            self.info['observation_complete']=not failures;self.info['observation_failures']=failures
            self.info['positive_pair_only']=failures==['fprobe_miss_counts_unknown']
            save(self.output/'direct-stock-finish.json',{'info':self.info,'markers':self.markers,'hooks_restored':True,
                'metric_changed':False,'no_pressure_or_flush':True,'target_mapped':self.target is not None})
        if failures and not self.info['positive_pair_only'] and not any(exc):
            raise RuntimeError('direct stock observation incomplete: '+','.join(failures))


def main(argv=None):
    import memory_bracket_probe as mb
    original=mb.MemoryBracketProbe
    class DirectMemory(original):
        def __enter__(self):
            self.direct=DirectStock(self.module,self.output/'direct-stock');self.direct.__enter__()
            try:return super().__enter__()
            except BaseException as error:
                self.direct.__exit__(type(error),error,error.__traceback__);raise
        def __exit__(self,*exc):
            try:return super().__exit__(*exc)
            finally:self.direct.__exit__(*exc)
    mb.MemoryBracketProbe=DirectMemory
    try:
        code=mb.main(argv)
        parser=argparse.ArgumentParser(add_help=False);parser.add_argument('--root',type=Path,required=True)
        root=parser.parse_known_args(argv)[0].root
        finish=root/'observer/direct-stock/direct-stock-finish.json'
        if finish.is_file():
            info=json.loads(finish.read_text())['info'];summary=root/'summary.json'
            if summary.is_file() and not info.get('observation_complete'):
                result=json.loads(summary.read_text())
                result.update(execution_complete=False,observation_complete=False,
                    positive_pair_only=info.get('positive_pair_only',False),direct_stock_observation=info)
                save(summary,result);return 2
        return code
    finally:mb.MemoryBracketProbe=original


if __name__=='__main__':
    raise SystemExit(main())
