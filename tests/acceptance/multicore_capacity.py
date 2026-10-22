#!/usr/bin/env python3
"""B01--B08 bounded native capacity acceptance; no production services.

The original sustained fixture and its deadlines remain independent. All native
sources, executables, peer sockets and client binaries live in one TemporaryDirectory.
Only compact input identities, measurements and bounded diagnostic traces survive.
"""
import argparse
import hashlib
import http.client
import json
import os
from pathlib import Path
import platform
import shutil
import signal
import socket
import subprocess
import sys
import tempfile
import time

sys.dont_write_bytecode = True
from multiloop_load import Service, atomic_json
from runtime_matrix import machine_environment


def digest(path):
    return hashlib.sha256(Path(path).read_bytes()).hexdigest()


def command(args, **kwargs):
    result = subprocess.run(args, capture_output=True, text=True, timeout=180, **kwargs)
    if result.returncode:
        raise RuntimeError(f"command failed {args[0]}: {result.stderr[-6000:]}")
    return result.stdout[-3000:]


def build(ku, root, peer_port):
    repository = Path(__file__).resolve().parents[2]
    fixture = repository / "tests/fixtures/multicore_capacity.ku"
    source = root / "capacity.ku"
    source.write_text(fixture.read_text().replace("__PEER_PORT__", str(peer_port)))
    (root / "capacity.txt").write_text("file-ready")
    executable = root / ("capacity.exe" if os.name == "nt" else "capacity")
    output = command([str(ku), "build", "--backend", "c", "--profile", "release",
                      "--emit-c", "-o", str(executable), str(source)], cwd=root)
    generated = list(root.rglob("capacity.c"))
    return executable, {"fixture_sha256": digest(fixture), "expanded_source_sha256": digest(source),
                        "native_sha256": digest(executable), "build_output": output,
                        "generated_c_sha256": [digest(p) for p in generated]}


def trace_exchange(service, count=12):
    connection = http.client.HTTPConnection("127.0.0.1", service.port, timeout=2)
    samples = []
    try:
        for index in range(count):
            start = time.monotonic_ns()
            connection.request("GET", "/cpu", headers={"Connection": "keep-alive"})
            submitted = time.monotonic_ns()
            response = connection.getresponse()
            headers = time.monotonic_ns()
            body = response.read(65536)
            ended = time.monotonic_ns()
            samples.append({"index": index, "submit_ns": submitted-start,
                            "headers_after_submit_ns": headers-submitted,
                            "body_after_headers_ns": ended-headers, "total_ns": ended-start,
                            "status": response.status, "body": body.decode(errors="replace")})
            if response.status != 200 or body != b"cpu-ready":
                raise RuntimeError(f"diagnostic unexpected response: {samples[-1]}")
    finally:
        connection.close()
    return samples


def diagnose(executable, root, evidence):
    service = Service(executable, root, 1)
    probes = []
    result = {"kind": "finite header/body and own-loopback trace", "instrumentation": []}
    try:
        if platform.system() == "Linux":
            for name, args in (
                ("packets", ["tcpdump", "-i", "lo", "-n", "-tt", "-l", "-s", "128", "-c", "240",
                             "tcp", "port", str(service.port)]),
                ("syscalls", ["strace", "-f", "-ttt", "-e", "trace=sendto,sendmsg,recvfrom,poll,ppoll",
                              "-p", str(service.child.pid)]),
            ):
                if shutil.which(args[0]):
                    log = (root / (name + ".txt")).open("wb")
                    prefix = [] if os.geteuid() == 0 else ["sudo", "-n"]
                    child = subprocess.Popen(prefix+args, stdout=log, stderr=log, start_new_session=True)
                    probes.append((name, child, log))
                    result["instrumentation"].append(name)
            # Startup synchronization only; this is outside the measured requests.
            time.sleep(.2)
        result["samples"] = trace_exchange(service)
        result["metrics"] = service.metrics()
    finally:
        for name, child, log in probes:
            if child.poll() is None:
                try:
                    os.killpg(child.pid, signal.SIGINT)
                    child.wait(timeout=3)
                except (OSError, subprocess.TimeoutExpired):
                    subprocess.run(["sudo", "-n", "kill", "-INT", str(child.pid)], timeout=3, check=False)
                    child.wait(timeout=3)
            log.close()
            data = (root / (name + ".txt")).read_bytes()
            # Fixed trace bound: do not upload an unbounded diagnostic stream.
            (evidence / (name + ".txt")).write_bytes(data[:262144])
            result[name] = {"bytes": len(data), "retained_bytes": min(len(data), 262144),
                            "exit": child.returncode}
        result["exit"] = service.stop()
    result["passed"] = result["exit"] == 0
    return result


