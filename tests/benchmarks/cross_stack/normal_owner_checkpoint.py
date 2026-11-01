#!/usr/bin/env python3
"""One idle checkpoint of fixed owners in an unchanged normal Linux ELF.

prepare builds only an offsetof helper from the exact emitted C. It never runs
Ku main. capture_owned must be called by the direct parent of an already-owned
Popen child after producers have stopped; no stand-alone arbitrary-PID capture
is offered. No service is started here. Metadata reads can fault pages and STOP
perturbs lifecycle, so none of this output is a CPU/memory performance score.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import signal
import struct
import subprocess
import time

import http_cost_diagnostic as hcd
import resident_snapshot as rs

MAX_FILE = 32 << 20
MAX_RANGES = 512
MAX_PAGES = 8192
TEMPLATE = Path(__file__).with_name('normal_owner_layout.c.in')
REQUIRED_BOUNDARY = ('clients_complete', 'request_producers_stopped',
                     'management_polling_stopped', 'isolated_owned_service',
                     'independent_idle_diagnostic', 'normal_uninstrumented_service')


def digest(path, bound=MAX_FILE):
    path = Path(path)
    if not path.is_file() or path.stat().st_size > bound:
        raise ValueError('missing/oversized identity input: ' + str(path))
    return hashlib.sha256(path.read_bytes()).hexdigest()


def bounded_json(path):
    return json.loads(rs.read_text(Path(path), 1 << 20))


def elf_loads(path):
    with Path(path).open('rb') as stream:
        header = stream.read(64)
        if len(header) != 64 or header[:6] != b'\x7fELF\x02\x01':
            raise ValueError('expected little-endian ELF64')
        kind, machine = struct.unpack_from('<HH', header, 16)
        offset = struct.unpack_from('<Q', header, 32)[0]
        width, count = struct.unpack_from('<HH', header, 54)
        if kind not in (2, 3) or machine != 183 or width != 56 or not 0 < count <= 128:
            raise ValueError('expected bounded normal Linux AArch64 ELF')
        if offset + width * count > Path(path).stat().st_size:
            raise ValueError('ELF program table outside file')
        stream.seek(offset)
        segments = [s for s in struct.iter_unpack('<IIQQQQQQ', stream.read(width * count)) if s[0] == 1]
    if not segments:
        raise ValueError('ELF lacks PT_LOAD')
    return kind, segments


def global_symbol(elf, destination=None):
    raw = subprocess.check_output(['nm', '-S', '--defined-only', str(elf)], text=True, timeout=2)
    rows = [line.split() for line in raw.splitlines() if line.split()[-1:] == ['ku_http_live_group']]
    if len(rows) != 1 or len(rows[0]) != 4 or rows[0][2] not in ('b', 'B', 'd', 'D'):
        raise ValueError('one exact data/BSS ku_http_live_group symbol required')
    address, size = int(rows[0][0], 16), int(rows[0][1], 16)
    if size != 8:
        raise ValueError('ku_http_live_group must be exactly one 8-byte pointer')
    if destination is not None:
        Path(destination).write_text(next(line for line in raw.splitlines() if line.split()[-1:] == ['ku_http_live_group']) + '\n')
    return address


def layout_wrapper(source):
    template = TEMPLATE.read_text()
    if template.count('@SOURCE_C_LITERAL@') != 1:
        raise ValueError('layout include marker must occur exactly once')
    if any(c in str(source) for c in ('"', '\\', '\n', '\r')):
        raise ValueError('C header-name cannot represent source path quotes/backslash/newline')
    return template.replace('@SOURCE_C_LITERAL@', json.dumps(str(source), ensure_ascii=False))


def prepare(elf, source, manifest, compiler_receipt, compiler, libpq, source_commit, output):
    """Run on the original Linux ABI/compiler only, before a checkpoint."""
    elf, source, compiler, libpq = [Path(p).resolve(strict=True) for p in (elf, source, compiler, libpq)]
    receipt = bounded_json(compiler_receipt)
    original = bounded_json(manifest)
    native = original['stacks']['ku']['build']['native']['server']
    if original['candidate_base'] != source_commit or native['compiler_invocation'] != receipt:
        raise ValueError('original manifest/compile receipt/product SHA do not agree')
    if receipt.get('kind') != 'compile' or receipt.get('flags') != ['-std=c11', '-O2', '-pthread']:
        raise ValueError('original normal C11/O2/pthread compiler receipt required')
    identities = {'ELF': digest(elf), 'source': digest(source), 'compiler': digest(compiler), 'libpq': digest(libpq)}
    if identities['ELF'] != native['binary_sha256'] or identities['source'] != native['generated_c_sha256']:
        raise ValueError('ELF/C do not match the original normal manifest')
    if identities['source'] != receipt['source']['sha256'] or identities['compiler'] != receipt['compiler']['sha256']:
        raise ValueError('source/compiler bytes differ from normal build')
    libraries = [r for r in receipt['library_inputs'] if r['role'] == 'libpq']
    if len(libraries) != 1 or identities['libpq'] != libraries[0]['sha256']:
        raise ValueError('exact original libpq input required for layout helper')
    if len(source_commit) != 40 or any(c not in '0123456789abcdef' for c in source_commit):
        raise ValueError('original product SHA required, not current equivalent relabel')
    elf_loads(elf)
    output = Path(output).resolve()
    output.mkdir(exist_ok=False)
    symbol = global_symbol(elf, output / 'normal-global-symbol.readback')
    wrapper, helper = output / 'layout-only.c', output / 'layout-only'
    wrapper.write_text(layout_wrapper(source))
    argv = [str(compiler), str(wrapper), '-std=c11', '-O2', '-pthread', str(libpq), '-o', str(helper)]
    built = subprocess.run(argv, capture_output=True, timeout=120)
    (output / 'layout-build.stdout').write_bytes(built.stdout)
    (output / 'layout-build.stderr').write_bytes(built.stderr)
    if built.returncode:
        raise ValueError('layout-only compile failed; see preserved logs')
    elf_loads(helper)
    run = subprocess.run([str(helper)], capture_output=True, timeout=5)
    (output / 'layout.stdout').write_bytes(run.stdout)
    (output / 'layout.stderr').write_bytes(run.stderr)
    if run.returncode or len(run.stdout) > 65536:
        raise ValueError('layout-only helper failed or output exceeded bound')
    layout = json.loads(run.stdout)
    if layout['schema'] != 'ku-normal-owner-layout-v1' or layout['pointer_bytes'] != 8:
        raise ValueError('wrong layout ABI')
    layout_file = output / 'layout.json'
    layout_file.write_text(json.dumps(layout, indent=2) + '\n')
    value = dict(schema='ku-normal-fixed-owner-prepared-v1', product_source_commit=source_commit,
                 ELF=str(elf), source=str(source), compiler=str(compiler), libpq=str(libpq),
                 identities=identities, normal_manifest=str(Path(manifest).resolve()),
                 normal_manifest_SHA256=digest(manifest), normal_compiler_receipt=str(Path(compiler_receipt).resolve()),
                 normal_compiler_receipt_SHA256=digest(compiler_receipt),
                 layout=str(layout_file), layout_SHA256=digest(layout_file),
                 template_SHA256=digest(TEMPLATE), wrapper_SHA256=digest(wrapper),
                 layout_helper_SHA256=digest(helper), layout_build_argv=argv,
                 global_symbol_vaddr=symbol, normal_ELF_unmodified=True, Ku_main_invoked=False,
                 score_eligible=False, original_compiler_receipt=receipt)
    (output / 'prepared.json').write_text(json.dumps(value, indent=2) + '\n')
    return value


def read_start(proc):
    return rs.read_text(proc / 'stat', 65536).rsplit(')', 1)[1].split()[19]


def check_identity(proc, prepared, start, executable):
    if read_start(proc) != str(start):
        raise ValueError('process start identity changed')
    status = rs.read_text(proc / 'status', 65536)
    if int(next(x.split()[1] for x in status.splitlines() if x.startswith('PPid:'))) != os.getpid():
        raise ValueError('only a direct owned child may be inspected')
    target = (proc / 'exe').stat()
    if (target.st_ino, target.st_dev) != executable:
        raise ValueError('owned process executable inode/device changed')
    return status


def load_bias(mappings, elf, inode, device, symbol, page):
    kind, segments = elf_loads(elf)
    # Anchor the ELF-header PT_LOAD at file offset0; duplicate RELRO mappings
    # of later pages are deliberately not candidates for this calculation.
    anchors = [s for s in segments if s[2] == 0 and s[5] >= 64]
    mapped = [m for m in mappings if m['inode'] == inode and m['device'] == device and m['offset'] == 0]
    values = {m['start'] - (s[3] // page * page) for s in anchors for m in mapped}
    if len(values) != 1:
        raise ValueError('unique same-inode ELF-header load bias unavailable')
    bias = values.pop()
    if kind == 2 and bias != 0:
        raise ValueError('ET_EXEC must have zero load bias')
    if not any(s[3] <= symbol and symbol + 8 <= s[3] + s[6] for s in segments):
        raise ValueError('global symbol outside PT_LOAD memory')
    address = bias + symbol
    if not any(m['inode'] == inode and m['device'] == device and 'r' in m['perms']
               and m['start'] <= address and address + 8 <= m['end'] for m in mappings):
        raise ValueError('global address outside exact readable executable mapping')
    return bias


class OwnerReader:
    """Read only compiled scalar fields; never export object/heap contents."""
    def __init__(self, fd, mappings, layout, pid, deadline):
        self.fd, self.maps, self.layout, self.pid = fd, mappings, layout, pid
        self.deadline = deadline
        self.records, self.metadata, self.missing = [], [], []

    def budget(self):
        if time.monotonic() > self.deadline:
            raise TimeoutError('normal fixed-owner eight-second stopped-work budget')

    def mapped(self, pointer, count):
        self.budget()
        if not isinstance(pointer, int) or not 0 < pointer < pointer + count <= 1 << 64:
            raise ValueError('invalid bounded address')
        if sum(max(0, min(pointer + count, m['end']) - max(pointer, m['start']))
               for m in self.maps if 'r' in m['perms']) != count:
            raise ValueError('declared field/range outside readable mappings')

    def word(self, pointer, count):
        if count not in (4, 8):
            raise ValueError('only4/8-byte compiled scalar reads are allowed')
        self.mapped(pointer, count)
        return int.from_bytes(rs.read_exact(self.fd, pointer, count), 'little')

    def field(self, pointer, typename, name):
        t = self.layout['types'][typename]
        f = t['fields'][name]
        if not 0 <= f['offset'] < t['size'] or f['offset'] + f['bytes'] > t['size']:
            raise ValueError('compiled field outside its declared structure')
        return self.word(pointer + f['offset'], f['bytes'])

    def fields(self, pointer, typename, *names):
        return {name: self.field(pointer, typename, name) for name in names}

    def cap(self, value, maximum, name, allow_zero=False):
        if not (0 if allow_zero else 1) <= value <= maximum:
            raise ValueError('invalid bounded capacity: ' + name)
        return value

    def quiet(self, pointer, typename, *names):
        values = self.fields(pointer, typename, *names)
        if any(values.values()):
            raise ValueError('checkpoint is not quiet: ' + typename + ' ' + str(values))

    def backing(self, label, pointer, count, width, context):
        if not count:
            return
        self.cap(count, 32 << 20, label)
        self.cap(width, 1 << 20, label + ' element')
        size = count * width
        if size > 32 << 20:
            raise ValueError('single fixed range exceeds32MiB')
        self.mapped(pointer, size)
        for row in self.records:
            lo, hi = int(row['pointer'], 16), int(row['pointer'], 16) + row['requested_bytes']
            if lo == pointer and hi == pointer + size:
                row.setdefault('aliases', []).append(label)
                return
            if pointer < hi and lo < pointer + size:
                raise ValueError('fixed backing ranges overlap: ' + label)
        if len(self.records) >= MAX_RANGES:
            raise ValueError('fixed owner range count exceeds bound')
        self.records.append(dict(pid=self.pid, label=label, pointer=hex(pointer),
                                 requested_bytes=size, context=hex(context)))

    def snapshot(self, global_address):
        group = self.word(global_address, 8)
        g = self.fields(group, 'KuHttpLoopGroup', 'count', 'created', 'started', 'draining', 'tasks')
        loops = self.cap(g['count'], 32, 'loop count')
        if g['created'] != loops or g['started'] != 1 or g['draining']:
            raise ValueError('group not fully started/idle')
        self.metadata.append(dict(kind='stack group header', pointer=hex(group), **g))
        runtime = g['tasks']
        capacity = self.cap(self.field(runtime, 'KuHttpTaskRuntime', 'capacity'), 16384, 'runtime capacity')
        self.quiet(runtime, 'KuHttpTaskRuntime', 'active_handles')
        if self.field(runtime, 'KuHttpTaskRuntime', 'free_handle_count') != capacity:
            raise ValueError('Task handles not fully returned')
        self.backing('task-runtime-header', runtime, 1, self.layout['types']['KuHttpTaskRuntime']['size'], runtime)
        driver = runtime + self.layout['types']['KuHttpTaskRuntime']['fields']['driver']['offset']
        dcap = self.cap(self.field(driver, 'KuTaskDriverV1', 'capacity'), 16384, 'driver capacity')
        self.quiet(driver, 'KuTaskDriverV1', 'resident', 'queued', 'running', 'building', 'closing')
        elements = self.layout['element_bytes']
        for field, count, element in [('slots', dcap, 'driver_slot'), ('ring', dcap, 'ring'),
                                       ('handles', capacity, 'task_handle'), ('free_handles', capacity, 'free_handle')]:
            self.backing('task-' + field, self.field(runtime, 'KuHttpTaskRuntime', field), count, elements[element], runtime)
        binding_cells = self.cap(self.field(driver, 'KuTaskDriverV1', 'binding_cells'), 1 << 20, 'binding cells')
        self.backing('task-binding-keys', self.field(driver, 'KuTaskDriverV1', 'binding_keys'), binding_cells,
                     elements['binding_key'], driver)
        pool = self.field(runtime, 'KuHttpTaskRuntime', 'blocking')
        if pool:
            self.quiet(pool, 'KuTaskBlockingPoolV1', 'resident', 'queued', 'running', 'net_wait_count', 'net_step_count')
            pcap = self.cap(self.field(pool, 'KuTaskBlockingPoolV1', 'capacity'), 16384, 'blocking capacity')
            storage = self.field(pool, 'KuTaskBlockingPoolV1', 'storage')
            storage_bytes = self.cap(self.field(pool, 'KuTaskBlockingPoolV1', 'storage_bytes'), 32 << 20, 'blocking storage bytes')
            self.backing('blocking-pool-header', pool, 1, self.layout['types']['KuTaskBlockingPoolV1']['size'], pool)
            self.backing('blocking-storage', storage, storage_bytes, 1, pool)
            nested = {}
            for field, element in [('jobs', 'blocking_job'), ('inputs', 'blocking_input'), ('ring', 'ring'), ('free_indices', 'free_handle')]:
                pointer = self.field(pool, 'KuTaskBlockingPoolV1', field)
                if not storage <= pointer < pointer + pcap * elements[element] <= storage + storage_bytes:
                    raise ValueError('blocking subrange not inside single owned storage')
                nested[field] = dict(pointer=hex(pointer), bytes=pcap * elements[element])
            self.metadata.append(dict(kind='blocking storage subranges; not extra allocations', ranges=nested))
        else:
            self.missing.append('optional blocking pool is null')
        offset = self.layout['types']['KuHttpLoopGroup']['fields']['loops']['offset']
        owner_size = self.layout['types']['KuHttpLoopOwner']['size']
        for index in range(loops):
            owner = group + offset + index * owner_size
            values = self.fields(owner, 'KuHttpLoopOwner', 'index', 'slots', 'ready', 'exited', 'connection_base', 'handler_queue', 'connections', 'pending_count')
            if values['index'] != index or values['ready'] != 1 or values['exited']:
                raise ValueError('loop owner identity/readiness mismatch')
            self.quiet(owner, 'KuHttpLoopOwner', 'queued', 'running', 'resident', 'active_requests')
            slots = self.cap(values['slots'], 16384, 'HTTP owner slots')
            connections = values['connection_base']
            self.backing('http-connections', connections, slots, elements['connection'], owner)
            queue = values['handler_queue']
            if queue:
                self.quiet(queue, 'KuHttpQueue', 'count', 'resident', 'running')
                cap = self.cap(self.field(queue, 'KuHttpQueue', 'cap'), 16384, 'HTTP handler queue')
                self.backing('http-queue-items', self.field(queue, 'KuHttpQueue', 'items'), cap, elements['pointer'], owner)
            else:
                self.missing.append('owner%d queue pointer unavailable' % index)
            # Read only two known pointer fields of connection0, never request
            # IDs, payloads, SQL, connection strings or arbitrary heap bytes.
            self.quiet(connections, 'KuHttpConnection', 'request_lease', 'request_queued', 'buffered', 'carry_length', 'job', 'async_task')
            phase = self.field(connections, 'KuHttpConnection', 'phase')
            if phase not in (0, 1):
                raise ValueError('connection0 is neither FREE nor idle HEADER')
            header_capacity = self.field(connections, 'KuHttpConnection', 'capacity')
            header_pointer = self.field(connections, 'KuHttpConnection', 'header')
            if header_capacity:
                self.backing('idle-header-capacity', header_pointer, header_capacity, 1, owner)
            self.metadata.append(dict(kind='idle connection0 metadata only; no payload read', owner=index,
                phase=phase, connections=values['connections'], cached_pending=values['pending_count'],
                retained_header_capacity=header_capacity, retained_header_pointer=hex(header_pointer)))
            active = self.field(connections, 'KuHttpConnection', 'active_set')
            reactor = self.field(connections, 'KuHttpConnection', 'reactor')
            if not active or not reactor:
                self.missing.append('owner%d active/reactor pointers unavailable in connection0' % index)
                continue
            self.quiet(active, 'KuHttpActiveSet', 'request_active')
            acap = self.field(active, 'KuHttpActiveSet', 'capacity')
            if acap != slots or self.field(active, 'KuHttpActiveSet', 'connections') != connections:
                raise ValueError('active set does not match its connection allocation')
            pending = self.field(active, 'KuHttpActiveSet', 'pending')
            if not pending:
                raise ValueError('live pending pointer unavailable; quiet cannot be established')
            self.quiet(pending, 'KuHttpPending', 'count', 'raw_count')
            pcap = self.cap(self.field(pending, 'KuHttpPending', 'cap'), 16384, 'HTTP pending capacity', allow_zero=True)
            self.backing('http-pending-items', self.field(pending, 'KuHttpPending', 'items'), pcap, elements['pending_item'], owner)
            self.backing('http-active-indices', self.field(active, 'KuHttpActiveSet', 'indices'), slots, elements['int'], owner)
            if self.field(reactor, 'KuHttpReactor', 'active') != active or self.field(reactor, 'KuHttpReactor', 'slots') != slots:
                raise ValueError('reactor owner/slots mismatch')
            for field, count_field, element in [('registrations', 'capacity', 'registration'),
                                                 ('hash', 'hash_capacity', 'token_bucket'),
                                                 ('fd_entries', 'fd_capacity', 'fd_bucket')]:
                count = self.cap(self.field(reactor, 'KuHttpReactor', count_field), 1 << 20, 'reactor ' + count_field)
                self.backing('reactor-' + field, self.field(reactor, 'KuHttpReactor', field), count, elements[element], owner)
            for field in ('work', 'idle_heap'):
                self.backing('reactor-' + field, self.field(reactor, 'KuHttpReactor', field), slots, elements['int'], owner)
            self.metadata.append(dict(kind='stack queue/active/reactor headers, not heap allocations',
                                      owner=index, queue=hex(queue), active=hex(active), reactor=hex(reactor)))
        return dict(complete=True, records=self.records, metadata=self.metadata, missing=self.missing,
                    coverage_complete=False, scope='Fixed reachable backing plus explicitly labelled idle header capacity only; loop-local net/poll arrays and third-party heap remain unknown.')


def page_ranges(owners, page):
    unique, boundary = set(), set()
    rows = []
    for row in owners['records']:
        lo = int(row['pointer'], 16); hi = lo + row['requested_bytes']
        pages = set(range(lo // page, (hi - 1) // page + 1))
        interior = {p for p in pages if lo <= p * page and (p + 1) * page <= hi}
        boundary.update(pages - interior); unique.update(pages)
        if len(unique) > MAX_PAGES:
            raise ValueError('fixed-owner union exceeds8192 pages')
        rows.append(dict(label=row['label'], context=row['context'],
                         first_page=hex(min(pages) * page), last_page=hex(max(pages) * page),
                         page_count=len(pages), full_interior_pages=len(interior)))
    return dict(page_size=page, unique_pages=len(unique), unique_boundary_pages=len(boundary),
                records=rows, resident_bytes_inferred=False,
                scope='Virtual address-page correspondence only; pages shared by adjacent owners counted once in union. PFN/ANON/ZERO/exclusive checks are separate.')


def capture_owned(process, prepared_path, destination, boundary):
    """Call only from the harness that owns this direct Popen service child."""
    if not isinstance(process, subprocess.Popen) or process.poll() is not None:
        raise ValueError('live owned Popen child required')
    if any(boundary.get(k) is not True for k in REQUIRED_BOUNDARY):
        raise ValueError('producers/management must be stopped at an independent idle checkpoint')
    prepared = bounded_json(prepared_path)
    if prepared['schema'] != 'ku-normal-fixed-owner-prepared-v1':
        raise ValueError('unsupported preparation receipt')
    for kind in ('ELF', 'source', 'compiler', 'libpq'):
        if digest(prepared[kind]) != prepared['identities'][kind]:
            raise ValueError('prepared input changed: ' + kind)
    if digest(prepared['normal_manifest']) != prepared['normal_manifest_SHA256']:
        raise ValueError('original normal manifest changed')
    if digest(prepared['layout']) != prepared['layout_SHA256'] or digest(TEMPLATE) != prepared['template_SHA256']:
        raise ValueError('compiled layout/template identity changed')
    layout = bounded_json(prepared['layout'])
    proc = Path('/proc') / str(process.pid)
    start = read_start(proc)
    target = (proc / 'exe').stat()
    executable_key = (target.st_ino, target.st_dev)
    if digest(proc / 'exe') != prepared['identities']['ELF']:
        raise ValueError('actual process executable bytes differ from prepared ELF')
    status = check_identity(proc, prepared, start, executable_key)
    if next(x.split()[1] for x in status.splitlines() if x.startswith('State:')) in ('T', 't'):
        raise ValueError('refuse to own/resume a pre-stopped process')
    destination = Path(destination)
    destination.mkdir(exist_ok=False)
    # Permission failures occur before STOP. No privilege or ptrace policy change.
    mem = os.open(proc / 'mem', os.O_RDONLY)
    began = time.monotonic_ns(); held = False; confirmed = None; resumed = None; pidfd = None
    try:
        if not hasattr(os, 'pidfd_open') or not hasattr(signal, 'pidfd_send_signal'):
            raise ValueError('pidfd signalling unavailable; no target was stopped')
        pidfd = os.pidfd_open(process.pid)
        check_identity(proc, prepared, start, executable_key)
        held = True
        signal.pidfd_send_signal(pidfd, signal.SIGSTOP)
        deadline = time.monotonic() + 2
        while not (threads := rs.stopped_threads(proc)):
            if time.monotonic() > deadline:
                raise TimeoutError('normal checkpoint threads did not stop within2s')
            time.sleep(.01)
        confirmed = time.monotonic_ns()
        check_identity(proc, prepared, start, executable_key)
        mappings_raw = rs.read_text(proc / 'maps', 1 << 20)
        mappings = rs.maps_parse(mappings_raw)
        executable = (proc / 'exe').stat()
        device = '%02x:%02x' % (os.major(executable.st_dev), os.minor(executable.st_dev))
        bias = load_bias(mappings, prepared['ELF'], executable.st_ino, device,
                         prepared['global_symbol_vaddr'], os.sysconf('SC_PAGE_SIZE'))
        reader = OwnerReader(mem, mappings, layout, process.pid, began / 1e9 + 8)
        owners = reader.snapshot(bias + prepared['global_symbol_vaddr'])
        address_pages = page_ranges(owners, os.sysconf('SC_PAGE_SIZE'))
        smaps_raw = rs.read_text(proc / 'smaps', 4 << 20)
        pages = hcd.allocation_owner_pages(process.pid, owners, start, maximum_records=16384)
        if pages.get('available'):
            pages['exclusive_ordinary_anon_full_interior_bytes_total'] = sum(
                r['exclusive_ordinary_anon_interior_bytes'] for r in pages['records'])
            pages['aggregate_scope'] = 'Disjoint validated byte ranges, each full interior page occurs once; boundary pages are excluded. Not total RSS/PSS or cgroup charge.'
        reader.budget()
        if rs.stopped_threads(proc) != threads:
            raise ValueError('thread set changed during checkpoint')
        check_identity(proc, prepared, start, executable_key)
    finally:
        try:
            if held:
                try:
                    signal.pidfd_send_signal(pidfd, signal.SIGCONT)
                except ProcessLookupError:
                    pass
                resumed = time.monotonic_ns()
        finally:
            if pidfd is not None:
                os.close(pidfd)
            os.close(mem)
    value = dict(schema='ku-normal-fixed-owner-checkpoint-v1', pid=process.pid, start_ticks=start,
                 prepared_SHA256=digest(prepared_path), normal_ELF_SHA256=prepared['identities']['ELF'],
                 normal_C_SHA256=prepared['identities']['source'], source_commit=prepared['product_source_commit'],
                 executable_inode=executable_key[0], executable_device=executable_key[1],
                 load_bias=hex(bias), global_pointer_address=hex(bias + prepared['global_symbol_vaddr']),
                 owners=owners, virtual_page_correspondence=address_pages, page_metadata=pages,
                 idle_only=True, score_eligible=False, coverage_complete=False,
                 stop_requested_monotonic_ns=began, all_threads_stopped_monotonic_ns=confirmed,
                 CONT_returned_monotonic_ns=resumed, total_STOP_span_ns=resumed - began,
                 scope='One unchanged ordinary ELF. No allocation interposer. Requested ranges, VMA Anonymous/RSS/PSS and cgroup charge are distinct; no additive whole-memory claim.')
    (destination / 'maps.log').write_text(mappings_raw)
    (destination / 'smaps.log').write_text(smaps_raw)
    (destination / 'owners.json').write_text(json.dumps(value, indent=2) + '\n')
    return value


def main():
    p = argparse.ArgumentParser(description=__doc__)
    for key in ('elf', 'source', 'manifest', 'compiler-receipt', 'compiler', 'libpq', 'source-commit', 'output'):
        p.add_argument('--' + key, required=True)
    args = p.parse_args()
    result = prepare(args.elf, args.source, args.manifest, args.compiler_receipt, args.compiler,
                     args.libpq, args.source_commit, args.output)
    print(json.dumps(result, separators=(',', ':')))


if __name__ == '__main__':
    main()
