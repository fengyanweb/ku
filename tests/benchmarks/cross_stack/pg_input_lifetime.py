#!/usr/bin/env python3
"""Single normal-Release AB PG input-lifetime observation; never a score."""
import argparse, grp, hashlib, json, os, platform, pwd, secrets, shutil, sys, tempfile, threading, time
from pathlib import Path
sys.dont_write_bytecode = True
import database as db, linux_harness as lh
REPO = Path(__file__).resolve().parents[3]; sys.path.insert(0, str(REPO / 'tests/acceptance'))
import http_pg_lifecycle as fixture
from multiloop_load import process_sample
SQL = 'SELECT octet_length($1::text)::text,md5($1::text) FROM pg_sleep(1.0) /* ku_sent_inputs_v1 */'
SOURCE = '''import http from "std.http"
import pg from "std.pg"
import "std.fs"
fn main(): null! {
    client = pg.client({conninfo: fs.read("db.conn")?, max_connections: 1, max_waiters: 1,
        connect_timeout_ms: 2000, acquire_timeout_ms: 250, query_timeout_ms: 2000})?
    async fn check(): HttpResponse! {
        result = client.query("SELECT 1::text", [])?
        return ok(http.text(result.value(0, 0)?))
    }
    async fn waiting(): HttpResponse! {
        body = ""
        try {
            input = fs.read("input-1.txt")? + fs.read("input-2.txt")? + fs.read("input-3.txt")?
            result = client.query("@SQL@", [input.clone()])?
            body = result.value(0, 0)? + ":" + result.value(0, 1)?
        } catch(err) { body = "error:" + err.domain.clone() + ":" + err.code.clone() }
        return ok(http.text(body))
    }
    app = http.server({max_connections: 8, max_active_requests: 2, max_pending_requests: 2,
        max_buffer_bytes: 8388608, handler_timeout_ms: 2000})
    app.get("/check", check)
    app.get("/waiting", waiting)
    app.listen(fs.read("http.addr")?)?
    return ok(null)
}
'''.replace('@SQL@', SQL)
R = dict(schema='ku-PG-sent-inputs-Linux-v1', diagnostic_only=True, formal_qualified=False, status='preflight', original_eight_memory_score=False,
    statistical_adoption=False, maximum_replacements=0, commands=[], lanes={}, cleanup_errors=[])
def sha(path): return hashlib.sha256(Path(path).read_bytes()).hexdigest()
def save(): lh.write_json(EVIDENCE / 'result.json', R) if globals().get('OWN_RESULT') else None
def tool(command, **kwargs):
    row = dict(command=list(map(str, command)), cwd=str(kwargs.get('cwd', ROOT)), began=time.monotonic()); R['commands'].append(row)
    try:
        output = fixture.run(row['command'], progress=lambda phase, **v: row.setdefault('events', []).append(dict(phase=phase, **v)), timeout_seconds=kwargs.pop('timeout_seconds', 30), **kwargs)
        row.update(exit_code=0, output=output); return output
    except BaseException as error: row['error'] = repr(error); raise
    finally: row['ended'] = time.monotonic()
def request(port, path, token=None):
    connection = fixture.DeadlineHTTPConnection('127.0.0.1', port, time.monotonic() + 4)
    try:
        connection.request('POST' if token else 'GET', path, headers={'X-Ku-Control-Token': token} if token else {})
        response = connection.getresponse(); body = response.read(4097)
        if response.status != 200 or len(body) > 4096: raise RuntimeError((response.status, body[:200]))
        return body.decode()
    finally: connection.close()
def capture(answer, port):
    try: answer['body'] = request(port, '/waiting')
    except BaseException as error: answer['error'] = repr(error)
def cleanup_service(process, domain, lane):
    forced = bool(process and process.poll() is None)
    if forced: domain.kill(); process.wait(timeout=3)
    after = domain.snapshot(); removed = domain.remove()
    lane['cleanup'] = dict(forced=forced, exit_code=process.returncode if process else None, after=after, cgroup_removed=removed, natural=not forced and bool(process and process.returncode == 0) and after['process_count'] == 0 and removed)
    if after['process_count'] or not removed: raise RuntimeError('owned service domain did not empty/remove')
    if forced: raise RuntimeError('service required forced cleanup; not natural reclamation')