def capacity(ku, executable, client, root, args, save):
    from capacity_cpu import checksum, run_client, work_path
    from capacity_wait import run_connection_occupancy, run_waiting
    result = {"calibration": [], "rounds": [], "legacy": []}
    service = Service(executable, root, 1)
    try:
        # Freeze the first bounded workload costing >=10 ms event CPU/request.
        # All later N configurations use exactly this same input and response.
        for units in (32, 128, 512, 1000):
            value = run_client(service, client, work_path(units), checksum(units), 3, concurrency=1)
            result["calibration"].append({"work_units": units, **value})
            save(result)
            if value["server_delta"].get("event_cpu_seconds", 0) / max(1, value["client"]["ledger"]["results"].get("correct_success", 0)) >= .01:
                break
        result["frozen_work_units"] = units
    finally:
        service.stop()
    path, expected = work_path(units), checksum(units)
    for repetition in range(args.repeat):
        for loops in ((1, 2, 4), (4, 1, 2), (2, 4, 1))[repetition % 3]:
            service = Service(executable, root, loops)
            item = {"repetition": repetition, "loops": loops}
            try:
                item["warmup"] = run_client(service, client, path, expected, args.warmup, concurrency=32)
                item["measurement"] = run_client(service, client, path, expected, args.duration, concurrency=32)
            finally:
                item["exit"] = service.stop()
            result["rounds"].append(item)
            save(result)
    for loops in (1, 2, 4):
        service = Service(executable, root, loops)
        item = {"loops": loops}
        try:
            item["measurement"] = run_client(service, client, "/cpu", "cpu-ready", 5, concurrency=8)
        finally:
            item["exit"] = service.stop()
        result["legacy"].append(item)
        save(result)
    service = Service(executable, root, 4)
    try:
        result["waiting"] = run_waiting(service, seconds=args.waiting)
        save(result)
        result["connection_occupancy"] = run_connection_occupancy(service)
        save(result)
    finally:
        result["waiting_exit"] = service.stop()
    return result


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--ku", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--mode", choices=("diagnose", "capacity", "all"), default="capacity")
    parser.add_argument("--go", default="go")
    parser.add_argument("--duration", type=float, default=30)
    parser.add_argument("--warmup", type=float, default=5)
    parser.add_argument("--repeat", type=int, default=3)
    parser.add_argument("--waiting", type=float, default=30)
    args = parser.parse_args()
    if not (0 < args.duration <= 60 and 0 < args.warmup <= 10 and 1 <= args.repeat <= 3 and 0 < args.waiting <= 30):
        parser.error("duration/repetition outside bounded acceptance plan")
    args.ku = args.ku.resolve()
    args.output = args.output.resolve()
    args.output.parent.mkdir(parents=True, exist_ok=True)
    repo = Path(__file__).resolve().parents[2]
    report = {"source_commit": command(["git", "rev-parse", "HEAD"], cwd=repo).strip(),
              "source_dirty": bool(command(["git", "status", "--porcelain"], cwd=repo)),
              "cli_sha256": digest(args.ku), "environment": machine_environment(),
              "plan": vars(args) | {"ku": str(args.ku), "output": str(args.output),
                 "loops": [1, 2, 4], "client_concurrency": 32, "total_background_threads": 5,
                 "thread_budget": 16, "task_bytes": 16777216,
                 "dedicated_wait_handler_deadline_ms": 45000,
                 "original_sustained_handler_deadline_ms": 1000}}
    save = lambda value=None: atomic_json(args.output, report)
    save()
    with tempfile.TemporaryDirectory(prefix="ku-multicore-capacity-") as directory:
        root = Path(directory)
        executable, identity = build(args.ku, root, 39081)
        report["build"] = identity
        save()
        if args.mode in ("diagnose", "all"):
            report["diagnosis"] = diagnose(executable, root, args.output.parent)
            save()
        if args.mode in ("capacity", "all"):
            client = root / ("client.exe" if os.name == "nt" else "client")
            report["go_version"] = command([args.go, "version"]).strip()
            command([args.go, "build", "-trimpath", "-o", str(client),
                     str(repo / "tests/acceptance/capacity_client.go")], cwd=root)
            report["client_sha256"] = digest(client)
            def checkpoint(value):
                report["capacity"] = value
                save()
            report["capacity"] = capacity(args.ku, executable, client, root, args, checkpoint)
            save()
    report["temporary_directory_removed"] = True
    save()
    print(json.dumps({"report": str(args.output), "source": report["source_commit"],
                      "mode": args.mode, "temporary_directory_removed": True}))


if __name__ == "__main__":
    main()
