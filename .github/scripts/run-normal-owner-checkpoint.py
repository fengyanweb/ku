#!/usr/bin/env python3
"""One bounded idle fixed-owner capture of the unchanged normal5b ELF.

Restore only the fixed old build artifact. Do not load the rejected LDAP
candidate, build a CLI/normal ELF, run a driver or measure a performance score.
"""
import argparse
import copy
import hashlib
import http.client
import importlib.util
import json
import os
from pathlib import Path
import platform
import shutil
import signal
import sys
import time

REPO = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(REPO / 'tests/benchmarks/cross_stack'))
import normal_owner_checkpoint as checkpoint
import database
import linux_harness as lh
import hosted_cycle as hosted

spec = importlib.util.spec_from_file_location('fixed_old_restore', Path(__file__).with_name('prepare-libpq-loader-memory.py'))
restorer = importlib.util.module_from_spec(spec); spec.loader.exec_module(restorer)
SOURCE = '5b6b41138cce4086c19cc7daa24e820c9b6513d8'
ELF_SHA = '234300bba9b6fce603a178701ac289a82108398d5f35de3c0a7ad585f082367b'
C_SHA = '830536089f72ce56118e4e6a1556bbe007f816cc16a3c2f96c0dcff010d14e77'
D_SHA = 'b59de287c314a78cb70e7e0bb31fb2be34b961ccc90beff405d8c48412ace2df'
C_REL = 'B/build/ku-native/.ku/build/release/c/CPkLjMo5cOr7gaP-eyr5YRGcrGvMEuNt83h2b7ezDEs/ku-server.c'
CC_REL = 'B/build/logs/1793514266618607938/native-compiler-server/compiler.json'


def save(path, value):
    Path(path).write_text(json.dumps(value, ensure_ascii=False, indent=2) + '\n')


def alarm(signum, frame):
    raise TimeoutError('independent normal-owner checkpoint finite execution budget')


def check_default_maps(raw):
    if len(raw) > 1 << 20:
        raise ValueError('default maps exceeded1MiB')
    maps = checkpoint.rs.maps_parse(raw.decode())
    paths = sorted({m['path'] for m in maps if m['path'].startswith('/') and Path(m['path']).name.startswith('libpq.so')})
    if len(paths) != 1 or checkpoint.digest(paths[0]) != D_SHA:
        raise ValueError('actual process did not map the unchanged default D library')
    return dict(actual_default_D_library=paths[0], sha256=D_SHA,
                scope='Actual mapped file hash, not merely a link flag or environment value')


def distinct_object(pairs):
    result = {}
    for key, value in pairs:
        if key in result:
            raise ValueError('duplicate key in JSON response')
        result[key] = value
    return result


def single_json(service, corpus, destination=None):
    body = Path(corpus).read_bytes()
    if checkpoint.digest(corpus) != '548a8397f835c17ee8b524054e1da90d00d9293965986c5e48be5692663f5e3a':
        raise ValueError('frozen ordinary JSON input changed')
    host, port = service.address.rsplit(':', 1)
    connection = http.client.HTTPConnection(host, int(port), timeout=3)
    try:
        connection.request('POST', '/json?rid=owner-checkpoint-1', body=body,
                           headers={'Content-Type': 'application/json', 'Connection': 'keep-alive'})
        response = connection.getresponse(); data = response.read((1 << 20) + 1)
        if destination is not None:
            destination=Path(destination)
            (destination / 'ordinary-JSON-response.log').write_bytes(data)
            save(destination / 'ordinary-JSON-response-readback.json', dict(status=response.status,
                bytes_read=len(data),body_SHA256=hashlib.sha256(data).hexdigest(),
                truncated_at_bound=len(data)>1<<20,will_close=response.will_close,requests=1))
        original = json.loads(body, object_pairs_hook=distinct_object)
        wanted = {'id': 'owner-checkpoint-1', 'counter': original['counter'] + 1, 'data': original['data']}
        received = json.loads(data, object_pairs_hook=distinct_object)
        canonical = lambda value: json.dumps(value, sort_keys=True, ensure_ascii=False, separators=(',', ':'))
        if response.status != 200 or len(data) > 1 << 20 or canonical(received) != canonical(wanted):
            raise ValueError('one ordinary JSON response failed correctness')
        if response.will_close or connection.sock is None:
            raise ValueError('the single ordinary response did not preserve its idle keepalive')
        return connection, dict(status=200, input_bytes=len(body), response_bytes=len(data),
            response_SHA256=hashlib.sha256(data).hexdigest(), raw_response_utf8=data.decode('utf-8'), correct=True, requests=1,
            keepalive_open=True, elapsed_not_a_performance_score=True)
    except BaseException:
        connection.close(); raise


