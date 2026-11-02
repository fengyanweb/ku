#!/usr/bin/env python3
"""Read-only, byte-exact evidence export. Never extract or execute source members."""
import argparse
import gzip
import hashlib
import io
import json
import os
from pathlib import Path, PurePosixPath
import re
import statistics
import tarfile
import tempfile
import urllib.request

REPOSITORY = 'fengyanweb/ku'
ARTIFACT = 'ku-performance-v2-evidence'
MAX_ARCHIVE, MAX_EXPANDED, MAX_FILE, MAX_FILES = 384 << 20, 512 << 20, 128 << 20, 20000
MAP = 'portable-file-map.json'
IDENTITIES = {'frozen-plan.json', 'build-identities.json', 'dependency-identities.json',
              'target-abi.json', 'hosted-state.json'}
OBSERVATIONS = {'window.json', 'full-lifecycle.json', 'client.json', 'client.log',
                'driver-cleanup.json', 'identity.json', 'progress.json', 'termination.json',
                'service.json', 'service.log', 'runtime-metrics.json', 'runtime-before.json',
                'runtime-after.json', 'runtime-after-failure.json', 'summary.json',
                'results.json', 'environment-qualification.json'}
SELF = {'perf.data', 'perf-pcs.log', 'perf-self.log', 'maps.log', 'symbol-identities.json',
        'normal-self-observations.json', 'perf-record.log', 'processes-before.log',
        'before-smaps.log', 'after-smaps.log'}


def encoded(value):
    return (json.dumps(value, indent=2, sort_keys=True) + '\n').encode()


def digest(data):
    return hashlib.sha256(data).hexdigest()


def run_id(value):
    if not re.fullmatch(r'[1-9][0-9]{0,19}', value):
        raise ValueError('source_run must be a positive decimal Actions run ID')
    return value


def safe_path(name):
    path = PurePosixPath(name)
    if (not path.parts or path.is_absolute() or '..' in path.parts or '\\' in name or ':' in name
            or str(path) != name or any(ord(c) < 32 for c in name)):
        raise ValueError('noncanonical archive path: ' + repr(name))
    return path


def selected(name, include_self):
    path = PurePosixPath(name)
    if name in IDENTITIES:
        return True
    # Explicit observation names exclude requests/events CSV and private fixture/config trees.
    if path.parts[0] == 'http-short-screen' or (include_self and path.parts[0] == 'normal-self-probe'):
        return (path.name in OBSERVATIONS or re.fullmatch(r'resources(?:\.[a-z0-9_-]+)+', path.name)
                or (include_self and path.parts[0] == 'normal-self-probe' and (path.name in SELF
                    or (path.parent.name == 'pmu-stat' and path.name in ('stat.csv', 'stat.log'))
                    or ('symfs' in path.parts and re.fullmatch(
                        r'libpq\.so(?:\.[0-9.]+)?|lib[cm]\.so\.6|ld-linux-aarch64\.so\.1', path.name)))))
    if path.parts[0] in ('A', 'B'):
        return (name == path.parts[0] + '/manifest.json'
                or (path.parts[1:3] == ('build', 'logs') and (path.name in ('build.json', 'compiler.json')
                    or re.fullmatch(r'(?:ku-(?:cli|elf-(?:dynamic|header)-(?:batch|server)|native-(?:batch|server))|version-(?:cargo|cc|ku|rustc))\.log', path.name)))
                or name.endswith('/build/bin/ku-server')
                or (path.parts[1:3] == ('build', 'ku-native') and path.suffix in ('.c', '.ku')))
    if include_self and path.parts[0] == 'R':
        return name == 'R/manifest.json' or name in ('R/build/bin/ku-cross-stack-rust',
            'R/build/rust-cargo/release/ku-cross-stack-rust', 'R/build/target/release/ku-cross-stack-rust')
    return False


