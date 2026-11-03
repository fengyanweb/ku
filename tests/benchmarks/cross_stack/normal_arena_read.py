#!/usr/bin/env python3
"""One owned normal-Release allocator observation. Never a performance score."""
import argparse, copy, json, os, platform, re, shlex, shutil, signal, subprocess, tempfile, time
from pathlib import Path
import xml.etree.ElementTree as ET
import focus_cycle as fc
import hosted_cycle as h
import linux_harness as lh

BASE='166453a1d2768bbce19796d9c7823be73cc84ccb'
ELF='234300bba9b6fce603a178701ac289a82108398d5f35de3c0a7ad585f082367b'
XML_LIMIT=1<<20


def xml_totals(raw):
    if not 0<len(raw)<=XML_LIMIT or b'<!' in raw:raise ValueError('bounded plain allocator XML required')
    root=ET.fromstring(raw)
    if root.tag!='malloc' or root.get('version')!='1':raise ValueError('unknown malloc_info schema')
    totals={}
    for item in root.findall('total'):
        kind=item.get('type');size=item.get('size','')
        if kind in totals or not re.fullmatch('[0-9]{1,20}',size):raise ValueError('ambiguous allocator global total')
        totals[kind]=int(size)
    if not {'fast','rest'}<=totals.keys():raise ValueError('allocator free totals absent')
    return dict(global_totals=totals,free_bytes=totals['fast']+totals['rest'],heaps=len(root.findall('heap')),
        boundary='Only direct malloc/total fast+rest; heap subtotals overlap. Free bytes are not resident or trim-releasable pages.')


def owned(service,expected=None):
    pid=service.process.pid;proc=Path('/proc')/str(pid)
    stat=(proc/'stat').read_text();start=stat.rsplit(')',1)[1].split()[19]
    value=dict(pid=pid,start_ticks=start,exe_sha256=h.sha(proc/'exe'))
    if (service.process.poll() is not None or str(pid) not in (service.domain.path/'cgroup.procs').read_text().split()
            or value['exe_sha256']!=ELF or (expected is not None and value!=expected)):
        raise ValueError('owned live normal ELF identity changed')
    return value


def snapshot(service,directory,label,identity):
    owned(service,identity);proc=Path('/proc')/str(identity['pid']);threads=[]
    for task in sorted((proc/'task').iterdir(),key=lambda x:int(x.name))[:128]:
        row={'tid':int(task.name)}
        for name in ('stat','wchan','syscall'):
            try:
                with (task/name).open('rb') as stream:raw=stream.read(4097)
                row[name]=raw[:4096].decode(errors='replace');row[name+'_truncated']=len(raw)>4096
            except OSError as error:row[name]={'unknown':repr(error)}
        threads.append(row)
    value=dict(identity=identity,monotonic_ns=time.monotonic_ns(),threads=threads,
        memory=h.diagnostic_memory(service,directory/(label+'-smaps.log')),
        boundary='Sequential endpoint observations, not simultaneous allocator/object/RSS ownership.')
    h.save(directory/(label+'.json'),value);return value


