#!/usr/bin/env python3
"""One bounded diagnostic snapshot of an LD_PRELOAD glibc live table.

Usage: resident_snapshot.py PID /absolute/resident-live.so /new/output-directory
Requires Linux /proc/PID/mem read permission. No gdb inferior calls. Never
signals a pre-stopped process. Always resumes a process this tool stopped.
"""
import hashlib
import json
import os
from pathlib import Path
import signal
import struct
import subprocess
import sys
import time


def read_text(path, limit):
    with path.open('rb') as stream:
        raw = stream.read(limit+1)
    if len(raw)>limit:
        raise RuntimeError('snapshot file exceeds bounded read: '+str(path))
    return raw.decode()


def maps_parse(raw):
    result = []
    for line in raw.splitlines():
        cells = line.split(None, 5)
        start, end = [int(x, 16) for x in cells[0].split('-')]
        result.append(dict(start=start, end=end, perms=cells[1], offset=int(cells[2], 16), device=cells[3], inode=int(cells[4]),
                           path=cells[5] if len(cells) == 6 else ''))
    return result


def read_exact(fd, address, count):
    raw = os.pread(fd, count, address)
    if len(raw) != count:
        raise RuntimeError('short /proc mem read')
    return raw


def stopped_threads(proc):
    paths = list((proc / 'task').glob('*/status'))
    if len(paths)>128:
        raise RuntimeError('snapshot thread limit')
    stopped = bool(paths) and all(next(line for line in read_text(p,65536).splitlines()
                                     if line.startswith('State:')).split()[1] == 'T' for p in paths)
    return sorted(p.parent.name for p in paths) if stopped else None


def elf_identity(path, expected_inode=None):
    before = path.stat()
    if (expected_inode is not None and before.st_ino != expected_inode) or before.st_size > 64 * 1024 * 1024:
        raise RuntimeError('module inode mismatch or oversized ELF')
    digest = hashlib.sha256(path.read_bytes()).hexdigest()
    notes = subprocess.check_output(['readelf', '-n', '--wide', str(path)], text=True, timeout=2)
    build_id = next((line.split('Build ID:', 1)[1].strip() for line in notes.splitlines()
                     if 'Build ID:' in line), None)
    after = path.stat()
    if (before.st_ino, before.st_size, before.st_mtime_ns) != (after.st_ino, after.st_size, after.st_mtime_ns):
        raise RuntimeError('module changed during identity capture')
    return dict(sha256=digest, build_id=build_id, bytes=after.st_size, inode=after.st_ino)


