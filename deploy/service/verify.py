#!/usr/bin/env python3
"""Actual Ku native + HAProxy resident recovery / rolling / rollback check.

All generated sources, binaries, tokens, configs and processes are temporary.
Keep only the requested JSON result. This is W5/W6 coverage, not the complete
runtime matrix or the separate fixed 30-minute acceptance.
"""

import argparse
import hashlib
import http.client
import importlib.util
import json
import os
from pathlib import Path
import platform
import secrets
import shutil
import signal
import socket
import subprocess
import sys
import tempfile
import time
import traceback


HERE = Path(__file__).resolve().parent
SPEC = importlib.util.spec_from_file_location("ku_service_manage", HERE / "manage.py")
M = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(M)
ALLOCATED_ADDRESSES = set()


def task_command(name, verb):
    # name is generated locally from an ASCII-only random prefix, not user data.
    confirm = " -Confirm:$false" if verb == "Unregister" else ""
    return ["powershell.exe", "-NoProfile", "-NonInteractive", "-Command",
            f"$ErrorActionPreference='Stop'; {verb}-ScheduledTask -TaskName '{name}'{confirm}"]


def windows_host_processes(root):
    prefix = str(root).replace("'", "''")
    command = ("$ErrorActionPreference='Stop'; @("+
        "Get-CimInstance Win32_Process | Where-Object { $_.ProcessId -ne $PID -and "+
        f"$_.CommandLine -and $_.CommandLine.Contains('{prefix}') "+
        "} | Select-Object ProcessId,ParentProcessId,Name,CreationDate) | ConvertTo-Json -Depth 4 -Compress")
    result = subprocess.run(["powershell.exe", "-NoProfile", "-NonInteractive", "-Command", command],
                            capture_output=True, text=True, timeout=10, check=True)
    return json.loads(result.stdout or "[]")


