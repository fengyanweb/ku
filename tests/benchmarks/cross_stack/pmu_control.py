"""Explicit diagnostic PMU counts; no product execution, retries or acceptance."""
import collections
import math
import os
from pathlib import Path
import re
import select
import signal
import subprocess
import struct
import threading
import time


EVENTS = ('cycles', 'instructions', 'task-clock')


def thread_ids(pid):
    values=sorted(int(p.name) for p in (Path('/proc')/str(pid)/'task').iterdir() if p.name.isdecimal())
    if not values or len(values)>4096:raise ValueError('bounded live target TID list required')
    return values


def cycle_identities(root=Path('/sys/bus/event_source/devices')):
    """Generic hardware cycles, plus the actual ARM64 PMU's sysfs alias."""
    rows=[dict(type=0,config=0,source='PERF_TYPE_HARDWARE/PERF_COUNT_HW_CPU_CYCLES')]
    for device in sorted(root.glob('armv8_pmuv3*'))[:16]:
        for name in ('cpu_cycles','cpu-cycles'):
            alias=device/'events'/name
            if not alias.is_file():continue
            text=alias.read_text()[:256].strip()
            match=re.fullmatch(r'event=(0x[0-9a-fA-F]+|[0-9]+)',text)
            if not match:continue
            kind=int((device/'type').read_text()[:32]);config=int(match[1],0)
            if kind>=4:rows.append(dict(type=kind,config=config,source=str(alias),alias=text))
    return rows


def cycles_attributes_valid(rows,identities):
    allowed={(r['type'],r['config']) for r in identities}
    events=[r for r in rows if (r['type'],r['config']) in allowed]
    return (len(events)==1 and events[0]['frequency_mode'] and events[0]['frequency']==999
        and events[0]['user_regs_mask']=='0x1ffffffff' and events[0]['stack_user']==16384
        and events[0]['sample_type']==78247)