def fetch_metadata(source_run, output):
    source_run = run_id(source_run)
    if os.environ.get('GITHUB_REPOSITORY') != REPOSITORY:
        raise ValueError('evidence slicing is restricted to ' + REPOSITORY)
    def get(suffix):
        request = urllib.request.Request('https://api.github.com/repos/' + REPOSITORY + suffix,
            headers={'Authorization': 'Bearer ' + os.environ['GH_TOKEN'],
                     'Accept': 'application/vnd.github+json', 'X-GitHub-Api-Version': '2022-11-28'})
        with urllib.request.urlopen(request, timeout=30) as response:
            raw = response.read((4 << 20) + 1)
        if len(raw) > 4 << 20:
            raise ValueError('Actions metadata exceeds bound')
        return json.loads(raw)
    run = get('/actions/runs/' + source_run)
    listing = get('/actions/runs/' + source_run + '/artifacts?per_page=100')
    matches = [a for a in listing['artifacts'] if a['name'] == ARTIFACT]
    if (run['id'] != int(source_run) or run['repository']['full_name'] != REPOSITORY
            or run['status'] != 'completed' or listing['total_count'] > 100 or len(matches) != 1):
        raise ValueError('requires one artifact from the completed same-repository source run')
    artifact = matches[0]
    if (artifact['expired'] or artifact['workflow_run']['id'] != int(source_run)
            or artifact['workflow_run']['head_sha'] != run['head_sha']
            or not 0 < artifact['size_in_bytes'] <= MAX_ARCHIVE + (1 << 20)):
        raise ValueError('source artifact is expired, oversized or has different run/head identity')
    metadata = dict(repository=REPOSITORY, source_run=source_run, head_sha=run['head_sha'],
        run={k: run[k] for k in ('id', 'head_sha', 'head_branch', 'actor', 'triggering_actor', 'event', 'status', 'conclusion',
             'html_url', 'created_at', 'updated_at')}, artifact=artifact,
        retrieval_run=os.environ.get('GITHUB_RUN_ID'), retrieval_head=os.environ.get('GITHUB_SHA'))
    with output.open('xb') as stream:
        stream.write(encoded(metadata))
    with open(os.environ['GITHUB_OUTPUT'], 'a') as stream:
        stream.write(f'run-id={source_run}\nartifact-id={int(artifact["id"])}\n')


class BoundedReader:
    def __init__(self, source):
        self.source, self.total = source, 0

    def read(self, count=-1):
        count = min(count if count >= 0 else 1 << 20, MAX_EXPANDED - self.total + 1)
        data = self.source.read(count)
        self.total += len(data)
        if self.total > MAX_EXPANDED:
            raise ValueError('archive exceeds 512MiB expanded bound')
        return data


def add(tar, name, data):
    info = tarfile.TarInfo(name)
    info.size, info.mode, info.mtime = len(data), 0o644, 0
    tar.addfile(info, io.BytesIO(data))