def natural(cleanup, service=False):
    return (cleanup.get('forced') is False and cleanup.get('cgroup_removed') is True
            and cleanup.get('after', {}).get('process_count') == 0
            and (not service or cleanup.get('exit_code') == 0))


def run_restored(root):
    original = root / 'original'; evidence = root / 'evidence'
    base = json.loads((original / 'B/manifest.json').read_text().replace('${EVIDENCE_ROOT}', str(original)))
    if base['candidate_base'] != SOURCE or checkpoint.digest(original / C_REL) != C_SHA or checkpoint.digest(original / 'B/build/bin/ku-server') != ELF_SHA:
        raise ValueError('fixed ordinary ELF/C/product identity differs')
    proof = json.loads((REPO / '.github/fixtures/libpq-loader-memory/current-default-C-proof.json').read_text())
    if proof.get('byte_equal') is not True or proof['current']['sha256'] != C_SHA or proof['old_source_commit'] != SOURCE:
        raise ValueError('current complete-C equivalence proof absent')
    # Build only the isolated offsetof helper, using original C and compiler.
    prepared = checkpoint.prepare(original / 'B/build/bin/ku-server', original / C_REL,
        original / 'B/manifest.json', original / CC_REL, '/usr/bin/clang-18',
        original / 'B/build/libpq/libpq.so', SOURCE, root / 'layout')
    save(evidence / 'current-default-C-proof.json', proof)
    fixture_root, service_root = root / 'fixture', root / 'service-input'
    fixture_root.mkdir(); service_root.mkdir()
    base['cgroup_root'] = '/sys/fs/cgroup/ku-idle-owner-' + hashlib.sha256(str(root).encode()).hexdigest()[:12]
    if Path(base['cgroup_root']).exists():
        raise ValueError('fresh checkpoint cgroup path already exists; no domain reused')
    base['database_cpu_set'] = '3'
    base['stacks'] = {'ku': base['stacks']['ku']}
    if base['stacks']['ku'].get('env'):
        raise ValueError('original ordinary service unexpectedly has a runtime library override')
    base['stacks']['ku']['build']['libpq_source'] = str(original / 'B/build/libpq/libpq.so')
    fixture_manifest = copy.deepcopy(base); fixture_manifest['stacks'] = {}
    save(fixture_root / 'manifest.json', fixture_manifest)
    fixture, service, keepalive, harness = None, None, None, None
    result = dict(schema='ku-one-normal-owner-checkpoint-v1', diagnostic_only=True,
        score_eligible=False, performance_windows=0, automatic_retry=False,
        original_product=SOURCE, normal_ELF_SHA256=ELF_SHA, normal_C_SHA256=C_SHA,
        default_libpq_SHA256=D_SHA, status='unknown', capture_attempts=0)
    began = time.monotonic()
    try:
        fixture = database.Database(fixture_root, base['uid'], base['gid'])
        result['database_start'] = fixture.start()
        result['database_child_provenance'] = dict(source='database.Database.start -> Harness.launch direct Popen',
            pid=result['database_start'].get('pid'), cgroup=result['database_start'].get('cgroup'),
            validation='Database.start validates the owned marker, executable, data directory and cgroup before returning; no waitpid observation is implied.')
        private = json.loads(fixture.manifest_path.read_text())
        base.update(db_url=private['db_url'], db_cgroup=private['db_cgroup'])
        save(service_root / 'manifest.json', base)
        harness = lh.Harness(service_root, root / 'service')
        service = harness.start('ku', cores=2, memory=lh.GIB, output=root / 'service/one')
        maps = checkpoint.rs.read_text(Path('/proc') / str(service.process.pid) / 'maps', 1 << 20).encode()
        result['runtime_default'] = check_default_maps(maps)
        (evidence / 'runtime-default.maps').write_bytes(maps)
        keepalive, result['single_correct_response'] = single_json(service,
            original / 'Driver/src/tests/benchmarks/cross_stack/corpora/object.json', evidence)
        result['single_correct_response'].pop('raw_response_utf8',None)
        # Exactly one management read, then no further producer or management
        # request until after the independent stopped checkpoint has resumed.
        save(evidence / 'pre-capture-metrics.json', hosted.redact(service.metrics(), root))
        time.sleep(.1)
        result['capture_attempts'] = 1
        try:
            captured = checkpoint.capture_owned(service.process, root / 'layout/prepared.json',
                evidence / 'checkpoint', dict.fromkeys(checkpoint.REQUIRED_BOUNDARY, True))
            result['status'] = 'captured'
            result['owner_ranges'] = len(captured['owners']['records'])
            result['page_metadata_available'] = captured['page_metadata'].get('available') is True
            if not result['page_metadata_available']:
                result['page_classification'] = 'unknown: ' + str(captured['page_metadata'].get('unavailable_reason'))
        except (OSError, ValueError, RuntimeError, TimeoutError) as error:
            result['capture_error'] = repr(error)[:1024]
            result['status'] = 'unknown'
    except (OSError, ValueError, RuntimeError, TimeoutError) as error:
        result['operation_error'] = repr(error)[:1024]
    finally:
        signal.alarm(30)
        try:
            if keepalive is not None:
                keepalive.close()
            if service is not None:
                try:
                    time.sleep(.1)
                    final_metrics=service.metrics()
                    save(evidence / 'post-close-metrics.json', hosted.redact(final_metrics, root))
                    result['post_close_quiet'] = lh.quiet_metrics('ku', final_metrics)
                except Exception as error:
                    result['post_close_quiet'] = False
                    result['post_close_metrics_error'] = repr(error)[:512]
            if service is not None:
                try: result['service_cleanup'] = service.stop()
                except Exception as error: result['service_cleanup'] = {'error': repr(error)[:1024]}
            if fixture is not None:
                try: result['database_cleanup'] = fixture.stop()
                except Exception as error: result['database_cleanup'] = {'error': repr(error)[:1024]}
            db_pid=result.get('database_start',{}).get('pid')
            if type(db_pid) is int:
                try:
                    reaped,status=os.waitpid(db_pid,os.WNOHANG)
                    result['owned_database_child_reaped'] = reaped==db_pid
                    result['database_waitpid_state'] = 'reaped_by_checkpoint' if reaped==db_pid else 'not_yet_reaped'
                    result['database_wait_status'] = status if reaped==db_pid else None
                except ChildProcessError:
                    result['owned_database_child_reaped'] = None
                    result['database_waitpid_state'] = 'already_reaped_or_not_child'
                    result['database_wait_status'] = None
                    result['database_reap_observation_boundary'] = ('ECHILD alone cannot distinguish prior reaping from not-a-child. '
                        'Only the recorded self-created fixture provenance plus zero processes and removed owned cgroup can establish no remaining fixture process; exact reap/exit status remains unknown.')
                except OSError as error:
                    result['owned_database_child_reaped'] = False
                    result['database_waitpid_state'] = 'waitpid_error'
                    result['database_reap_error'] = repr(error)[:256]
            owned_parent = Path(base['cgroup_root'])
            if owned_parent.exists():
                try:
                    owned_parent.rmdir(); result['empty_parent_cgroup_removed'] = True
                except OSError as error:
                    result['empty_parent_cgroup_removed'] = False
                    result['cgroup_parent_error'] = repr(error)[:256]
        finally:
            signal.alarm(0)
        provenance=result.get('database_child_provenance',{})
        result['database_child_cleanup_verified'] = (type(db_pid) is int and provenance.get('pid')==db_pid
            and provenance.get('source')=='database.Database.start -> Harness.launch direct Popen'
            and natural(result.get('database_cleanup',{}))
            and result.get('database_waitpid_state') in ('reaped_by_checkpoint','already_reaped_or_not_child'))
        result['natural_cleanup'] = (natural(result.get('service_cleanup', {}), service=True)
            and natural(result.get('database_cleanup', {})) and result.get('empty_parent_cgroup_removed') is True
            and result.get('database_child_cleanup_verified') is True)
        if result.get('post_close_quiet') is not True:
            result['status'] = 'unknown'
        if not result['natural_cleanup']:
            result['status'] = 'unknown'
        result['elapsed_seconds'] = time.monotonic() - began
        save(evidence / 'summary.json', hosted.redact(result, root))
        # Keep identities/layout/raw checkpoint, not temporary executables,
        # restored CLI/driver or the disposable PostgreSQL data directory.
        for filename in ('prepared.json', 'layout.json', 'known-static-symbols.json', 'layout-build.stdout', 'layout-build.stderr', 'layout.stdout', 'layout.stderr', 'normal-global-symbol.readback'):
            path = root / 'layout' / filename
            if path.exists(): shutil.copy2(path, evidence / filename)
        for path in (root / 'layout/layout-only', root / 'layout/layout-only.c',
                     original / 'B/build/ku-cargo', original / 'Driver/build'):
            if path.is_dir(): shutil.rmtree(path)
            elif path.exists(): path.unlink()
        if natural(result.get('database_cleanup', {})):
            for path in (fixture_root / 'database/data', fixture_root / 'database/socket'):
                if path.exists(): shutil.rmtree(path)
    return result


