#!/usr/bin/env python3
"""Bounded read-only macOS host evidence alongside a guest window.

ps RSS may repeat shared pages. Host swap counters cannot identify which
process/backing pages were swapped. Unix time aligns coarse windows; it does
not make host/guest monotonic clocks interchangeable. No apps are stopped.
"""
import argparse
import gzip
import hashlib
import json
from pathlib import Path
import subprocess
import time


def read(command):
    value = subprocess.run(command, check=True, capture_output=True, timeout=5)
    if len(value.stdout) > 1 << 20:
        raise RuntimeError('host observation exceeds1MiB bound')
    return value.stdout.decode('utf-8', errors='replace')


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--out', required=True, type=Path)
    parser.add_argument('--seconds', type=float, default=90)
    parser.add_argument('--interval', type=float, default=2)
    parser.add_argument('--stop-file', type=Path)
    args = parser.parse_args()
    if not 1 <= args.seconds <= 3600 or not .5 <= args.interval <= 30:
        raise ValueError('bounded duration1..3600s and interval0.5..30s required')
    start, used, count = time.monotonic(), 0, 0
    args.out.parent.mkdir(parents=True, exist_ok=True)
    with gzip.open(args.out, 'xb', compresslevel=6) as output:
        header = {'schema': 'ku-host-context-v1', 'unix_ns': time.time_ns(),
                  'entry_sha256': hashlib.sha256(Path(__file__).read_bytes()).hexdigest(),
                  'limits': read(['sysctl', 'hw.memsize', 'hw.ncpu']),
                  'scope': __doc__}
        output.write((json.dumps(header)+'\n').encode())
        while time.monotonic()-start < args.seconds:
            if args.stop_file and args.stop_file.exists():
                break
            row = {'unix_ns': time.time_ns(), 'monotonic_ns': time.monotonic_ns(),
                   'vm_stat': read(['vm_stat']), 'swapusage': read(['sysctl', 'vm.swapusage']),
                   'ps_comm_only': read(['ps', '-axo', 'pid,ppid,%cpu,rss,comm'])}
            data = (json.dumps(row)+'\n').encode()
            used += len(data)
            if used > 64 << 20:
                raise RuntimeError('host observation exceeded64MiB bound; retain partial evidence')
            output.write(data)
            output.flush()
            count += 1
            time.sleep(args.interval)
    print(json.dumps({'samples': count, 'uncompressed_bytes': used, 'seconds': time.monotonic()-start}))


if __name__ == '__main__':
    main()