def slice_archive(archive, receipt_path, metadata_path, output, include_self=False):
    if archive.is_symlink() or not archive.is_file() or not 0 < archive.stat().st_size <= MAX_ARCHIVE:
        raise ValueError('requires one regular archive of at most 384MiB')
    if any(p.is_symlink() or not p.is_file() or p.stat().st_size > 4 << 20 for p in (receipt_path, metadata_path)):
        raise ValueError('requires regular metadata/receipt files of at most 4MiB')
    receipt_raw, metadata_raw = receipt_path.read_bytes(), metadata_path.read_bytes()
    receipt, metadata = json.loads(receipt_raw), json.loads(metadata_raw)
    if (metadata.get('repository') != REPOSITORY or run_id(metadata['source_run']) != str(metadata['run']['id'])
            or metadata['head_sha'] != metadata['run']['head_sha']
            or metadata['artifact']['name'] != ARTIFACT
            or metadata['artifact']['workflow_run']['id'] != int(metadata['source_run'])
            or metadata['artifact']['workflow_run']['head_sha'] != metadata['head_sha']):
        raise ValueError('source metadata identity differs')
    with archive.open('rb') as stream:
        checksum = hashlib.sha256()
        for chunk in iter(lambda: stream.read(1 << 20), b''):
            checksum.update(chunk)
        whole_sha = checksum.hexdigest()
    if (receipt.get('complete') is not True or receipt.get('sha256') != whole_sha
            or receipt.get('bytes') != archive.stat().st_size):
        raise ValueError('archive receipt bytes/SHA/completion differs')
    if output.exists() or output.is_symlink():
        raise ValueError('output already exists; evidence is never overwritten')
    actual, exports, map_raw = {}, [], None
    with tempfile.NamedTemporaryFile(dir=output.parent, prefix='.evidence-slice-', suffix='.tgz') as temporary:
        with tarfile.open(fileobj=temporary, mode='w:gz', compresslevel=6) as target:
            with gzip.open(archive, 'rb') as compressed:
                bounded = BoundedReader(compressed)
                # One tar block per input read prevents buffering unexamined trailing members.
                with tarfile.open(fileobj=bounded, mode='r|', bufsize=512) as source:
                    for member in source:
                        safe_path(member.name)
                        if (not member.isfile() or member.issparse() or member.name in actual or member.name == MAP and map_raw is not None
                                or member.size < 0 or member.size > MAX_FILE or len(actual) >= MAX_FILES):
                            raise ValueError('unsupported/duplicate/oversized member: ' + member.name)
                        if member.name == MAP and member.size > 8 << 20:
                            raise ValueError('portable map exceeds bound')
                        data = source.extractfile(member).read()
                        if len(data) != member.size:
                            raise ValueError('truncated archive member')
                        if member.name == MAP:
                            map_raw = data
                            continue
                        actual[member.name] = dict(bytes=len(data), archived_sha256=digest(data))
                        if selected(member.name, include_self):
                            if ('symfs' in PurePosixPath(member.name).parts or member.name.endswith('/ku-server')
                                    or member.name.endswith('/ku-cross-stack-rust')) and data[:4] != b'\x7fELF':
                                raise ValueError('selected service/DSO is not ELF: ' + member.name)
                            add(target, member.name, data)
                            exports.append(dict(path=member.name, bytes=len(data), exported_sha256=digest(data)))
                while True:
                    trailing = bounded.read(1 << 20)
                    if not trailing:
                        break  # Also validates gzip footer and the total expanded bound.
                    if trailing.strip(b'\0'):
                        raise ValueError('nonzero data after tar terminator')
            index = json.loads(map_raw) if map_raw is not None else {}
            if index.get('path_root') != '${EVIDENCE_ROOT}' or index.get('raw_and_redacted_hashes_distinct') is not True:
                raise ValueError('portable map provenance absent')
            entries = index['files']
            if len(entries) != len(actual) or len(entries) != receipt.get('file_count'):
                raise ValueError('portable map/receipt member count differs')
            seen = set()
            for entry in entries:
                name = entry['path']; safe_path(name)
                if (name in seen or name not in actual or type(entry.get('bytes')) is not int
                        or any(entry.get(k) != actual[name][k] for k in ('bytes', 'archived_sha256'))
                        or not re.fullmatch('[0-9a-f]{64}', entry.get('original_sha256', ''))):
                    raise ValueError('portable map bytes/SHA/path differs: ' + name)
                seen.add(name)
            if not any(e['path'].startswith('http-short-screen/') for e in exports) and not (
                    include_self and any(e['path'].startswith('normal-self-probe/') for e in exports)):
                raise ValueError('requested observation tree absent')
            manifest = dict(schema='ku-readonly-evidence-slice-v1', source_run=metadata['source_run'],
                source_head=metadata['head_sha'], source_archive_sha256=whole_sha,
                source_archive_bytes=archive.stat().st_size, verified_files=len(actual),
                original_map_sha256=digest(map_raw), include_normal_self_probe=include_self, files=exports,
                scope='Existing archived bytes only; no build, service, database, perf or new measurement executed')
            for name, data in [(MAP, map_raw), ('slice-source.json', metadata_raw),
                               ('source-archive.json', receipt_raw), ('slice-manifest.json', encoded(manifest))]:
                add(target, name, data)
        os.link(temporary.name, output)  # Exclusive publication after complete verification.
    manifest.update(exported_files=len(exports), output_bytes=output.stat().st_size)
    return {k: v for k, v in manifest.items() if k != 'files'}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('mode', choices=('metadata', 'slice', 'recompute'))
    parser.add_argument('--source-run')
    parser.add_argument('--archive', type=Path)
    parser.add_argument('--receipt', type=Path)
    parser.add_argument('--metadata', type=Path)
    parser.add_argument('--output', type=Path, required=True)
    parser.add_argument('--include-normal-self-probe', action='store_true')
    args = parser.parse_args()
    if args.mode == 'metadata':
        fetch_metadata(args.source_run or '', args.output)
    elif args.mode == 'recompute':
        print(json.dumps(recompute_slice(args.archive, args.output), sort_keys=True))
    else:
        print(json.dumps(slice_archive(args.archive, args.receipt, args.metadata, args.output,
                                       args.include_normal_self_probe), sort_keys=True))