def raw_cpu_threads(path,start_ns,end_ns,pid,identities):
    """Actual cycles SAMPLE CPU/TID and task sideband, within original client."""
    with Path(path).open('rb') as stream:raw=stream.read((96<<20)+1)
    if not 56<=len(raw)<=96<<20 or raw[:8]!=b'PERFILE2':raise ValueError('bounded perf raw header required')
    hs,stride,ao,an,do,dn=struct.unpack_from('<6Q',raw,8)
    if not 56<=hs<=ao or not 112<=stride<=4096 or an%stride or an//stride>8 or ao+an>len(raw) or do+dn>len(raw):
        raise ValueError('PMU perf raw section bounds')
    allowed={(r['type'],r['config']) for r in identities};ids={}
    for at in range(ao,ao+an,stride):
        kind=struct.unpack_from('<I',raw,at)[0];config=struct.unpack_from('<Q',raw,at+8)[0]
        sample_type=struct.unpack_from('<Q',raw,at+24)[0]
        io,n=struct.unpack_from('<QQ',raw,at+stride-16)
        if n%8 or n>1024 or io+n>len(raw):raise ValueError('PMU event ID bounds')
        for z in range(io,io+n,8):ids[struct.unpack_from('<Q',raw,z)[0]]=(kind,config,sample_type)
    at=do;counts=collections.Counter();periods=collections.Counter();tasks=[];lost=0;records=0;abis=collections.Counter()
    while at<do+dn:
        if at+8>do+dn:raise ValueError('truncated record header')
        kind,misc,size=struct.unpack_from('<IHH',raw,at);records+=1
        if size<8 or at+size>do+dn or records>200000:raise ValueError('PMU raw record bound')
        if kind==2:
            if size<24:raise ValueError('truncated lost record')
            lost+=struct.unpack_from('<Q',raw,at+16)[0]
        elif kind==13:
            if size<16:raise ValueError('truncated lost-samples record')
            lost+=struct.unpack_from('<Q',raw,at+8)[0]
        elif kind in (4,7):
            if size<32:raise ValueError('truncated task record')
            process,parent,tid,ptid,when=struct.unpack_from('<4IQ',raw,at+8)
            if process==pid and start_ns<=when<=end_ns:
                tasks.append(dict(kind='FORK' if kind==7 else 'EXIT',pid=process,parent_pid=parent,
                    tid=tid,parent_tid=ptid,time_ns=when))
                if len(tasks)>4096:raise ValueError('task sideband limit')
        elif kind==9:
            if size<56:raise ValueError('truncated CPU sample')
            identifier,ip,process,tid,when,cpu,reserved,period=struct.unpack_from('<QQIIQIIQ',raw,at+8)
            event=ids.get(identifier)
            if event is None or event[:2] not in allowed or event[2]!=78247:raise ValueError('unexpected cycles sample layout/event')
            if reserved or process!=pid:raise ValueError('unexpected CPU sample target/reserved field')
            z=at+56
            if z+8>at+size:raise ValueError('missing callchain length')
            nr=struct.unpack_from('<Q',raw,z)[0];z+=8
            if nr>8192 or z+nr*8+8>at+size:raise ValueError('callchain bound')
            z+=nr*8;abi=struct.unpack_from('<Q',raw,z)[0];z+=8
            if abi not in (0,2):raise ValueError('unknown GPR ABI')
            z+=264 if abi==2 else 0
            if z+8>at+size:raise ValueError('truncated GPR/stack fields')
            stack=struct.unpack_from('<Q',raw,z)[0];z+=8
            if stack>16384 or z+stack+(8 if stack else 0)!=at+size:raise ValueError('user stack bound')
            if stack and struct.unpack_from('<Q',raw,z+stack)[0]>stack:raise ValueError('valid stack bytes exceed capture')
            if start_ns<=when<=end_ns:
                mode='K' if misc&7==1 else 'U' if misc&7==2 else 'unknown'
                counts[tid,cpu,mode]+=1;periods[tid,cpu,mode]+=period;abis[abi]+=1
        at+=size
    return dict(sample_count=sum(counts.values()),lost_samples=lost,
        running_sample_CPU_TID=[dict(tid=tid,cpu=cpu,mode=mode,samples=n,cycle_period_sum=periods[tid,cpu,mode]) for (tid,cpu,mode),n in sorted(counts.items())],
        sampled_tids=sorted({tid for tid,cpu,mode in counts}),GPR_ABI_sample_counts=dict(abis),task_events=tasks,
        created_tids=sorted({r['tid'] for r in tasks if r['kind']=='FORK'}),
        exited_tids=sorted({r['tid'] for r in tasks if r['kind']=='EXIT'}),
        scope='Only actual running cycles samples and captured task sideband in original client interval; not wall-time/core residency or offCPU duration. Period units are cycles, not nanoseconds.')