def evidence_manifest(directory):
    files=[]; total=0
    for path in sorted(Path(directory).rglob('*')):
        if path.is_symlink():raise ValueError('unsupported evidence symlink')
        if path.name=='manifest.json' or path.is_dir():continue
        if not path.is_file():raise ValueError('unsupported evidence member')
        size=path.stat().st_size; total+=size
        if size>5<<20 or total>16<<20 or len(files)>=64:raise ValueError('bounded evidence exceeds allowlist limits')
        files.append(dict(path=str(path.relative_to(directory)),bytes=size,sha256=checkpoint.digest(path)))
    save(Path(directory)/'manifest.json',dict(schema='ku-normal-owner-export-v1',files=files,total_bytes=total,
        manifest_self_excluded=True,normal_ELF_or_C_rewritten=False,original_archive_included=False,
        scope='All selected raw checkpoint files are exported directly, not filtered by generic performance archive names.'))


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--old-zip', type=Path, required=True); parser.add_argument('--root', type=Path, required=True)
    args = parser.parse_args(argv)
    if platform.system() != 'Linux' or platform.machine() != 'aarch64' or os.geteuid() != 0:
        raise ValueError('existing isolated Linux ARM64 root harness required')
    if any(os.environ.get(k) for k in ('LD_PRELOAD', 'LD_AUDIT', 'LD_LIBRARY_PATH', 'GNUTLS_SKIP_GLOBAL_INIT')):
        raise ValueError('ordinary checkpoint cannot inherit a library override/interposer')
    root = args.root.resolve(); root.mkdir(exist_ok=False); (root / 'evidence').mkdir()
    old_alarm = signal.signal(signal.SIGALRM, alarm); old_term = signal.signal(signal.SIGTERM, alarm)
    signal.alarm(85)
    began = time.monotonic()
    try:
        restoration = restorer.restore_old(args.old_zip, root / 'original',
            REPO / '.github/fixtures/libpq-loader-memory/original-allowlist.json')
        save(root / 'evidence/restoration.json', restoration)
        if checkpoint.digest('/usr/lib/aarch64-linux-gnu/libpq.so.5') != D_SHA:
            raise ValueError('system default D libpq changed; no service started')
        result = run_restored(root)
        print(json.dumps(dict(status=result['status'], natural_cleanup=result['natural_cleanup'],
                              capture_attempts=result['capture_attempts']), sort_keys=True))
        return 0 if result['natural_cleanup'] else 1
    except (OSError, ValueError, RuntimeError, TimeoutError) as error:
        save(root / 'evidence/failure.json', dict(status='unknown', error=repr(error)[:1024],
             diagnostic_only=True, score_eligible=False, elapsed_seconds=time.monotonic()-began))
        return 1
    finally:
        signal.alarm(0); signal.signal(signal.SIGALRM, old_alarm); signal.signal(signal.SIGTERM, old_term)
        for name in ('layout-build.stdout', 'layout-build.stderr', 'layout.stdout', 'layout.stderr', 'normal-global-symbol.readback', 'known-static-symbols.json'):
            path=root/'layout'/name
            if path.exists(): shutil.copy2(path, root/'evidence'/name)
        for name in ('layout-only', 'layout-only.c'):
            (root/'layout'/name).unlink(missing_ok=True)
        args.old_zip.unlink(missing_ok=True)
        (root / 'original-evidence.tar.gz').unlink(missing_ok=True)
        evidence_manifest(root / 'evidence')


if __name__ == '__main__':
    raise SystemExit(main())