def memory_retirement(args):
    """One actual service/provider integration case; not the old 36-case matrix."""
    report = {"kind": "resident-hard-memory-provider", "platform": platform.platform(),
              "database_retry": False, "database_used": False, "status": "prepared",
              "plan": {"worker_memory_bytes": 67108864, "event_loops": 2,
                  "blocking_threads": 2, "pressure_target_bytes": 268435456,
                  "replacement_deadline_seconds": 15, "pressure_requests": 1}}
    args.output.parent.mkdir(parents=True, exist_ok=True)
    def save():
        args.output.write_text(json.dumps(report, ensure_ascii=False, indent=2)+"\n", encoding="utf-8")
    save()
    config = None
    started = time.monotonic()
    with tempfile.TemporaryDirectory(prefix="ku-svc-hard-memory-") as temporary:
        root = Path(temporary)
        try:
            cli = root / ("ku.exe" if os.name == "nt" else "ku")
            shutil.copy2(args.ku.resolve(strict=True), cli)
            report["cli_sha256"] = hashlib.sha256(cli.read_bytes()).hexdigest()
            source = root / "memory.ku"
            source.write_text('''import {http, fs} from "std"
fn pressure(): HttpResponse! {
    text = fs.read("seed.txt")?
    i = 0
    while (i < 12) {
        text = text + text
        i = i + 1
    }
    return ok(http.text(text))
}
fn main(): null! {
    fs.write("application-started", "started")?
    address = fs.read("http.addr")?
    app = http.service()
    app.max_connections = 32
    app.max_active_requests = 16
    app.max_pending_requests = 16
    app.get("/", fn() { return http.text("ready") })
    app.get("/pressure", pressure)
    app.listen(address)?
    return ok(null)
}
''', encoding="utf-8")
            executable = root / ("memory.exe" if os.name == "nt" else "memory")
            build = subprocess.run([str(cli), "build", "--native", str(source), "-o", str(executable)],
                                   cwd=root, capture_output=True, text=True, timeout=60)
            report["build"] = {"exit": build.returncode, "stderr_tail": build.stderr[-4000:]}
            if build.returncode:
                raise RuntimeError("hard-memory native fixture did not build")
            data = json.loads((HERE / "cluster.example.json").read_text(encoding="utf-8"))
            data["deployment_budget"]["per_instance"].update(event_threads=2, blocking_threads=2, total_threads=13)
            token = root / "token"
            token.write_text(secrets.token_hex(32), encoding="ascii")
            token.chmod(0o600)
            data.update(ku=str(cli), control_token_file=str(token), frontend_addr=address(),
                        operation_timeout_seconds=15)
            data["service_options"].update(probe_interval_ms=100, probe_timeout_ms=100,
                startup_timeout_ms=5000, drain_timeout_ms=1000, restart_limit=3)
            for slot in data["slots"]:
                directory = root / slot["name"]
                directory.mkdir()
                slot["listen_addr"], slot["probe_addr"], slot["control_addr"] = address(), address(), address()
                (directory / "http.addr").write_text(slot["listen_addr"], encoding="ascii")
                (directory / "seed.txt").write_bytes(b"x" * 65536)
                target = directory / executable.name
                shutil.copy2(executable, target)
                slot.update(program=str(target), memory_limit_bytes=67108864)
                if platform.system() == "Linux":
                    if not args.cgroup_root:
                        raise RuntimeError("Linux memory-only requires a dedicated empty delegated --cgroup-root")
                    slot["cgroup_root"] = str(args.cgroup_root.resolve(strict=True))
                slot.pop("manager_start", None)
                slot.pop("manager_stop", None)
            path = root / "cluster.json"
            path.write_text(json.dumps(data), encoding="utf-8")
            config = M.load(path)
            a, b, _ = config["slots"]
            report["slot_preflight"] = {
                slot["name"]: M.status(config, slot) for slot in config["slots"]
            }
            save()
            if platform.system() == "Darwin":
                rejected = subprocess.run(M.command(config, a), env=M.environment(config, a),
                                          capture_output=True, text=True, timeout=5)
                report["unsupported_provider"] = {"exit": rejected.returncode,
                    "stderr": rejected.stderr[-4000:],
                    "application_started": (Path(a["program"]).parent/"application-started").exists()}
                if not (rejected.returncode != 0 and "unavailable" in rejected.stderr
                        and "macOS" in rejected.stderr and not report["unsupported_provider"]["application_started"]):
                    raise RuntimeError("macOS did not reject the hard provider before application startup")
                report["guarantee"] = "macOS native hard mode explicitly unavailable; no RSS-sampling substitute"
            else:
                if platform.system() == "Linux":
                    report["cgroup_before"] = [item.name for item in args.cgroup_root.iterdir() if item.is_dir()]
                    if report["cgroup_before"] or (args.cgroup_root/"cgroup.procs").read_text().strip():
                        raise RuntimeError("CI delegated cgroup root must initially be dedicated and empty")
                first = M.start(config, a)
                M.start(config, b)
                report["initial_status"] = first
                trigger = time.monotonic()
                try:
                    code, body = M.call(a["listen_addr"], "GET", "/pressure", timeout=5)
                    report["pressure_result"] = {"status": code, "body_prefix": body[:128]}
                except (OSError, ValueError, http.client.HTTPException) as error:
                    report["pressure_result"] = {"transport_or_limit_result": type(error).__name__, "detail": str(error)[:256]}
                others = [M.call(b["listen_addr"], "GET", "/") for _ in range(10)]
                deadline = trigger + 15
                while time.monotonic() < deadline:
                    current = M.status(config, a)
                    if current["phase"] == "ready" and current["generation"] > first["generation"]:
                        break
                    time.sleep(.02)
                else:
                    raise RuntimeError("limited native instance was not retired and replaced within 15 seconds")
                report.update(replacement_status=current,
                    recovery_ms=round((time.monotonic()-trigger)*1000, 3),
                    other_instance_responses=others,
                    replacement_business=M.call(a["listen_addr"], "GET", "/"))
                expected_metric = "linux_cgroup_v2_charged_memory_bytes" if platform.system() == "Linux" else "windows_job_committed_memory_bytes"
                if not (current.get("limit_metric") == expected_metric
                        and current.get("memory_peak_bytes", 0) >= 33554432
                        and current.get("database_outcome") == "unknown"
                        and current.get("database_retry") is False
                        and others == [(200, "ready")] * 10
                        and report["replacement_business"] == (200, "ready")):
                    raise RuntimeError("provider metrics, uncertainty, surviving instance or replacement response mismatch")
                if platform.system() == "Linux" and not current.get("memory_limit_observed"):
                    raise RuntimeError("Linux pressure did not record actual cgroup limit/OOM evidence")
                if platform.system() == "Linux":
                    evidence = current.get("memory_limit_evidence") or {}
                    worker = evidence.get("worker", {})
                    before = evidence.get("private_domain_before", {})
                    after = evidence.get("private_domain_after", {})
                    for counters in (worker, before, after):
                        if not (set(counters) == {"max", "oom", "oom_kill"}
                                and all(type(value) is int and value >= 0 for value in counters.values())):
                            raise RuntimeError("Linux provider omitted bounded raw worker/private-domain event evidence")
                    if any(after[key] < before[key] for key in before):
                        raise RuntimeError("Linux private-domain event counter decreased")
                    leaf_oom = worker["oom"] > 0
                    domain_oom = after["oom"] > before["oom"]
                    if not (worker["oom_kill"] > 0 and (leaf_oom or domain_oom)
                            and current.get("last_reason") == "os_memory_limit"):
                        raise RuntimeError("Linux pressure termination lacks correlated configured-domain OOM and worker victim evidence")
                    report["limit_event_scope"] = "worker_and_private_domain" if leaf_oom and domain_oom else "worker" if leaf_oom else "private_domain"
                report["guarantee"] = ("Linux cgroup v2 actual memory/oom evidence" if platform.system() == "Linux"
                    else "Windows configured Job commit limit, pressure process failure and replacement; no OS-kill or OOM inference from exit code")
            report["status"] = "passed"
        except BaseException as error:
            report["status"] = "failed"
            report["failure"] = f"{type(error).__name__}: {error}"
        finally:
            errors = []
            if config:
                for slot in config["slots"]:
                    try:
                        M.stop(config, slot)
                    except (OSError, RuntimeError, TimeoutError) as error:
                        errors.append(f"{slot['name']}: {error}")
            if platform.system() == "Linux" and args.cgroup_root:
                report["cgroup_after"] = [item.name for item in args.cgroup_root.iterdir() if item.is_dir()]
                if report["cgroup_after"]:
                    errors.append("provider domains remain under the dedicated CI delegation")
            report["cleanup_errors"] = errors
            if errors:
                report["status"] = "failed"
            report["duration_seconds"] = round(time.monotonic()-started, 3)
            save()
    print(json.dumps({"status": report["status"], "output": str(args.output)}, ensure_ascii=False))
    if report["status"] != "passed":
        raise SystemExit(1)