def parse_stat(path,expected_tids):
    """Read unscaled, per-thread CSV; never turn unavailable rows into zero."""
    with Path(path).open('rb') as stream:raw=stream.read((1<<20)+1)
    if len(raw)>1<<20:raise ValueError('PMU stat CSV exceeds1MiB')
    rows={};dormant=[]
    for line in raw.decode().splitlines():
        if not line.strip() or line.lstrip().startswith('#'):continue
        fields=[v.strip() for v in line.split(';')]
        indexes=[i for i,v in enumerate(fields) if v in EVENTS]
        if not indexes:continue  # Optional derived metric continuation, no event.
        if len(indexes)!=1 or indexes[0] not in (3,4):raise ValueError('unexpected per-thread perf CSV layout')
        i=indexes[0];match=re.fullmatch(r'(?:.*-)?([0-9]+)',fields[0])
        if not match or (i==4 and fields[1]!='1') or len(fields)<=i+2:raise ValueError('missing per-thread counter identity')
        tid=int(match[1]);event=fields[i];key=(tid,event)
        if key in rows:raise ValueError('duplicate per-thread event')
        text=fields[i-2];value=None if text.startswith('<') else float(text)
        runtime=float(fields[i+1]);running=float(fields[i+2].rstrip('%'))
        if (value is not None and (not math.isfinite(value) or value<0)) or not math.isfinite(runtime) or runtime<0 or not math.isfinite(running):
            raise ValueError('nonfinite or negative PMU measurement')
        rows[key]=dict(tid=tid,event=event,value=value,raw_value=text,unit=fields[i-1],
            runtime_ns=runtime,percent_running=running)
    expected={(tid,event) for tid in expected_tids for event in EVENTS}
    if set(rows)!=expected:raise ValueError('stat target TIDs/events differ from captured initial set')
    for tid in expected_tids:
        hardware=[rows[tid,event] for event in EVENTS[:2]];clock=rows[tid,'task-clock']
        inactive=(clock['value']==0 and clock['runtime_ns']==0 and all(r['runtime_ns']==0 and r['value'] in (None,0) and r['raw_value']!='<not supported>' for r in hardware))
        if inactive:dormant.append(tid);continue
        if any(r['value'] is None or r['value']<=0 or r['runtime_ns']<=0 or r['percent_running']!=100 for r in [*hardware,clock]):
            raise ValueError('active target event unsupported/not counted/multiplexed')
        if hardware[0]['runtime_ns']!=hardware[1]['runtime_ns']:raise ValueError('grouped hardware event runtimes differ')
        if clock['unit'].lower() not in ('msec','nsec'):raise ValueError('unknown task-clock unit')
    totals={event:sum(rows[tid,event]['value'] or 0 for tid in expected_tids) for event in EVENTS}
    clock_ns=sum((rows[tid,'task-clock']['value'] or 0)*(1e6 if rows[tid,'task-clock']['unit'].lower()=='msec' else 1) for tid in expected_tids)
    if min(totals.values())<=0:raise ValueError('positive target counts required')
    return dict(rows=list(rows.values()),initial_target_tids=list(expected_tids),dormant_zero_CPU_tids=dormant,
        totals=totals,task_clock_ns=clock_ns,cycles_per_instruction=totals['cycles']/totals['instructions'],
        cycles_per_task_clock_ns=totals['cycles']/clock_ns,scaled=False,
        scope='Only initial attached TIDs. Boundary lists do not prove absence of transient new threads.',
        running_percent_scope='100 is perf reported precision, not proof of absolutely zero sub-display-precision multiplexing; raw runtime and grouped checks retained.')