def run_lane(name, private, harness, database, observer, environment, cpus, expected, libpq):
    lane = R['lanes'][name] = dict(status='running'); cwd = private / name; cwd.mkdir(mode=0o755); cwd.chmod(0o755)
    for i, count in enumerate((700000, 700000, 697152), 1):
        p = cwd / f'input-{i}.txt'; p.write_bytes(b'x' * count); os.chown(p, harness.uid, harness.gid); p.chmod(0o600)
    appname = 'ku_inputs_' + name + '_' + secrets.token_hex(6); port = lh.free_port(); binary = cwd / 'service'; cli = ROOT / (name + '-ku')
    (cwd / 'db.conn').write_text(database['db_url'] + '&application_name=' + appname)
    (cwd / 'http.addr').write_text(f'127.0.0.1:{port}'); (cwd / 'service.ku').write_text(SOURCE)
    lane.update(cli_sha256=sha(cli), cli_version=tool([cli, '--version']), application_name=appname)
    build = tool([cli, 'build', '--verbose', '--backend', 'c', '--profile', 'release', cwd / 'service.ku', '-o', binary], cwd=cwd, env=environment, timeout_seconds=60)
    build += '\n' + json.dumps(R['commands'][-1].get('events', [])); (EVIDENCE / (name + '-build.log')).write_text(build)
    if '-std=c11' not in build or '-O2' not in build or '-O3' in build: raise RuntimeError('actual default C11/O2 not verified')
    generated = list((cwd / '.ku/build/release/c').rglob('*.c'))
    if len(generated) != 1: raise RuntimeError('ambiguous generated C')
    shutil.copyfile(generated[0], EVIDENCE / (name + '.c')); binary.chmod(0o755)
    links = tool(['ldd', binary]); paths = [Path(l.split('=>', 1)[1].split()[0]) for l in links.splitlines() if 'libpq.so' in l and '=>' in l]
    if len(paths) != 1 or sha(paths[0]) != sha(libpq): raise RuntimeError('linked libpq differs from selected library')
    lane.update(C_sha256=sha(generated[0]), ELF_sha256=sha(binary), libpq_sha256=sha(paths[0]), libraries=links, service_command=[str(binary)])
    token = secrets.token_hex(24); domain = lh.Domain.__new__(lh.Domain); process = None
    try:
        lh.Domain.__init__(domain, harness, 'service-' + name, 2, lh.GIB, cpus)
        process = harness.launch([str(binary)], cwd, environment | {'KU_SERVICE_CONTROL_TOKEN': token}, domain, EVIDENCE / (name + '-service.log'))
        lane.update(pid=process.pid, domain=str(domain.path), limits=domain.settings); deadline = time.monotonic() + 8
        while True:
            try:
                if request(port, '/check') == '1': break
            except OSError: pass
            if process.poll() is not None or time.monotonic() >= deadline: raise RuntimeError('service readiness failed')
            time.sleep(.02)
        proc = Path('/proc') / str(process.pid); status = (proc / 'status').read_text(); ids = {k: [int(x) for x in next(l for l in status.splitlines() if l.startswith(k + ':')).split()[1:]] for k in ('Uid', 'Gid')}
        mapped = {Path(l.split()[-1]) for l in (proc / 'maps').read_text().splitlines() if 'libpq.so' in l and l.split()[-1].startswith('/')}
        if ids != {'Uid': [harness.uid] * 4, 'Gid': [harness.gid] * 4} or sha(proc / 'exe') != lane['ELF_sha256'] or len(mapped) != 1 or any(sha(p) != sha(libpq) for p in mapped): raise RuntimeError('live service UID/GID/ELF/libpq identity differs')
        lane['runtime_identity'] = dict(ids, start_ticks=(proc / 'stat').read_text().rsplit(')', 1)[1].split()[19], mapped_libpq=[str(p) for p in mapped])
        def memory():
            began = time.monotonic(); record = dict(domain=domain.snapshot(), process=process_sample(process.pid), began=began, ended=time.monotonic())
            if process.poll() is not None or record['domain']['process_count'] != 1 or 'rss_bytes' not in record['process'] or 'sample_error' in record['process'] or not isinstance(record['domain'].get('memory_current'), int) or not isinstance(record['domain'].get('memory_stat', {}).get('anon'), int): raise RuntimeError('live service RSS/current/anon sample unavailable')
            return record
        lane['ready'] = memory(); answer = {}; thread = threading.Thread(target=capture, args=(answer, port)); thread.start()
        try:
            query = f"SELECT pid FROM pg_stat_activity WHERE application_name='{appname}' AND state='active' AND wait_event_type='Timeout' AND wait_event='PgSleep' AND query='{SQL}'"
            deadline = time.monotonic() + 1.8; before = ''
            for _ in range(12):
                before = observer(query, deadline)
                if before: break
                time.sleep(.02)
            lane['PgSleep_bracket'] = dict(before=before, valid=False)
            if not before: raise RuntimeError('no complete send-after PgSleep window; no replacement')
            lane['waiting'] = memory(); after = observer(query, deadline); lane['PgSleep_bracket'].update(after=after, valid=bool(before == after))
        finally:
            thread.join(timeout=5); lane['request'] = answer
            if thread.is_alive(): raise RuntimeError('bounded request helper did not finish')
        if answer != {'body': expected} or not lane['PgSleep_bracket']['valid']: raise RuntimeError('wrong result or invalid wait bracket')
        lane['fresh_normal_read'] = request(port, '/check'); lane['after'] = memory()
        if lane['fresh_normal_read'] != '1': raise RuntimeError('new normal request failed')
        request(port, '/__ku/drain', token); process.wait(timeout=5)
        if process.returncode != 0: raise RuntimeError('non-normal exit')
        deadline = time.monotonic() + 2
        while observer(f"SELECT count(*) FROM pg_stat_activity WHERE application_name='{appname}'", deadline) != '0': time.sleep(.02)
        lane['status'] = 'observed'
    except BaseException as error: lane.update(status='failed', error=repr(error)); raise
    finally:
        if hasattr(domain, 'path') and domain.path.exists(): cleanup_service(process, domain, lane)
        save()
