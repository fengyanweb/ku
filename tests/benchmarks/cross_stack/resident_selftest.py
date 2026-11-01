#!/usr/bin/env python3
"""Linux/glibc semantics and external stop/read/resume smoke; no server/load."""
from __future__ import annotations
import json
import os
from pathlib import Path
import select
import subprocess
import sys
import tempfile
import resident_snapshot


def run(compiler='cc', library=None):
    source = Path(__file__).resolve().parent
    with tempfile.TemporaryDirectory(prefix='ku-resident-selftest-') as temporary:
        work = Path(temporary)
        fixture = work/'fixture'
        flags = [str(compiler), '-O2', '-fno-builtin', '-std=c11', '-Wall', '-Wextra', '-Werror']
        commands = []
        def compile(argv):
            subprocess.run(argv, check=True, timeout=30, stdin=subprocess.DEVNULL, capture_output=True)
            commands.append(argv)
        if library is None:
            library = work/'resident-live.so'
            compile(flags + ['-shared', '-fPIC', str(source/'resident_live.c'), '-o', str(library)])
        library = Path(library).resolve(strict=True)
        compile(flags + [str(source/'resident_live_fixture.c'), '-o', str(fixture)])
        child = subprocess.Popen([str(fixture)], env={**os.environ, 'LD_PRELOAD': str(library)},
                                 stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True)
        try:
            if not select.select([child.stdout], [], [], 5)[0]:
                raise RuntimeError('fixture readiness timeout')
            expected = json.loads(child.stdout.readline())
            resident_snapshot.capture(child.pid, library, work/'result')
            result = json.loads((work/'result/live.json').read_text())
            if not result['live_table_valid']:
                raise RuntimeError('invalid live table: '+repr(result['status']))
            records = {r['pointer']: r for r in result['records']}
            for label, bytes_ in [('malloc', 37), ('calloc', 87), ('failed_realloc_old', 53)]:
                if records[expected[label]]['requested_bytes'] != bytes_:
                    raise RuntimeError('live allocation differs: '+label)
                if Path(records[expected[label]]['callsite_file']).resolve()!=fixture.resolve():
                    raise RuntimeError('wrapper return PC does not identify actual caller: '+label)
            if len(result['threads']) != 1 or any('error' in m for m in result['modules'].values()):
                raise RuntimeError('thread/module identities unavailable')
            child.stdin.write('x'); child.stdin.flush()
            if child.wait(timeout=5) != 0:
                raise RuntimeError('fixture did not exit naturally')
            return dict(result='pass', commands=commands, assertions=['malloc', 'calloc-zero', 'calloc-overflow',
                  'realloc-NULL', 'realloc-failed-preserves-old', 'glibc-realloc-size-zero',
                  'posix_memalign-invalid-and-valid', 'churn-delete', 'stopped-stable-thread-list',
                  'inflight-zero', 'actual-caller-return-PC', 'exact-module-identities', 'natural-exit'])
        finally:
            if child.poll() is None:
                child.kill(); child.wait(timeout=5)
            child.stdin.close(); child.stdout.close()


if __name__ == '__main__':
    print(json.dumps(run(sys.argv[1] if len(sys.argv)>1 else 'cc')))