def dependency_owners(proc, mappings, smaps_raw):
    """ELF full-page zero-fill tails, with conservative VMA-wide anon bounds."""
    vmas=[]
    for line in smaps_raw.splitlines():
        if not line.strip():continue
        if '-' in line.split()[0] and len(line.split())>=5:
            vmas.extend(maps_parse(line))
        elif line.startswith('Anonymous:') and vmas:
            vmas[-1]['anonymous_bytes']=int(line.split()[1])*1024
    page=os.sysconf('SC_PAGE_SIZE');up=lambda n:(n+page-1)//page*page
    rows=[]
    for path in sorted(set(m['path'] for m in mappings if m['path'].startswith('/') and '.so' in m['path'])):
        if path.endswith(' (deleted)'):continue
        file=proc/'root'/path.lstrip('/')
        with file.open('rb') as stream:
            header=stream.read(64)
            if header[:6]!=b'\x7fELF\x02\x01':raise RuntimeError('expected little-endian ELF64 dependency')
            offset=struct.unpack_from('<Q',header,32)[0];width,count=struct.unpack_from('<HH',header,54)
            if width!=56 or not 0<count<=128 or offset+width*count>file.stat().st_size:
                raise RuntimeError('invalid bounded ELF load headers')
            stream.seek(offset);segments=list(struct.iter_unpack('<IIQQQQQQ',stream.read(width*count)))
        loads=[s for s in segments if s[0]==1];mapped=[v for v in vmas if v['path']==path]
        first=next(v for v in mapped if v['offset']==0);segment=next(s for s in loads if s[2]==0)
        bias=first['start']-(segment[3]//page*page);tails=[]
        for s in loads:
            lo,hi=bias+up(s[3]+s[5]),bias+up(s[3]+s[6])
            for v in vmas:
                overlap=max(0,min(hi,v['end'])-max(lo,v['start']))
                if not overlap:continue
                anon=v.get('anonymous_bytes',0)
                tails.append(dict(range=[hex(lo),hex(hi)],vma_start=hex(v['start']),vma_path=v['path'],
                    anonymous_lower_bytes=max(0,anon-(v['end']-v['start']-overlap)),
                    anonymous_upper_bytes=min(anon,overlap)))
        rows.append(dict(path=path,named_mapping_anonymous_bytes=sum(v.get('anonymous_bytes',0) for v in mapped),
                         ELF_zero_fill_tail_intersections=tails))
    return dict(dependencies=rows,scope='Named private anonymous DSO pages and ELF zero-fill tails are not malloc objects. Tail residency is bounded from VMA-wide Anonymous, never inferred from virtual size. Do not add RSS/PSS/cgroup axes.')


def capture(pid, library, destination):
    pid, library, destination = int(pid), Path(library).resolve(), Path(destination)
    began_ns=time.monotonic_ns()
    proc = Path('/proc') / str(pid)
    if pid <= 1 or pid == os.getpid() or not library.is_absolute():
        raise RuntimeError('invalid target')
    status = read_text(proc/'status',65536)
    if next(line for line in status.splitlines() if line.startswith('State:')).split()[1] in ('T', 't'):
        raise RuntimeError('refuse target that was already stopped')
    symbols = {}
    for line in subprocess.check_output(['nm', '-D', '--defined-only', str(library)], text=True, timeout=2).splitlines():
        cells = line.split()
        if len(cells) == 3 and cells[2] in ('ku_resident_status', 'ku_resident_records'):
            symbols[cells[2]] = int(cells[0], 16)
    if len(symbols) != 2:
        raise RuntimeError('missing exported diagnostic symbols')
    destination.mkdir(parents=False, exist_ok=False)
    memfd = os.open(proc / 'mem', os.O_RDONLY)
    held = False
    attempts = 0; intervals=[]; stopped_ns=None; confirmed_ns=None
    def resumed():
        nonlocal held,stopped_ns,confirmed_ns
        resume_ns=time.monotonic_ns()
        try:os.kill(pid,signal.SIGCONT)
        except ProcessLookupError:pass
        intervals.append(dict(started_monotonic_ns=stopped_ns,threads_stopped_monotonic_ns=confirmed_ns,
            resume_requested_monotonic_ns=resume_ns,ended_monotonic_ns=time.monotonic_ns()))
        held=False;stopped_ns=None;confirmed_ns=None
    def stop_budget():
        if stopped_ns is not None and time.monotonic_ns()-stopped_ns>8_000_000_000:
            raise TimeoutError('idle snapshot eight-second STOP work budget')
    try:
        for attempts in range(1, 11):
            held = True; stopped_ns=time.monotonic_ns()
            os.kill(pid, signal.SIGSTOP)
            deadline = time.monotonic() + 2
            while not (thread_ids := stopped_threads(proc)):
                if time.monotonic() >= deadline:
                    raise RuntimeError('threads did not stop within two seconds')
                time.sleep(.01)
            confirmed_ns=time.monotonic_ns()
            maps_raw = read_text(proc/'maps',1<<20)
            mappings = maps_parse(maps_raw)
            matches = [m for m in mappings if m['path'] == str(library)]
            if not matches:
                raise RuntimeError('exact observer library is not mapped')
            if any(m['inode'] != library.stat().st_ino for m in matches):
                raise RuntimeError('mapped observer inode differs from local library')
            base = min(m['start'] - m['offset'] for m in matches)
            words = struct.unpack('<11Q', read_exact(memfd, base + symbols['ku_resident_status'], 88))
            if words[0] != 1 or words[1] != 16384:
                raise RuntimeError('unsupported observer layout')
            if not words[2] and not words[3] and stopped_threads(proc) == thread_ids:
                break
            resumed()
            time.sleep(.025)
        else:
            raise RuntimeError('no quiescent allocation boundary within ten attempts')
        records_raw = read_exact(memfd, base + symbols['ku_resident_records'], 16384 * 40)
        records = []
        for pointer, size, pc, tid, kind in struct.iter_unpack('<5Q', records_raw):
            stop_budget()
            if not pointer:
                continue
            owner = next((m for m in mappings if m['start'] <= pc-1 < m['end']), None)
            target = next((m for m in mappings if m['start'] <= pointer < m['end']), None)
            records.append(dict(pointer=hex(pointer), requested_bytes=size, return_pc=hex(pc), symbol_probe_pc=hex(pc-1), tid=tid,
                                kind=kind, callsite_file=owner['path'] if owner else None,
                                return_file_offset=hex(pc-owner['start']+owner['offset']) if owner else None,
                                symbol_probe_file_offset=hex(pc-1-owner['start']+owner['offset']) if owner else None,
                                allocation_vma_start=hex(target['start']) if target else None))
        smaps_raw = read_text(proc/'smaps',4<<20)
        task_status = {tid: read_text(proc/'task'/tid/'status',65536) for tid in thread_ids}
        if len(records) != words[9]:
            raise RuntimeError('live-count mismatch')
        if sorted(task_status) != thread_ids or stopped_threads(proc) != thread_ids:
            raise RuntimeError('thread list changed during stopped snapshot')
        modules = {}
        paths=set(r['callsite_file'] for r in records if r['callsite_file'])
        paths.update(m['path'] for m in mappings if m['path'].startswith('/') and '.so' in m['path'])
        if len(paths)>64:raise RuntimeError('caller module limit')
        for path in sorted(paths):
            stop_budget()
            mapping = next(m for m in mappings if m['path'] == path)
            if not path.startswith('/') or path.endswith(' (deleted)'):
                modules[path] = dict(error='no stable file path')
                continue
            try:
                modules[path] = elf_identity(proc / 'root' / path.lstrip('/'), mapping['inode'])
            except (OSError, RuntimeError, subprocess.SubprocessError) as error:
                modules[path] = dict(error=str(error))
        executable = elf_identity(proc/'exe')
        stop_budget()
        dependencies = dependency_owners(proc,mappings,smaps_raw)
        import http_cost_diagnostic as hcd
        owner_ranges = dict(complete=not words[4] and not words[5], records=[dict(pid=pid,
            label=(r['callsite_file'] or 'unknown')+':'+str(r['symbol_probe_file_offset']), context=str(r['tid']),
            pointer=r['pointer'],requested_bytes=r['requested_bytes']) for r in records if r['requested_bytes']])
        ticks = read_text(proc/'stat',65536).rsplit(')',1)[1].split()[19]
        pages = hcd.allocation_owner_pages(pid,owner_ranges,ticks,maximum_records=16384)
        stop_budget()
        value=dict(
            schema='ku-resident-live-v1', pid=pid, attempts=attempts, observer_sha256=hashlib.sha256(library.read_bytes()).hexdigest(),
            status=dict(zip(('version', 'capacity', 'inflight', 'lock', 'lost', 'recursive', 'allocations',
                             'frees', 'untracked_frees', 'live_count', 'peak_live'), words)),
            live_table_valid=not words[4] and not words[5], modules=modules, executable=executable,
            pages=pages,
            dependency_owners=dependencies,
            observer_storage=dict(records_start=hex(base+symbols['ku_resident_records']),
                                  records_bytes=16384*40, status_start=hex(base+symbols['ku_resident_status']),
                                  status_bytes=88, mapped_library=[m for m in mappings if m['path']==str(library)]),
            scope='Interposed allocation boundaries only. libc private allocations, TLS/TCB, loader storage and raw mmap may be unobserved. Requested bytes are not resident bytes. Observer mappings and CPU are excluded from product comparisons.',
            coverage_complete=False, idle_only=True, normal_score_eligible=False, threads=task_status, records=records)
    finally:
        if held:resumed()
        os.close(memfd)
    value.update(capture_started_monotonic_ns=began_ns,capture_ended_monotonic_ns=time.monotonic_ns(),
        stop_intervals=intervals,total_stopped_ns=sum(i['ended_monotonic_ns']-i['started_monotonic_ns'] for i in intervals),
        total_confirmed_stopped_ns=sum(i['resume_requested_monotonic_ns']-i['threads_stopped_monotonic_ns'] for i in intervals if i['threads_stopped_monotonic_ns'] is not None),
        stop_timing_scope='total_stopped_ns spans STOP request to CONT return; confirmed span begins after all threads were observed T and ends at CONT request. Kernel scheduling boundaries are not directly observed.')
    (destination/'maps.log').write_text(maps_raw)
    (destination/'smaps.log').write_text(smaps_raw)
    (destination/'live.json').write_text(json.dumps(value,separators=(',',':'))+'\n')
    return dict(output=str(destination), records=len(records), attempts=attempts, live_table_valid=not words[4] and not words[5])


if __name__ == '__main__':
    print(json.dumps(capture(int(sys.argv[1]), sys.argv[2], sys.argv[3])))