def main():
    global ROOT, EVIDENCE, OWN_RESULT
    parser = argparse.ArgumentParser(description=__doc__); parser.add_argument('--root', type=Path, required=True); parser.add_argument('--uid', required=True); parser.add_argument('--gid', required=True); args = parser.parse_args()
    if platform.system() != 'Linux' or os.geteuid() != 0: raise RuntimeError('root Linux cgroup v2 only')
    os.umask(0o022)
    uid = int(args.uid) if args.uid.isdecimal() else pwd.getpwnam(args.uid).pw_uid; gid = int(args.gid) if args.gid.isdecimal() else grp.getgrnam(args.gid).gr_gid
    if uid <= 0 or gid <= 0: raise RuntimeError('service/DB must use a non-root runner')
    ROOT = args.root.resolve(strict=True); ROOT.chmod(0o755); EVIDENCE = ROOT / 'evidence'
    if EVIDENCE.is_symlink(): raise RuntimeError('refuse linked evidence directory')
    EVIDENCE.mkdir(mode=0o755, exist_ok=True)
    with (EVIDENCE / 'result.json').open('x') as stream: json.dump(R, stream)
    OWN_RESULT = True
    for n in ('A', 'B'):
        if not (ROOT / (n + '-ku')).is_file() or (ROOT / (n + '-ku')).is_symlink(): raise RuntimeError('missing regular CLI: ' + n)
    if any(os.environ.get(k) for k in ('KU_CFLAGS', 'KU_CFLAGS_RELEASE', 'LD_PRELOAD', 'LD_AUDIT', 'LD_LIBRARY_PATH', 'PGOPTIONS', 'PGSERVICE', 'MALLOC_ARENA_MAX', 'MALLOC_MMAP_THRESHOLD_')): raise RuntimeError('non-default build/library/allocator environment')
    if os.environ.get('KU_CC', 'clang-18') != 'clang-18': raise RuntimeError('only unmodified clang-18 is allowed')
    cpus = sorted(os.sched_getaffinity(0))
    if len(cpus) < 3: raise RuntimeError('need two service CPUs and a separate DB CPU')
    private = Path(tempfile.mkdtemp(prefix='private-', dir=ROOT)); private.chmod(0o755); group = Path('/sys/fs/cgroup') / ('ku-pg-input-' + secrets.token_hex(8)); database = None
    environment = os.environ | dict(KU_CC='clang-18', KU_EVENT_LOOPS='2', KU_BLOCKING_THREADS='2', KU_DB_MAX_CONNECTIONS='1', KU_DB_MAX_WAITERS='1', KU_DB_MAX_PENDING_BYTES=str(8 << 20), PGCLIENTENCODING='UTF8')
    R.update(status='running', source_sha256=hashlib.sha256(SOURCE.encode()).hexdigest(), input_bytes=2 << 20, input_sha256=hashlib.sha256(b'x' * (2 << 20)).hexdigest(), sql=SQL, uid=uid, gid=gid, service_cpus=cpus[:2], database_cpu=cpus[-1], cgroup_root=str(group))
    try:
        libdir = tool(['pg_config', '--libdir']); environment['KU_PG_LIB'] = libdir; libpq = Path(libdir) / 'libpq.so'
        R.update(libpq_sha256=sha(libpq), libpq_path=str(libpq.resolve()), cc=tool(['clang-18', '--version']), pg_config=tool(['pg_config', '--version']))
        lh.write_json(private / 'manifest.json', dict(uid=uid, gid=gid, stacks={}, protocol=str(Path(__file__).with_name('protocol.json')), cgroup_root=str(group), database_cpu_set=str(cpus[-1])))
        database = db.Database(private, uid, gid); R['database_start'] = database.start(); spec = lh.read_json(database.manifest_path)
        harness = lh.Harness(private, EVIDENCE / 'harness'); domain, exists = database.validated_domain(database.state())
        if not exists: raise RuntimeError('owned database domain missing')
        def observer(statement, deadline):
            remaining = deadline - time.monotonic()
            if remaining <= 0: raise TimeoutError('observation deadline; no replacement')
            value = database.bounded('observe', [db.PG / 'psql', '-X', '-A', '-t', '-v', 'ON_ERROR_STOP=1', '-h', '127.0.0.1', '-p', db.PORT, '-U', 'bench', '-d', 'bench', '-c', statement], domain, min(3, remaining)).decode().strip()
            if time.monotonic() >= deadline: raise TimeoutError('observation ended after deadline')
            return value
        expected = str(2 << 20) + ':' + hashlib.md5(b'x' * (2 << 20)).hexdigest(); R['expected'] = expected
        for name in ('A', 'B'): run_lane(name, private, harness, spec, observer, environment, ','.join(map(str, cpus[:2])), expected, libpq)
        R['status'] = 'observed'
    except BaseException as error: R.update(status='failed', error=repr(error)); raise
    finally:
        try:
            if database is not None: R['database_stop'] = database.stop()
            if database is not None and (R['database_stop'].get('forced') is not False or R['database_stop'].get('cgroup_removed') is not True or R['database_stop'].get('after', {}).get('process_count') != 0): raise RuntimeError('database cleanup was not natural and complete')
        except BaseException as error: R['cleanup_errors'].append(dict(stage='database', error=repr(error)))
        try:
            if group.exists(): group.rmdir()
            shutil.rmtree(private); R['private_tree_removed'] = True
        except BaseException as error: R['cleanup_errors'].append(dict(stage='owned-root', error=repr(error)))
        if R['cleanup_errors']: R['status'] = 'failed'
        save()
    if R['status'] != 'observed': raise RuntimeError('observation or cleanup incomplete')
if __name__ == '__main__':
    try: main()
    except BaseException as error:
        R.update(status='failed', error=repr(error)); save() if 'EVIDENCE' in globals() else None; raise