def inferior(service,gdb,directory,label,identity,trim=False):
    """One <=10s attach; no retry. Capture offset only after flushing old stdout."""
    owned(service,identity);log=service.cwd.parent/'service.log';initial=log.stat().st_size
    offset=directory/(label+'-stdout-offset.txt');output=directory/(label+'-gdb.log')
    operation='malloc_trim(0)' if trim else 'malloc_info(0, *(void**)&stdout)'
    commands=['set pagination off','set confirm off','set scheduler-locking off',
        'call (int)fflush(*(void**)&stdout)',
        'shell stat -c %s -- '+shlex.quote(str(log))+' > '+shlex.quote(str(offset)),
        'printf "KU_ARENA_MALLOC=%p\\n", (void*)malloc',
        'set $ku_arena_rc = (int)'+operation,'call (int)fflush(*(void**)&stdout)',
        'printf "KU_ARENA_RC=%d\\n", $ku_arena_rc','detach','quit']
    argv=[gdb,'--quiet','--nx','--nh','--batch','--pid',str(identity['pid'])]
    for command in commands:argv.extend(['-ex',command])
    process=None;began=time.monotonic();record=dict(command=argv,diagnostic_only=True,maximum_seconds=10,
        maximum_stdout_increment_bytes=XML_LIMIT,operation=operation,retry=False)
    try:
        with output.open('xb') as stream:
            process=subprocess.Popen(argv,stdout=stream,stderr=stream,stdin=subprocess.DEVNULL,start_new_session=True)
            while process.poll() is None:
                if time.monotonic()-began>10 or output.stat().st_size>262144 or log.stat().st_size-initial>XML_LIMIT:
                    raise TimeoutError('bounded gdb time/output/inferior stdout limit')
                time.sleep(.025)
        record['exit_code']=process.returncode
        text=output.read_text(errors='replace');returns=re.findall(r'^KU_ARENA_RC=(-?\d+)$',text,re.M)
        addresses=re.findall(r'^KU_ARENA_MALLOC=(0x[0-9a-fA-F]+)$',text,re.M)
        if process.returncode or len(returns)!=1 or len(addresses)!=1:raise ValueError('gdb call/detach or result unavailable')
        record.update(return_value=int(returns[0]),malloc_address=addresses[0],glibc_version='unknown: not queried by another inferior call')
        if record['return_value'] not in ((0,1) if trim else (0,)):raise ValueError('allocator call failed')
        start=int(offset.read_text().strip());end=log.stat().st_size
        if not initial<=start<=end or end-start>XML_LIMIT:raise ValueError('inferior stdout increment exceeded bound')
        with log.open('rb') as stream:stream.seek(start);raw=stream.read(end-start)
        (directory/(label+'-stdout.log')).write_bytes(raw);record['stdout_bytes']=len(raw)
        if not trim:
            matches=re.findall(rb'<malloc\b[^>]*>.*?</malloc>',raw,re.S)
            if len(matches)!=1:raise ValueError('exactly one complete allocator XML document required')
            (directory/(label+'.xml')).write_bytes(matches[0]);record['allocator']=xml_totals(matches[0])
        owned(service,identity);return record
    except BaseException as error:record['error']=repr(error);raise
    finally:
        if process is not None and process.poll() is None:
            try:os.killpg(process.pid,signal.SIGKILL)
            except ProcessLookupError:pass
            try:process.wait(timeout=2)
            except subprocess.TimeoutExpired:record['gdb_reap_failed']=True
            record['forced_gdb_cleanup']=True
        record['seconds']=time.monotonic()-began;h.save(directory/(label+'-call.json'),record)