def controlled_stat(client,pid,output,perf,record,*,deadline=None):
    """Enable ACK -> original client -> disable ACK -> SIGINT flush/reap."""
    output=Path(output);output.mkdir(parents=True,exist_ok=False)
    csv=output/'stat.csv';log_path=output/'stat.log';fds=[];process=watch=None;stop=threading.Event()
    def remaining(seconds):
        value=min(seconds,deadline-time.monotonic()) if deadline is not None else seconds
        if value<=0:raise TimeoutError('PMU stat wall bound exceeded')
        return value
    def snapshot(label):
        began=time.monotonic_ns();tids=thread_ids(pid)
        record.setdefault('thread_boundaries',[]).append(dict(label=label,tids=tids,
            started_monotonic_ns=began,ended_monotonic_ns=time.monotonic_ns()))
        return tids
    try:
        initial=snapshot('before_stat_attach');record['initial_tids']=initial
        read_ctl,write_ctl=os.pipe();fds.extend((read_ctl,write_ctl));read_ack,write_ack=os.pipe();fds.extend((read_ack,write_ack))
        os.set_blocking(write_ctl,False);os.set_blocking(read_ack,False)
        command=[perf,'stat','--per-thread','--no-big-num','--no-scale','-x',';',
            '-e','{cycles,instructions}','-e','task-clock','--delay=-1',
            f'--control=fd:{read_ctl},{write_ack}','-p',str(pid),'-o',str(csv)]
        record.update(command=command,control=[],started_monotonic_ns=time.monotonic_ns(),
            count_boundary='Enabled inside enable command/ACK bracket; disabled inside disable command/ACK bracket. Original client timestamps retained separately. SIGINT flush occurs after disable ACK.',
            target_scope='PID attachment and explicit per-TID CSV; driver/DB processes excluded. No future-thread coverage assumed.')
        with log_path.open('xb') as log:
            process=subprocess.Popen(command,stdout=log,stderr=log,stdin=subprocess.DEVNULL,
                env={**os.environ,'LC_ALL':'C'},start_new_session=True,pass_fds=(read_ctl,write_ack))
            record['perf_pid']=process.pid
            os.close(read_ctl);fds.remove(read_ctl);os.close(write_ack);fds.remove(write_ack)
            def control(command,seconds):
                began=time.monotonic_ns();until=time.monotonic()+remaining(seconds);data=b'';payload=(command+'\n').encode()
                if os.write(write_ctl,payload)!=len(payload):raise RuntimeError('partial perf stat control write')
                while len(data)<5:
                    left=until-time.monotonic()
                    if left<=0 or not select.select([read_ack],[],[],max(0,left))[0]:raise TimeoutError('perf stat ACK timeout')
                    chunk=os.read(read_ack,65-len(data));data+=chunk
                    if not chunk or len(data)>64:raise RuntimeError('perf stat ACK EOF/bound')
                if data!=b'ack\n\0':raise RuntimeError('unexpected perf stat ACK frame')
                record['control'].append(dict(command=command,started_monotonic_ns=began,
                    ack_monotonic_ns=time.monotonic_ns(),ack_wire_bytes=len(data)))
            def bounded():
                while not stop.wait(.1):
                    if process.poll() is not None:return
                    if any(p.exists() and p.stat().st_size>1<<20 for p in (csv,log_path)):
                        record['output_limit_exceeded']=True
                        try:os.killpg(process.pid,signal.SIGKILL)
                        except ProcessLookupError:pass
                        return
            watch=threading.Thread(target=bounded,daemon=True);watch.start();control('enable',8);snapshot('enable_ACK')
            snapshot('before_original_client');record['client_started_monotonic_ns']=time.monotonic_ns()
            try:result=client()
            finally:record['client_finished_monotonic_ns']=time.monotonic_ns();snapshot('after_original_client')
            control('disable',2);snapshot('disable_ACK')
            record['flush_signal_monotonic_ns']=time.monotonic_ns();os.killpg(process.pid,signal.SIGINT)
            process.wait(timeout=remaining(5))
            # perf-stat's sig_atexit re-raises the deliberate SIGINT after
            # printing. A direct Popen therefore normally returns -SIGINT.
            record['flush_exit_expected']=process.returncode in (0,-signal.SIGINT)
        record['counts']=parse_stat(csv,initial)
        sets=[set(r['tids']) for r in record['thread_boundaries']]
        record['created_at_boundaries']=sorted(set.union(*sets)-sets[0]);record['exited_at_boundaries']=sorted(sets[0]-sets[-1])
        record['stable_boundary_TIDs']=all(s==sets[0] for s in sets)
        record['transient_thread_history']='unknown outside middle-window raw FORK/EXIT evidence; equal endpoint TIDs are not full lifecycle tracing'
        return result
    except BaseException as error:
        record['observer_error']=repr(error);raise
    finally:
        stop.set()
        if watch is not None:watch.join(timeout=1)
        if process is not None and process.poll() is None:
            try:os.killpg(process.pid,signal.SIGKILL)
            except ProcessLookupError:pass
            record['forced_cleanup']=True
            try:process.wait(timeout=1)
            except subprocess.TimeoutExpired:record['cleanup_error']='owned perf stat not reaped after KILL'
        for fd in fds:os.close(fd)
        record.update(ended_monotonic_ns=time.monotonic_ns(),exit_code=process.returncode if process is not None else None,
            observer_thread_stopped=watch is None or not watch.is_alive())
        record['complete']=(record.get('flush_exit_expected') is True and record['exit_code'] in (0,-signal.SIGINT)
            and record.get('stable_boundary_TIDs') is True and 'counts' in record
            and record['observer_thread_stopped'] and not record.get('output_limit_exceeded')
            and not record.get('forced_cleanup') and 'observer_error' not in record)