def blocking_retirement(args):
    """A14: real native pipe read remains live until process retirement.

    This path deliberately needs neither HAProxy nor a database and also runs
    on Windows. It shares the controlled kernel wait with runtime_matrix.py.
    """
    acceptance = HERE.parents[1] / "tests" / "acceptance"
    sys.path.insert(0, str(acceptance))
    try:
        spec = importlib.util.spec_from_file_location("ku_a14_matrix", acceptance / "runtime_matrix.py")
        matrix = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(matrix)
    finally:
        sys.path.remove(str(acceptance))
    report = {"kind": "native-blocking-service-retirement", "platform": platform.platform(),
              "python": platform.python_version(), "database_retry": False, "database_used": False,
              "windows_host_task": args.windows_host_task,
              "status": "prepared", "plan": {"handler_timeout_ms": 1000,
                  "native_cleanup_grace_ms": 1000, "native_drain_timeout_ms": 200,
                  "supervisor_drain_timeout_ms": 1500, "replacement_ready_deadline_seconds": 10,
                  "event_loops": 4, "blocking_threads": 5, "held_requests": 1,
                  "surviving_instance_requests": 10}}
    args.output.parent.mkdir(parents=True, exist_ok=True)
    def save():
        args.output.write_text(json.dumps(report, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")
    save()  # Freeze thresholds before building or starting the fixture.
    config = gate = peer = None
    task_names = []
    started = time.monotonic()
    with tempfile.TemporaryDirectory(prefix="ku-svc-native-block-") as temporary:
        root = Path(temporary)
        try:
            cli = root / ("ku.exe" if os.name == "nt" else "ku")
            shutil.copy2(args.ku.resolve(strict=True), cli)
            report["cli_sha256"] = hashlib.sha256(cli.read_bytes()).hexdigest()
            program = root / "blocking.ku"
            program.write_text('''import {http, fs} from "std"
async fn held(): HttpResponse! {
    path = fs.read("block.path")?
    text = fs.read(path)?
    return ok(http.text(text))
}
fn main(): null! {
    address = fs.read("http.addr")?
    app = http.service()
    app.max_connections = 32
    app.max_active_requests = 16
    app.max_pending_requests = 16
    app.handler_timeout_ms = 1000
    app.get("/", fn() { return http.text("ready") })
    app.get("/held", held)
    app.listen(address)?
    return ok(null)
}
''', encoding="utf-8")
            executable = root / ("service.exe" if os.name == "nt" else "service")
            build = subprocess.run([str(cli), "build", "--native", str(program), "-o", str(executable)],
                                   cwd=root, capture_output=True, text=True, timeout=60)
            report["build"] = {"exit": build.returncode, "stderr_tail": build.stderr[-4000:]}
            if build.returncode:
                raise RuntimeError("A14 native fixture did not build")
            report["native_sha256"] = hashlib.sha256(executable.read_bytes()).hexdigest()
            data = json.loads((HERE / "cluster.example.json").read_text(encoding="utf-8"))
            # Async native file work reserves one of the declared blocking
            # threads, in addition to the four synchronous HTTP owner workers.
            budget = data["deployment_budget"]
            budget["per_instance"].update(blocking_threads=5, total_threads=18)
            budget.update(blocking_threads=15, total_threads=56)
            token = root / "token"
            token.write_text(secrets.token_hex(32), encoding="ascii")
            token.chmod(0o600)
            data.update(ku=str(cli), control_token_file=str(token), frontend_addr=address(),
                        operation_timeout_seconds=10)
            data["service_options"].update(probe_interval_ms=100, probe_timeout_ms=100,
                drain_timeout_ms=1500, startup_timeout_ms=5000, restart_limit=3)
            directories = {}
            for slot in data["slots"]:
                directory = root / slot["name"]
                directory.mkdir()
                directories[slot["name"]] = directory
                slot["listen_addr"], slot["probe_addr"], slot["control_addr"] = address(), address(), address()
                (directory / "http.addr").write_text(slot["listen_addr"], encoding="ascii")
                target = directory / executable.name
                shutil.copy2(executable, target)
                slot.update(program=str(target), env={"KU_HTTP_DRAIN_TIMEOUT_MS": "200"})
                slot.pop("manager_start", None)
                slot.pop("manager_stop", None)
            if args.windows_host_task:
                prefix = f"KuCI-{os.getpid()}-{secrets.token_hex(4)}"
                task_names = [f"{prefix}-{slot['name']}" for slot in data["slots"]]
                for slot, name in zip(data["slots"], task_names):
                    slot["manager_start"] = task_command(name, "Start")
                    slot["manager_stop"] = task_command(name, "Stop")
            path = root / "cluster.json"
            path.write_text(json.dumps(data), encoding="utf-8")
            config = M.load(path)
            a, b, _ = config["slots"]
            gate = matrix.BlockingGate(directories["a"])
            if args.windows_host_task:
                host = root / "host"
                host.mkdir()
                for name in ("manage.py", "install-windows-task.ps1"):
                    shutil.copy2(HERE / name, host / name)
                # Only this isolated temporary tree gains read/execute access;
                # do not modify the runner checkout or credential locations.
                acl = subprocess.run(["icacls", str(root), "/grant", "*S-1-5-19:(OI)(CI)RX", "/T", "/C"],
                                     capture_output=True, text=True, timeout=10)
                report["host_acl"] = {"exit": acl.returncode, "stdout_tail": acl.stdout[-2000:],
                                      "stderr_tail": acl.stderr[-1000:]}
                if acl.returncode:
                    raise RuntimeError("temporary Local Service read/execute grant failed")
                install = subprocess.run(["powershell.exe", "-NoProfile", "-NonInteractive",
                    "-ExecutionPolicy", "Bypass", "-File", str(host / "install-windows-task.ps1"),
                    "-Config", str(path), "-Python", sys.executable, "-TaskPrefix", prefix, "-Register"],
                    capture_output=True, text=True, timeout=20)
                report["host_registration"] = {"exit": install.returncode,
                    "stdout_tail": install.stdout[-6000:], "stderr_tail": install.stderr[-3000:],
                    "task_names": task_names, "principal_requested": "NT AUTHORITY\\LOCAL SERVICE"}
                if install.returncode:
                    raise RuntimeError("Register-ScheduledTask failed; see retained exact API output")
                query = (f"$ErrorActionPreference='Stop'; @(Get-ScheduledTask -TaskName '{prefix}-*' | "+
                    "Select-Object TaskName,State,@{n='principal';e={$_.Principal.UserId}},"+
                    "@{n='logon_type';e={$_.Principal.LogonType.ToString()}},"+
                    "@{n='principal_sid';e={if($_.Principal.UserId -match '^S-[0-9-]+$') {$_.Principal.UserId} else {([System.Security.Principal.NTAccount]::new($_.Principal.UserId)).Translate([System.Security.Principal.SecurityIdentifier]).Value}}},"+
                    "@{n='action_count';e={@($_.Actions).Count}},"+
                    "@{n='execute';e={@($_.Actions)[0].Execute}},"+
                    "@{n='arguments';e={@($_.Actions)[0].Arguments}},"+
                    "@{n='restart_count';e={$_.Settings.RestartCount}},"+
                    "@{n='restart_interval';e={$_.Settings.RestartInterval}}) | ConvertTo-Json -Depth 4 -Compress")
                registered = subprocess.run(["powershell.exe", "-NoProfile", "-NonInteractive", "-Command", query],
                                           capture_output=True, text=True, timeout=10, check=True)
                report["host_registration_readback"] = json.loads(registered.stdout)
                if sorted(item["TaskName"] for item in report["host_registration_readback"]) != sorted(task_names):
                    raise RuntimeError("registered Task Scheduler names did not match the isolated plan")
                plans = {plan["task"]: plan for plan in json.loads(install.stdout)}
                for actual in report["host_registration_readback"]:
                    plan = plans[actual["TaskName"]]
                    if (actual["principal_sid"] != "S-1-5-19" or actual["logon_type"] != "ServiceAccount"
                            or actual["action_count"] != 1
                            or os.path.normcase(os.path.abspath(actual["execute"])) != os.path.normcase(os.path.abspath(plan["execute"]))
                            or actual["arguments"] != plan["arguments"]
                            or actual["restart_count"] != 3 or actual["restart_interval"] not in ("PT1M", "PT60S")):
                        raise RuntimeError("registered account, executable, argv or restart policy did not match the actual isolated plan")
            initial = M.start(config, a)
            M.start(config, b)
            report["initial_status"] = initial
            request_started = time.monotonic()
            host, port = a["listen_addr"].rsplit(":", 1)
            peer = socket.create_connection((host, int(port)), timeout=2)
            peer.sendall(b"GET /held HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
            entry_deadline = time.monotonic() + .8
            entered = False
            while time.monotonic() < entry_deadline:
                if os.name == "nt":
                    entered = gate.connected.is_set()
                elif platform.system() == "Linux":
                    pid = native_child(initial["pid"])
                    entered = any(handle.resolve() == Path(gate.path).resolve()
                        for handle in Path(f"/proc/{pid}/fd").iterdir())
                else:
                    pid = native_child(initial["pid"])
                    # Darwin may print /private/var for a /var locator and its
                    # filename filter can miss the FIFO. Inspect this child's
                    # actual numeric FDs and compare canonical paths instead.
                    opened = subprocess.run(["lsof", "-p", str(pid), "-Ffn"],
                                            capture_output=True, text=True, timeout=2)
                    report["kernel_wait_last_probe"] = {"native_pid": pid,
                        "lsof_exit": opened.returncode, "stdout_tail": opened.stdout[-1500:],
                        "stderr_tail": opened.stderr[-500:]}
                    entered = opened.returncode == 0 and any(line.startswith("n/")
                        and Path(line[1:]).resolve() == Path(gate.path).resolve()
                        for line in opened.stdout.splitlines())
                if entered:
                    break
                time.sleep(.01)
            if not entered:
                report["slot_files"] = [item.name for item in directories["a"].iterdir()]
                if platform.system() == "Darwin":
                    opened = subprocess.run(["lsof", "-p", str(pid), "-Ffn"],
                                            capture_output=True, text=True, timeout=3)
                    report["native_fds"] = opened.stdout[-6000:]
                    stack = root / "entry-stack.txt"
                    subprocess.run(["sample", str(pid), "1", "2", "-file", str(stack)],
                                   capture_output=True, text=True, timeout=5)
                    if stack.exists():
                        report["native_stack_excerpt"] = stack.read_text(encoding="utf-8", errors="replace")[:24000]
                peer.settimeout(.1)
                try:
                    report["early_http_response"] = peer.recv(1024).decode("utf-8", "replace")
                except OSError as error:
                    report["early_http_response"] = type(error).__name__
                raise RuntimeError("actual native pipe read entry was not observed before HTTP timeout")
            report["kernel_wait_entry_ms"] = round((time.monotonic()-request_started)*1000, 3)
            report["while_native_blocked"] = M.status(config, a)
            peer.settimeout(3)
            received = bytearray()
            while b"\r\n" not in received and len(received) < 4096:
                chunk = peer.recv(512)
                if not chunk:
                    break
                received.extend(chunk)
            report["held_http_status"] = bytes(received).split(b"\r\n", 1)[0].decode("ascii", "replace")
            report["held_http_response_ms"] = round((time.monotonic()-request_started)*1000, 3)
            if " 504 " not in report["held_http_status"]:
                raise RuntimeError("held native request did not produce the declared HTTP timeout")
            # Do not release the pipe: only native process retirement can end
            # its wait. The other instance serves fresh requests throughout.
            other = []
            left_ready_at = None
            deadline = request_started + 10
            while time.monotonic() < deadline:
                current = M.status(config, a)
                if current["phase"] != "ready" and left_ready_at is None:
                    left_ready_at = round((time.monotonic()-request_started)*1000, 3)
                if len(other) < 10:
                    code, body = M.call(b["listen_addr"], "GET", "/")
                    other.append({"status": code, "body": body})
                if current["phase"] == "ready" and current["generation"] > initial["generation"] and len(other) == 10:
                    break
                time.sleep(.05)
            else:
                raise RuntimeError("native work retirement did not recover within fixed 10-second deadline")
            report.update(first_not_ready_ms=left_ready_at, replacement_status=current,
                          replacement_ready_ms=round((time.monotonic()-request_started)*1000, 3),
                          surviving_instance_responses=other,
                          replacement_business=M.call(a["listen_addr"], "GET", "/"))
            if not (current.get("last_exit_code") == 75
                    and current.get("launcher_status") == "runtime_unfinished_work"
                    and current.get("database_outcome") == "unknown"
                    and current.get("database_retry") is False
                    and not current.get("termination_requested")
                    and all(item == {"status": 200, "body": "ready"} for item in other)
                    and report["replacement_business"] == (200, "ready")):
                raise RuntimeError("native runtime 75, no replay, uncertainty or replacement result mismatch")
            if args.windows_host_task:
                report["host_processes_before_stop"] = windows_host_processes(root)
                stopped = subprocess.run(task_command(task_names[0], "Stop"),
                                         capture_output=True, text=True, timeout=10)
                report["host_stop"] = {"exit": stopped.returncode,
                    "stdout_tail": stopped.stdout[-1000:], "stderr_tail": stopped.stderr[-2000:]}
                if stopped.returncode:
                    raise RuntimeError("Stop-ScheduledTask failed")
                M.wait_absent(config, a, 10)
                report["other_instance_after_host_stop"] = M.call(b["listen_addr"], "GET", "/")
                if report["other_instance_after_host_stop"] != (200, "ready"):
                    raise RuntimeError("other instance did not survive the real host task stop")
            report["status"] = "passed"
        except BaseException as error:
            report["status"] = "failed"
            report["failure"] = f"{type(error).__name__}: {error}"
        finally:
            if peer:
                peer.close()
            if gate:
                try:
                    gate.release()
                    report["pipe_release_error"] = gate.error
                except (OSError, RuntimeError) as error:
                    report["pipe_release_error"] = str(error)
                    report["status"] = "failed"
                report["pipe_release_note"] = "After native process retirement Windows may report broken pipe; it is not a business retry."
            errors = []
            if config:
                for slot in config["slots"]:
                    try:
                        M.stop(config, slot)
                    except (OSError, RuntimeError, TimeoutError) as error:
                        errors.append(f"{slot['name']}: {error}")
            if args.windows_host_task:
                for name in task_names:
                    removed = subprocess.run(task_command(name, "Unregister"), capture_output=True,
                                             text=True, timeout=10)
                    if removed.returncode:
                        errors.append(f"Unregister-ScheduledTask {name}: {removed.stderr[-1500:]}")
                try:
                    deadline = time.monotonic() + 10
                    while True:
                        remaining = windows_host_processes(root)
                        if not remaining or time.monotonic() >= deadline:
                            break
                        time.sleep(.1)
                    report["host_processes_after_cleanup"] = remaining
                    if remaining:
                        errors.append("temporary host-task process tree did not retire")
                except (OSError, subprocess.SubprocessError, ValueError) as error:
                    errors.append(f"host process cleanup receipt: {error}")
            report["cleanup_errors"] = errors
            if errors:
                report["status"] = "failed"
            report["duration_seconds"] = round(time.monotonic()-started, 3)
            save()
    print(json.dumps({"status": report["status"], "output": str(args.output)}, ensure_ascii=False))
    if report["status"] != "passed":
        raise SystemExit(1)


def address():
    while True:
        with socket.socket() as stream:
            stream.bind(("127.0.0.1", 0))
            value = f"127.0.0.1:{stream.getsockname()[1]}"
        if value not in ALLOCATED_ADDRESSES:
            ALLOCATED_ADDRESSES.add(value)
            return value


def native_child(leader):
    # The actual native service is the launcher's single direct child. Match
    # PPID, without treating an arbitrary name/prefix as ownership evidence.
    result = subprocess.run(["ps", "-axo", "pid=,ppid="], text=True, capture_output=True, timeout=3, check=True)
    matches = [int(parts[0]) for line in result.stdout.splitlines()
               if len(parts := line.split()) == 2 and int(parts[1]) == leader]
    if len(matches) != 1:
        raise RuntimeError(f"expected one native child of launcher {leader}, got {matches}")
    return matches[0]


def resource_snapshot(proxy):
    listing = subprocess.run(["ps", "-axo", "pid=,ppid=,rss="], capture_output=True, text=True, timeout=3, check=True)
    records = {int(parts[0]): {"ppid": int(parts[1]), "rss_bytes": int(parts[2]) * 1024}
               for line in listing.stdout.splitlines() if len(parts := line.split()) == 3}
    owners = {process.pid for process in M.OWNED_SUPERVISORS.values() if process.poll() is None}
    processes = set(owners)
    while True:
        descendants = {pid for pid, record in records.items() if record["ppid"] in processes}
        if descendants <= processes:
            break
        processes.update(descendants)
    processes.update([os.getpid(), proxy.pid])
    rows = []
    for pid in sorted(processes):
        if pid not in records:
            continue
        option = "-M" if os.uname().sysname == "Darwin" else "-T"
        result = subprocess.run(["ps", option, "-p", str(pid), "-o", "pid="], capture_output=True, text=True, timeout=2, check=True)
        lines = [line for line in result.stdout.splitlines() if line.strip()]
        threads = len(lines) - (1 if os.uname().sysname == "Darwin" else 0)
        rows.append({"pid": pid, "threads": threads, **records[pid]})
    return {"processes": rows, "process_count": len(rows), "thread_count": sum(row["threads"] for row in rows),
            "sum_rss_bytes": sum(row["rss_bytes"] for row in rows),
            "rss_note": "sum of process RSS includes shared pages more than once; not PSS or an OS hard limit"}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--ku", required=True, type=Path)
    parser.add_argument("--haproxy", default=shutil.which("haproxy"))
    parser.add_argument("--output", required=True, type=Path)
    parser.add_argument("--blocking-only", action="store_true", help="Cross-platform A14 native pipe retirement and resident replacement, without HAProxy")
    parser.add_argument("--windows-host-task", action="store_true", help="Actually register/start/stop/unregister unique Windows Task Scheduler instances for the A14 fixture")
    parser.add_argument("--memory-only", action="store_true", help="One resident-service hard memory provider recovery case, or macOS startup rejection")
    parser.add_argument("--cgroup-root", type=Path, help="Dedicated empty delegated Linux cgroup v2 root, used only with --memory-only")
    args = parser.parse_args()
    if args.memory_only:
        if args.blocking_only or args.windows_host_task:
            parser.error("--memory-only is a separate targeted provider acceptance")
        memory_retirement(args)
        return
    if args.windows_host_task and (os.name != "nt" or not args.blocking_only):
        parser.error("--windows-host-task requires Windows and --blocking-only")
    if args.blocking_only:
        blocking_retirement(args)
        return
    if os.name == "nt":
        parser.error("this HAProxy Unix-socket reference runs on Linux/macOS; Windows Ku service uses its Rust regression")
    if not args.haproxy:
        parser.error("HAProxy must be installed")
    args.ku = args.ku.resolve(strict=True)
    report = {"kind": "resident-service-acceptance", "platform": os.uname().sysname,
              "ku": str(args.ku), "database_retry": False, "results": {}, "complete_runtime_acceptance": False}
    report["cli_sha256"] = hashlib.sha256(args.ku.read_bytes()).hexdigest()
    report["python_version"] = sys.version.split()[0]
    report["architecture"] = os.uname().machine
    report["haproxy_version"] = subprocess.run([args.haproxy, "-v"], capture_output=True, text=True, timeout=3, check=True).stdout.splitlines()[0]
    report["base_commit"] = subprocess.run(["git", "-C", str(HERE.parents[1]), "rev-parse", "HEAD"], capture_output=True, text=True, timeout=3, check=True).stdout.strip()
    proxy = None
    config = None
    started = time.monotonic()
    diagnostic = {"stage": "prepare", "slot_last_status": {},
                  "surviving_b_requests": {"attempted": 0, "completed": 0,
                                            "http_200": 0, "status_codes": []}}
    report["diagnostic"] = diagnostic
    known_owned = {}

    def bounded_text(value, limit=256):
        text = str(value)
        if config is not None:
            text = text.replace(config["_token"], "<control-token>")
        return text[:limit]

    def control_context(error):
        context = getattr(error, "ku_control_context", None)
        if not isinstance(context, dict):
            return None
        return {key: bounded_text(context[key]) if isinstance(context[key], str) else context[key]
                for key in ("stage", "elapsed_ms", "error_type", "errno", "winerror", "detail")
                if key in context and (context[key] is None or type(context[key]) in (str, int, float))}

    def remember_owned():
        for name, process in list(M.OWNED_SUPERVISORS.items())[:3]:
            if process.pid in known_owned or len(known_owned) < 6:
                known_owned[process.pid] = (name, process)

    def stage(name):
        diagnostic.update(stage=name, stage_elapsed_ms=round((time.monotonic()-started)*1000, 3))
        remember_owned()

    def record_status(slot, state):
        # Only bounded lifecycle fields, never the config, argv, token or body.
        fields = ("phase", "pid", "generation", "last_reason", "last_exit_generation",
                  "last_exit_code", "last_launcher_signal", "launcher_status",
                  "owned_supervisor_alive", "termination_requested", "unfinished_work_possible",
                  "database_outcome", "database_retry", "control_error")
        safe = {key: bounded_text(state[key]) if isinstance(state[key], str) else state[key]
                for key in fields if key in state
                and (state[key] is None or type(state[key]) in (str, int, bool))}
        if isinstance(state.get("control_failure"), dict):
            wrapper = RuntimeError()
            wrapper.ku_control_context = state["control_failure"]
            safe["control_failure"] = control_context(wrapper)
        diagnostic["slot_last_status"][slot["name"]] = safe
        return state

    def slot_status(slot):
        return record_status(slot, M.status(config, slot))

    def owned_receipt():
        remember_owned()
        return [{"slot": name, "pid": pid, "returncode": process.poll()}
                for pid, (name, process) in known_owned.items()]

    with tempfile.TemporaryDirectory(prefix="ku-svc-", dir="/tmp") as temporary:
        root = Path(temporary)
        try:
            stage("freeze_cli_and_config")
            # Cargo may replace a workspace binary during independent checks.
            # Freeze one private CLI inode so the recorded hash identifies all
            # builds, generations and launcher invocations in this acceptance.
            fixed_cli = root / "ku"
            shutil.copy2(args.ku, fixed_cli)
            args.ku = fixed_cli
            report["cli_sha256"] = hashlib.sha256(fixed_cli.read_bytes()).hexdigest()
            report["cli_snapshot"] = "private immutable CLI copy; deleted after process cleanup"
            config_data = json.loads((HERE / "cluster.example.json").read_text())
            token = root / "token"
            token.write_text(secrets.token_hex(32))
            token.chmod(0o600)
            config_data.update(ku=str(args.ku), control_token_file=str(token),
                               frontend_addr=address(), haproxy_runtime_socket=str(root / "proxy.sock"),
                               operation_timeout_seconds=15)
            config_data["service_options"].update(probe_interval_ms=100, probe_timeout_ms=100,
                                                   drain_timeout_ms=1000, startup_timeout_ms=5000)
            builds = {}
            for revision in ("previous", "new"):
                stage(f"build_{revision}")
                build = root / revision
                build.mkdir()
                source = (HERE / "http_service.ku").read_text().replace("Ku HTTP OK", f"Ku HTTP OK {revision}")
                (build / "service.ku").write_text(source)
                result = subprocess.run([str(args.ku), "build", "--native", "service.ku", "-o", "service"],
                                        cwd=build, capture_output=True, text=True, timeout=120)
                if result.returncode:
                    raise RuntimeError(f"native build {revision}: {result.stdout}\n{result.stderr}")
                builds[revision] = build / "service"
            for slot in config_data["slots"]:
                directory = root / slot["name"]
                directory.mkdir()
                slot["listen_addr"], slot["probe_addr"], slot["control_addr"] = address(), address(), address()
                (directory / "http.addr").write_text(slot["listen_addr"])
                shutil.copy2(builds["previous"], directory / "previous")
                shutil.copy2(builds["new"], directory / "new")
                slot["program"] = str(directory / "previous")
                slot.pop("manager_start", None)
                slot.pop("manager_stop", None)
            config_path = root / "cluster.json"
            config_path.write_text(json.dumps(config_data))
            config = M.load(config_path)
            report["parameters"] = {"service_options": config["service_options"], "deployment_budget": config["deployment_budget"],
                                    "recovery_deadline_seconds": 10, "operation_timeout_seconds": config["operation_timeout_seconds"],
                                    "proxy_threads": 1, "tls": "loopback HTTP only; no TLS in this isolated fixture"}
            proxy_config = root / "haproxy.cfg"
            proxy_config.write_text(M.render(config))
            stage("validate_proxy_config")
            validation = subprocess.run([args.haproxy, "-c", "-f", str(proxy_config)], capture_output=True, text=True, timeout=10)
            if validation.returncode:
                raise RuntimeError(validation.stdout + validation.stderr)
            report["results"]["proxy_config"] = "passed"
            stage("start_proxy")
            proxy = subprocess.Popen([args.haproxy, "-db", "-f", str(proxy_config)],
                                     stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
            a, b, spare = config["slots"]
            stage("start_a")
            record_status(a, M.start(config, a))
            stage("start_b")
            record_status(b, M.start(config, b))
            report["initial_slot_status"] = {name: dict(diagnostic["slot_last_status"][name])
                                              for name in ("a", "b")}
            stage("normal_proxy")
            deadline = time.monotonic() + 3
            while time.monotonic() < deadline:
                try:
                    code, body = M.call(config["frontend_addr"], "GET", "/")
                    if code == 200 and body == "Ku HTTP OK previous":
                        break
                except OSError:
                    pass
                time.sleep(.05)
            else:
                raise RuntimeError("HAProxy did not admit the ready native instances")
            report["results"]["normal_proxy"] = "passed"
            stage("steady_resource_snapshot")
            report["steady_resource_snapshot"] = resource_snapshot(proxy)
            stage("native_exit_initial_a_status")
            initial = slot_status(a)
            failure_started = time.monotonic()
            stage("native_exit_resolve_target")
            target_pid = native_child(initial["pid"])
            diagnostic["fault_target"] = {"slot": "a", "launcher_pid": initial["pid"],
                                          "native_pid": target_pid, "generation": initial["generation"]}
            stage("native_exit_send_sigkill")
            os.kill(target_pid, signal.SIGKILL)
            ordinary = []
            for index in range(10):
                stage(f"native_exit_surviving_b_request_{index+1}")
                progress = diagnostic["surviving_b_requests"]
                progress["attempted"] += 1
                code = M.call(b["listen_addr"], "GET", "/")[0]
                ordinary.append(code)
                progress["completed"] += 1
                progress["http_200"] += int(code == 200)
                progress["status_codes"].append(code)
            if ordinary != [200] * 10:
                raise RuntimeError(f"surviving instance results: {ordinary}")
            stage("native_exit_wait_replacement")
            deadline = time.monotonic() + 10
            while time.monotonic() < deadline:
                recovered = slot_status(a)
                if recovered["phase"] == "ready" and recovered["generation"] > initial["generation"]:
                    break
                time.sleep(.02)
            else:
                raise RuntimeError("replacement was not ready within frozen 10 s recovery budget")
            report["results"]["native_exit_recovery"] = {"passed": True, "recovery_ms": round((time.monotonic() - failure_started) * 1000, 3),
                "old_generation": initial["generation"], "new_generation": recovered["generation"], "surviving_requests": len(ordinary)}
            rolling_started = time.monotonic()
            def capture_coexist():
                stage("rolling_resource_snapshot")
                report["rolling_resource_snapshot"] = resource_snapshot(proxy)
                stage("rolling_after_resource_snapshot")
            stage("rolling")
            deployment = M.roll(config, "a", "spare", root / "spare" / "new", on_coexist=capture_coexist)
            stage("rolling_proxy_responses")
            observations = [M.call(config["frontend_addr"], "GET", "/")[1] for _ in range(20)]
            if "Ku HTTP OK new" not in observations:
                raise RuntimeError("new revision never received proxy traffic")
            stage("rolling_old_slot_status")
            if slot_status(a)["phase"] != "absent":
                raise RuntimeError("old supervisor remained after rolling drain")
            report["results"]["rolling"] = {"passed": True, "duration_ms": round((time.monotonic() - rolling_started) * 1000, 3), "deployment": deployment}
            rollback_started = time.monotonic()
            stage("rollback")
            M.roll(config, deployment["new_slot"], deployment["old_slot"], deployment["old_program"], rollback=True)
            stage("rollback_proxy_responses")
            observations = [M.call(config["frontend_addr"], "GET", "/")[1] for _ in range(20)]
            if any(body != "Ku HTTP OK previous" for body in observations):
                raise RuntimeError(f"rollback content mismatch: {observations}")
            report["results"]["rollback"] = {"passed": True, "duration_ms": round((time.monotonic() - rollback_started) * 1000, 3), "verified_proxy_requests": len(observations)}
            stage("completed_checks")
            report["passed"] = True
        except BaseException as error:
            report["passed"] = False
            report["failure"] = bounded_text(f"{type(error).__name__}: {error}", 2048)
            diagnostic["failure_stage"] = diagnostic["stage"]
            diagnostic["failure_elapsed_ms"] = round((time.monotonic()-started)*1000, 3)
            diagnostic["trace"] = [{"file": Path(frame.filename).name[:128], "line": frame.lineno,
                                    "function": frame.name[:128]}
                                   for frame in traceback.extract_tb(error.__traceback__, limit=-12)]
            diagnostic["ku_control_context"] = control_context(error)
            diagnostic["owned_before_cleanup"] = owned_receipt()
            diagnostic["proxy_before_cleanup"] = (None if proxy is None else
                {"pid": proxy.pid, "returncode": proxy.poll()})
            if config is not None:
                # Once per fixed slot, existing one-second management reads;
                # diagnostic failures never erase the original test failure.
                for slot in config["slots"][:3]:
                    try:
                        slot_status(slot)
                    except (OSError, RuntimeError, ValueError, http.client.HTTPException) as probe_error:
                        diagnostic["slot_last_status"].setdefault(slot["name"], {}).update(
                            diagnostic_error=bounded_text(f"{type(probe_error).__name__}: {probe_error}"))
        finally:
            cleanup_errors = []
            remember_owned()
            if config is not None:
                for slot in config["slots"]:
                    try:
                        M.stop(config, slot)
                    except (OSError, RuntimeError, TimeoutError, subprocess.SubprocessError) as error:
                        cleanup_errors.append(bounded_text(f"{slot['name']}: {error}"))
            if proxy is not None:
                try:
                    proxy.terminate()
                    try:
                        proxy.wait(timeout=5)
                    except subprocess.TimeoutExpired:
                        proxy.kill()
                        proxy.wait(timeout=5)
                except (OSError, subprocess.SubprocessError) as error:
                    cleanup_errors.append(bounded_text(f"proxy: {type(error).__name__}: {error}"))
            report["cleanup_errors"] = cleanup_errors
            if cleanup_errors:
                report["passed"] = False
            diagnostic["owned_after_cleanup"] = owned_receipt()
            diagnostic["proxy_after_cleanup"] = (None if proxy is None else
                {"pid": proxy.pid, "returncode": proxy.poll()})
            report["duration_seconds"] = round(time.monotonic() - started, 3)
            args.output.parent.mkdir(parents=True, exist_ok=True)
            args.output.write_text(json.dumps(report, ensure_ascii=False, indent=2) + "\n")
    print(json.dumps(report, ensure_ascii=False, indent=2))
    if not report["passed"]:
        raise SystemExit(1)


if __name__ == "__main__":
    main()