def main(argv=None):
    p=argparse.ArgumentParser(description=__doc__);p.add_argument('--root',required=True,type=Path);p.add_argument('--manifest',required=True,type=Path)
    a=p.parse_args(argv)
    if os.geteuid()!=0 or platform.system()!='Linux' or platform.machine() not in ('arm64','aarch64'):p.error('owned Linux ARM64/cgroup root required')
    if any(v for k,v in os.environ.items() if k.startswith(('LD_','MALLOC_')) or k=='GLIBC_TUNABLES'):p.error('normal allocator/library environment required')
    gdb=shutil.which('gdb')
    if not gdb:p.error('gdb unavailable; no service started')
    a.root=a.root.resolve();a.manifest=a.manifest.resolve(strict=True)
    if a.root==a.manifest.parent or a.root in a.manifest.parents or a.manifest.parent in a.root.parents:p.error('fresh independent output required')
    a.root.mkdir(parents=True,exist_ok=False);evidence=a.root/'evidence';evidence.mkdir()
    def stop(*unused):raise KeyboardInterrupt('normal allocator coordinator wall bound')
    signal.signal(signal.SIGTERM,stop);signal.signal(signal.SIGALRM,stop);signal.alarm(180)
    manifest=h.load(a.manifest);spec=dict(root=str(a.manifest.parent),stack='ku',source_commit=BASE,binary_sha256=ELF)
    binding=fc.version_identity({'versions':{'A':spec}},'A')
    expected=dict(formatting_mode='helpers',json_response_mode='text',json_execution_mode='worker',pg_execution_mode='async',pg_read_mode='int',native_flags={'batch':['-std=c11','-O2'],'server':['-std=c11','-O2','-pthread']})
    if manifest['candidate_base']!=BASE or binding['build_mode']!=expected:raise ValueError('frozen normal A166/helpers/O2 required')
    if binding['files']['expanded source']['sha256']!='59ede7cdba403d4d038c3720ab65f1cd940efd1038d07df782743a35eb9887ae' or binding['files']['generated C']['sha256']!='830536089f72ce56118e4e6a1556bbe007f816cc16a3c2f96c0dcff010d14e77':raise ValueError('frozen normal business/generated C differs')
    h.save(evidence/'plan.json',dict(binding=binding,entry_sha256=h.sha(__file__),gdb=gdb,gdb_sha256=h.sha(gdb),diagnostic_only=True,performance_score=False,checkpoints=['ready','one-db'],maximum_info_calls=2,maximum_trim_calls=1,trim_free_threshold=131072,maximum_retries=0,
        boundary='gdb inferior calls can change allocator state and disturb scheduling. malloc_trim deliberately changes it; none of these are normal memory/performance scores.'))
    service=fixture=None;result=dict(execution_complete=False,diagnostic_only=True,performance_score=False,candidate_accepted=False)
    temporary=tempfile.TemporaryDirectory(prefix='ka-',dir='/tmp');dbroot=Path(temporary.name);dbroot.chmod(0o755)
    try:
        dbmanifest=copy.deepcopy(manifest);dbmanifest['stacks']={};h.save(dbroot/'manifest.json',dbmanifest)
        fixture=h.db.Database(dbroot,manifest['uid'],manifest['gid']);fixture.start();private=h.load(fixture.manifest_path)
        harness=lh.Harness(a.manifest.parent,a.root/'harness');harness.manifest=copy.deepcopy(harness.manifest)
        harness.manifest['db_url']=private['db_url'];harness.db_domain=lh.ExternalDomain(private['db_cgroup'])
        service=harness.start('ku',cores=2,memory=h.RESOURCES['service_memory_bytes'],output=a.root/'service')
        identity=owned(service);h.save(evidence/'process-identity.json',identity)
        maps=(Path('/proc')/str(identity['pid'])/'maps').read_bytes()
        if len(maps)>XML_LIMIT:raise ValueError('maps limit')
        (evidence/'maps.log').write_bytes(maps)
        libraries=[]
        for line in maps.decode().splitlines():
            fields=line.split(None,5)
            if len(fields)==6 and Path(fields[5]).name=='libc.so.6' and not libraries:
                libc=Path(fields[5]);st=libc.stat();device=fields[3].split(':')
                if st.st_ino!=int(fields[4]) or (os.major(st.st_dev),os.minor(st.st_dev))!=tuple(int(x,16) for x in device):raise ValueError('mapped libc identity changed')
                libraries.append(dict(mapped_path=str(libc),sha256=h.sha(libc),bytes=st.st_size,inode=st.st_ino))
        h.save(evidence/'allocator-library.json',dict(libc=libraries or 'unknown',version='unknown; no extra inferior call'))
        for label in ('ready','one-db'):
            if label=='one-db':
                status,body=lh.request(service.address,'/db?key=1&rid=arena1',timeout=3)
                if status!=200 or json.loads(body)!={'id':1,'label':'item-1-中文🙂','score':17,'rid':'arena1'}:raise ValueError('single public PG request incorrect; no retry')
                h.save(evidence/'single-db.json',dict(status=status,correct=True,requests=1))
            snapshot(service,evidence,label+'-before',identity)
            call=inferior(service,gdb,evidence,label,identity);snapshot(service,evidence,label+'-after',identity)
            if lh.request(service.address,'/ok?rid=arena',timeout=2)!=(200,b'ok:arena'):raise ValueError('HTTP did not recover after attach')
        result['trim_eligible']=call['allocator']['free_bytes']>=131072
        if result['trim_eligible']:
            snapshot(service,evidence,'trim-before',identity);result['trim']=inferior(service,gdb,evidence,'trim',identity,trim=True)
            snapshot(service,evidence,'trim-after',identity)
            if lh.request(service.address,'/ok?rid=arena',timeout=2)!=(200,b'ok:arena'):raise ValueError('HTTP did not recover after trim')
        result['observation_complete']=True
    except BaseException as error:result['error']=repr(error)
    finally:
        signal.alarm(0)
        if service is not None:
            try:owned(service);os.kill(service.process.pid,signal.SIGCONT)
            except (OSError,ValueError):pass
            try:result['service_cleanup']=service.stop()
            except Exception as error:result['service_cleanup']={'error':repr(error)}
        if fixture is not None:
            try:result['database_cleanup']=fixture.stop()
            except Exception as error:result['database_cleanup']={'error':repr(error)}
            log=fixture.directory/'postgres.log'
            if log.is_file():
                with log.open('rb') as stream:stream.seek(max(0,log.stat().st_size-262144));(evidence/'postgres-tail.log').write_bytes(stream.read(262144))
        temporary.cleanup();cleanup=result.get('database_cleanup',{})
        result['execution_complete']=result.get('observation_complete') is True and lh.service_termination_succeeded('ku',result.get('service_cleanup',{})) and cleanup.get('forced') is False and cleanup.get('cgroup_removed') is True and cleanup.get('after',{}).get('process_count')==0
        h.save(evidence/'summary.json',result)
    return 0 if result['execution_complete'] else 2


if __name__=='__main__':raise SystemExit(main())