def recompute_slice(archive, output):
    """Replay original samples. No executable, service or workload is run."""
    if archive.is_symlink() or not archive.is_file() or not 0 < archive.stat().st_size <= MAX_ARCHIVE:
        raise ValueError('requires a bounded regular slice')
    if output.exists() or output.is_symlink():
        raise ValueError('recompute output already exists')
    inputs = {}
    with tarfile.open(archive, 'r:gz') as source:
        members = source.getmembers()
        if len(members) > MAX_FILES or sum(m.size for m in members) > MAX_EXPANDED:
            raise ValueError('slice exceeds bound')
        for member in members:
            safe_path(member.name)
            if not member.isfile() or member.size > MAX_FILE or member.name in inputs:
                raise ValueError('unsupported slice member')
            if member.name == 'slice-manifest.json' or (member.name.startswith('http-short-screen/')
                    and member.name.endswith(('/score/resources.json', '/score/client.json', '/full-lifecycle.json', '/window.json'))):
                raw = source.extractfile(member).read()
                if len(raw) != member.size:
                    raise ValueError('truncated slice input')
                inputs[member.name] = (json.loads(raw), digest(raw), len(raw))
    manifest = inputs['slice-manifest.json'][0]
    expected = {f['path']: f for f in manifest['files']}
    for name, (_, checksum, size) in inputs.items():
        if name != 'slice-manifest.json' and (expected[name]['exported_sha256'] != checksum or expected[name]['bytes'] != size):
            raise ValueError('recompute input identity differs: ' + name)
    rows = []
    for name in sorted(inputs):
        if not name.endswith('/score/resources.json'):
            continue
        prefix = name.removesuffix('/score/resources.json')
        needed = [name, prefix+'/score/client.json', prefix+'/full-lifecycle.json', prefix+'/window.json']
        samples, client, full, window = [inputs[k][0] for k in needed]
        cost, correct = client['service_cost'], client['ledger']['correct_total']
        clock = cost['cross_process_clock']
        if not clock['available'] or samples['error'] or samples['overflow'] or correct <= 0:
            raise ValueError('unavailable original metric inputs: ' + prefix)
        start = clock['origin_ns']
        stop = start + int(client['configuration']['duration_seconds']*1e9)
        selected = [d for sample in samples['samples'] for d in sample['domains']
                    if d['label'] == cost['before']['label'] and start <= d['monotonic_ns'] <= stop]
        if len(selected) < 5 or any(a['monotonic_ns'] > b['monotonic_ns'] for a, b in zip(selected, selected[1:])):
            raise ValueError('insufficient or unordered original memory samples')
        lo, hi = len(selected)//5, len(selected)*4//5
        middle = selected[lo:hi]
        memory = statistics.median(d['memory_current'] for d in middle)
        begin, end = cost['before'], full['accounting_end']
        cpu_delta = end['cpu_stat']['usage_usec'] - begin['cpu_stat']['usage_usec']
        cpu = cpu_delta/correct
        if (cpu_delta < 0 or end['monotonic_ns'] < begin['monotonic_ns'] or correct != window['correct']
                or abs(cpu-window['full_cpu_us']) > 1e-9
                or abs(cpu-full['cpu_seconds_per_correct']*1e6) > 1e-9
                or memory != window['memory']['steady_median_bytes']
                or memory != cost['arrival_window_memory']['steady_median_bytes']):
            raise ValueError('original metric replay differs: ' + prefix)
        rows.append(dict(window=prefix, correct=correct, full_cpu_us=cpu,
            cpu_begin_monotonic_ns=begin['monotonic_ns'], cpu_end_monotonic_ns=end['monotonic_ns'],
            cpu_begin_usage_usec=begin['cpu_stat']['usage_usec'], cpu_end_usage_usec=end['cpu_stat']['usage_usec'],
            arrival_start_ns=start, arrival_end_ns=stop, memory_sample_count=len(selected),
            middle_indices=[lo, hi], middle_memory_values=[d['memory_current'] for d in middle],
            middle_monotonic_ns=[d['monotonic_ns'] for d in middle], steady_median_bytes=memory,
            inputs={k: dict(sha256=inputs[k][1], bytes=inputs[k][2]) for k in needed}))
    if not rows:
        raise ValueError('no original short-screen resource windows')
    report = dict(schema='ku-existing-short-metric-replay-v1', source_run=manifest['source_run'],
        source_head=manifest['source_head'], source_archive_sha256=manifest['source_archive_sha256'],
        code_sha256=digest(Path(__file__).read_bytes()), source_slice_sha256=digest(archive.read_bytes()),
        no_new_measurement=True, window_count=len(rows), correct=sum(r['correct'] for r in rows),
        CPU_scope='Original service-domain snapshot before score-driver launch (after startup/warmup) through recorded accounting_end after drain/termination. Does not include cold startup or warmup.',
        memory_scope='Original1Hz service-cgroup memory.current within arrival interval, chronological sample slice[n//5:n*4//5], then median. No RSS or endpoint substitution.', rows=rows)
    with output.open('x') as stream:
        stream.write(encoded(report).decode())
    return {k: v for k, v in report.items() if k != 'rows'}


if __name__ == '__main__':
    main()
