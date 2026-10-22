#!/usr/bin/env python3
"""Bounded reference service operations; never retries a business request.

The host manager owns `ku service`; Ku alone restarts the native instance.
`start` is a local acceptance convenience, not a replacement for that manager.
All operations are serialized by one exclusive deployment lock.
"""

import argparse
import contextlib
import csv
import http.client
import io
import json
import os
from pathlib import Path
import socket
import subprocess
import sys
import time

OWNED_SUPERVISORS = {}


def load(path):
    config = json.loads(path.read_text(encoding="utf-8"))
    if len(config["slots"]) < 3 or len(config["slots"]) > 16:
        raise ValueError("reference rolling deployment requires 3..16 fixed slots")
    if len({slot["name"] for slot in config["slots"]}) != len(config["slots"]):
        raise ValueError("slot names must be unique")
    for slot in config["slots"]:
        if not slot["name"].replace("_", "").isalnum():
            raise ValueError("slot names must be alphanumeric or underscore")
        for key in ("listen_addr", "probe_addr", "control_addr"):
            host, port = slot[key].rsplit(":", 1)
            if host != "127.0.0.1" or not 0 < int(port) < 65536:
                raise ValueError("reference slot addresses must use 127.0.0.1")
    addresses = [slot[key] for slot in config["slots"] for key in ("listen_addr", "probe_addr", "control_addr")]
    if len(set(addresses)) != len(addresses) or config["frontend_addr"] in addresses:
        raise ValueError("business, runtime-probe, supervisor-control and frontend addresses must be distinct")
    capacity = config["deployment_budget"]
    maximum = capacity["max_parallel_instances"]
    if not 3 <= maximum <= len(config["slots"]):
        raise ValueError("budget must permit two active instances and a rolling spare")
    for resource in ("reserved_memory_bytes", "event_threads", "blocking_threads", "database_connections", "total_threads", "database_waiters", "database_pending_bytes"):
        per_instance = capacity["per_instance"][resource]
        extra = capacity.get("management_threads", 0) if resource == "total_threads" else 0
        if not isinstance(per_instance, int) or per_instance < 0 or maximum * per_instance + extra > capacity[resource]:
            raise ValueError(f"rolling coexistence exceeds deployment {resource}")
    per_instance = capacity["per_instance"]
    if per_instance["total_threads"] < per_instance["event_threads"] + per_instance["blocking_threads"] + 9:
        raise ValueError("total_threads must reserve native main/logger plus Ku supervisor/launcher threads")
    for slot in config["slots"]:
        if slot.get("memory_limit_bytes", 0) + capacity["supervisor_reserve_bytes"] > per_instance["reserved_memory_bytes"]:
            raise ValueError("worker hard limit plus coordinator reserve exceeds instance reservation")
    token = Path(config["control_token_file"]).read_text(encoding="ascii").strip()
    if not 32 <= len(token) <= 128 or not token.isalnum():
        raise ValueError("invalid deployment control token")
    config["_token"] = token
    config["_path"] = str(path.resolve())
    return config


def slot_named(config, name):
    return next(slot for slot in config["slots"] if slot["name"] == name)


def call(address, method, route, token=None, timeout=1):
    host, port = address.rsplit(":", 1)
    connection = http.client.HTTPConnection(host, int(port), timeout=timeout)
    started = time.monotonic()
    stage = "connect"
    try:
        connection.connect()
        stage = "request"
        headers = {"Connection": "close"}
        if token:
            headers["X-Ku-Control-Token"] = token
        connection.request(method, route, headers=headers)
        stage = "response_headers"
        response = connection.getresponse()
        stage = "response_body"
        data = response.read(8193)
        if len(data) > 8192:
            raise ValueError("bounded control response exceeded")
        return response.status, data.decode("utf-8")
    except (OSError, http.client.HTTPException) as error:
        error.ku_control_context = {
            "stage": stage, "elapsed_ms": round((time.monotonic()-started)*1000, 3),
            "error_type": type(error).__name__, "errno": getattr(error, "errno", None),
            "winerror": getattr(error, "winerror", None), "detail": str(error)[:256],
        }
        raise
    finally:
        connection.close()


def windows_control_port_absent(address):
    """Prove vacancy without connecting, listening or reusing another socket.

    Windows may still be resolving a refused loopback connect when its socket
    deadline expires. An exclusive bind succeeds only if the exact endpoint
    is free. Any failure, including lingering connections, stays inconclusive;
    it must never release a deployment reservation.
    """
    host, port = address.rsplit(":", 1)
    if host != "127.0.0.1" or not hasattr(socket, "SO_EXCLUSIVEADDRUSE"):
        return False, {"method": "windows_exclusive_bind", "result": "unavailable"}
    try:
        with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as probe:
            probe.setsockopt(socket.SOL_SOCKET, socket.SO_EXCLUSIVEADDRUSE, 1)
            probe.bind((host, int(port)))
        return True, {"method": "windows_exclusive_bind", "result": "vacant"}
    except OSError as error:
        return False, {"method": "windows_exclusive_bind", "result": "inconclusive",
                       "errno": error.errno, "winerror": getattr(error, "winerror", None),
                       "detail": str(error)[:256]}


def control_failure(slot, error):
    context = getattr(error, "ku_control_context", {
        "stage": "unknown", "error_type": type(error).__name__,
        "errno": getattr(error, "errno", None), "winerror": getattr(error, "winerror", None),
        "detail": str(error)[:256],
    })
    state = {"phase": "unreachable", "pid": None,
             "control_error": f"{type(error).__name__}: {error}"[:256],
             "control_failure": context}
    process = OWNED_SUPERVISORS.get(slot["name"])
    if process is not None and process.poll() is None:
        # A live owned child may not have bound yet. Preserve its reservation
        # even when the endpoint is not listening, and refuse duplicate start.
        state.update(pid=process.pid, owned_supervisor_alive=True)
        return state
    if isinstance(error, ConnectionRefusedError):
        state.update(phase="absent", pid=0)
    elif sys.platform == "win32":
        absent, proof = windows_control_port_absent(slot["control_addr"])
        state["control_presence"] = proof
        if absent:
            state.update(phase="absent", pid=0)
    return state


def status(config, slot):
    try:
        code, body = call(slot["control_addr"], "GET", "/status", config["_token"])
        if code == 400:
            return {"phase": "unreachable", "pid": None,
                    "control_error": f"bounded control header rejected: {body[:256]}"}
        if code != 200:
            raise RuntimeError(f"control {slot['name']} at {slot['control_addr']} response {code}: {body[:256]}")
        return json.loads(body)
    except (OSError, http.client.HTTPException) as error:
        # A failed read is not a process-exit receipt. Keep its deployment
        # reservation and refuse duplicate startup until the listener is gone.
        return control_failure(slot, error)


def command(config, slot):
    args = [config["ku"], "service", "--probe-addr", slot["probe_addr"], "--control-addr", slot["control_addr"],
            "--control-token-file", config["control_token_file"]]
    for name, value in config.get("service_options", {}).items():
        args.extend(["--" + name.replace("_", "-"), str(value)])
    if "memory_limit_bytes" in slot:
        args.extend(["--worker-memory-bytes", str(slot["memory_limit_bytes"]),
                     "--total-memory-bytes", str(slot["memory_limit_bytes"])])
        if "cgroup_root" in slot:
            args.extend(["--cgroup-root", slot["cgroup_root"]])
    return args + ["--", slot["program"]] + slot.get("args", [])


def environment(config, slot):
    result = os.environ.copy()
    result.update(slot.get("env", {}))
    budget = config["deployment_budget"]["per_instance"]
    derived = {
        "KU_EVENT_LOOPS": budget["event_threads"],
        "KU_BLOCKING_THREADS": budget["blocking_threads"],
        "KU_THREAD_BUDGET": budget["event_threads"] + budget["blocking_threads"] + 2,
        "KU_THREAD_STACK_BYTES": config["deployment_budget"]["thread_stack_bytes"],
        "KU_DB_MAX_CONNECTIONS": budget["database_connections"],
        "KU_DB_MAX_WAITERS": budget["database_waiters"],
        "KU_DB_MAX_PENDING_BYTES": budget["database_pending_bytes"],
    }
    for name, value in derived.items():
        if name in slot.get("env", {}) and slot["env"][name] != str(value):
            raise ValueError(f"slot environment conflicts with declared budget: {name}")
        result[name] = str(value)
    return result


def write_config(config):
    path = Path(config["_path"])
    clean = {key: value for key, value in config.items() if not key.startswith("_")}
    temporary = path.with_suffix(path.suffix + ".new")
    with temporary.open("x", encoding="utf-8") as handle:
        json.dump(clean, handle, ensure_ascii=False, indent=2)
        handle.write("\n")
        handle.flush()
        os.fsync(handle.fileno())
    os.replace(temporary, path)


def wait_ready(config, slot, timeout):
    deadline = time.monotonic() + timeout
    state = None
    while time.monotonic() < deadline:
        state = status(config, slot)
        if state["phase"] == "ready":
            return state
        time.sleep(0.05)
    raise TimeoutError(f"slot {slot['name']} not ready before fixed deadline; last status: {json.dumps(state)}")


def wait_absent(config, slot, timeout):
    deadline = time.monotonic() + timeout
    state = None
    while time.monotonic() < deadline:
        state = status(config, slot)
        if state["phase"] == "absent":
            return
        time.sleep(0.05)
    raise TimeoutError(f"slot {slot['name']} has not retired; last status: {json.dumps(state)}")


def start(config, slot):
    state = status(config, slot)
    if state["phase"] != "absent":
        raise RuntimeError(f"slot {slot['name']} already has a supervisor or cannot prove absence; status: {json.dumps(state)}")
    active = sum(status(config, candidate)["phase"] != "absent" for candidate in config["slots"])
    if active >= config["deployment_budget"]["max_parallel_instances"]:
        raise RuntimeError("deployment coexistence limit reached")
    if "manager_start" in slot:
        subprocess.run(slot["manager_start"], check=True, timeout=10)
    else:
        # Deliberately no automatic supervisor restart: use the supplied host
        # units for production ownership, with finite host restart intensity.
        OWNED_SUPERVISORS[slot["name"]] = subprocess.Popen(
            command(config, slot), env=environment(config, slot),
            stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL,
            stderr=config.get("_supervisor_stderr", {}).get(slot["name"], subprocess.DEVNULL))
    try:
        return wait_ready(config, slot, config["operation_timeout_seconds"])
    except BaseException:
        try:
            stop(config, slot)
        except (OSError, RuntimeError, TimeoutError):
            pass
        raise


def stop(config, slot):
    deadline = time.monotonic() + config["operation_timeout_seconds"]
    while True:
        state = status(config, slot)
        if state["phase"] == "absent":
            break
        if time.monotonic() >= deadline:
            raise TimeoutError(f"slot {slot['name']} has not retired; last status: {json.dumps(state)}")
        try:
            code, _ = call(slot["control_addr"], "POST", "/stop", config["_token"],
                           timeout=min(1, max(.01, deadline-time.monotonic())))
        except (OSError, http.client.HTTPException):
            code = None
        if code == 202:
            wait_absent(config, slot, max(0, deadline-time.monotonic()))
            break
        if code is not None and code != 400:
            raise RuntimeError(f"slot stop returned {code}")
        # Repeating this management-only stop is idempotent. No HTTP business
        # operation or SQL is replayed and the original deadline never moves.
        time.sleep(min(.05, max(0, deadline-time.monotonic())))
    if "manager_stop" in slot:
        subprocess.run(slot["manager_stop"], check=True, timeout=10)
    if process := OWNED_SUPERVISORS.pop(slot["name"], None):
        process.wait(timeout=config["operation_timeout_seconds"])


def proxy_command(config, command):
    deadline = time.monotonic() + 1
    result = bytearray()
    with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as stream:
        stream.settimeout(1)
        stream.connect(config["haproxy_runtime_socket"])
        stream.sendall(command.encode("ascii") + b"\n")
        stream.shutdown(socket.SHUT_WR)
        while True:
            remaining = deadline - time.monotonic()
            if remaining <= 0 or len(result) >= 256 * 1024:
                raise TimeoutError("bounded HAProxy Runtime API response")
            stream.settimeout(remaining)
            chunk = stream.recv(min(4096, 256 * 1024 - len(result)))
            if not chunk:
                return result.decode("utf-8")
            result.extend(chunk)


def proxy_state(config, slot, state):
    # HAProxy's local Runtime API changes admission before draining. No request
    # replay, redispatch or application-aware retry is performed here.
    response = proxy_command(config, f"set server ku/{slot['name']} state {state}")
    if response.strip():
        raise RuntimeError(f"HAProxy Runtime API: {response}")


def wait_proxy_ready(config, slot):
    deadline = time.monotonic() + config["operation_timeout_seconds"]
    while time.monotonic() < deadline:
        rows = csv.DictReader(io.StringIO(proxy_command(config, "show stat").lstrip("# ")))
        if any(row.get("pxname") == "ku" and row.get("svname") == slot["name"]
               and row.get("status") == "UP" for row in rows):
            return
        time.sleep(.05)
    raise TimeoutError(f"HAProxy has not admitted {slot['name']}; old instance retained")


def roll(config, old_name, new_name, program, rollback=False, on_coexist=None):
    old, new = slot_named(config, old_name), slot_named(config, new_name)
    if old is new or status(config, old)["phase"] != "ready" or status(config, new)["phase"] != "absent":
        raise RuntimeError("rolling requires a ready old slot and an absent distinct spare")
    if sum(status(config, item)["phase"] == "ready" for item in config["slots"]) < 2:
        raise RuntimeError("rolling requires another ready instance to preserve capacity")
    program = str(Path(program).resolve(strict=True))
    previous = new["program"]
    new["program"] = program
    write_config(config)
    try:
        start(config, new)
        proxy_state(config, new, "ready")
        wait_proxy_ready(config, new)
        if on_coexist is not None:
            on_coexist()
        code, body = call(config["frontend_addr"], "GET", config["smoke_path"])
        if code != 200 or config["smoke_expected"] not in body:
            raise RuntimeError("proxy smoke did not match the declared result")
        proxy_state(config, old, "maint")
        stop(config, old)
    except BaseException:
        # Never replay business traffic. If the old instance has already left,
        # preserve the surviving instances and fail visibly for operator action.
        if status(config, old)["phase"] == "ready":
            proxy_state(config, old, "ready")
            stop(config, new)
            new["program"] = previous
            write_config(config)
        raise
    config["last_deployment"] = {"old_slot": old["name"], "new_slot": new["name"], "old_program": old["program"], "new_program": program}
    if rollback:
        config["last_deployment"]["was_rollback"] = True
    write_config(config)
    return config["last_deployment"]


def render(config):
    token = config["_token"]
    result = f"""global
    maxconn 4096
    nbthread 1
    stats socket {config['haproxy_runtime_socket']} mode 600 level admin
defaults
    mode http
    retries 0
    no option redispatch
    timeout connect 1s
    timeout client 5s
    timeout server 5s
    timeout http-request 2s
    timeout http-keep-alive 2s
frontend public
    bind {config['frontend_addr']}
    http-request deny deny_status 404 if {{ path_beg /__ku/ }}
    http-request del-header X-Ku-Control-Token
    default_backend ku
backend ku
    balance roundrobin
    option httpchk
    http-check send meth GET uri /status ver HTTP/1.1 hdr Host localhost hdr X-Ku-Control-Token {token}
    http-check expect rstring phase...ready
"""
    for slot in config["slots"]:
        check_port = slot["control_addr"].rsplit(":", 1)[1]
        result += f"    server {slot['name']} {slot['listen_addr']} check port {check_port} inter 250ms fall 2 rise 1 maxconn 1024 maxqueue 32\n"
    return result


@contextlib.contextmanager
def locked(path):
    lock = path.with_suffix(path.suffix + ".lock")
    fd = os.open(lock, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
    try:
        os.write(fd, str(os.getpid()).encode("ascii"))
        yield
    finally:
        os.close(fd)
        lock.unlink()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("action", choices=["check", "render", "run", "start", "status", "stop", "roll", "rollback"])
    parser.add_argument("config", type=Path)
    parser.add_argument("slots", nargs="*")
    parser.add_argument("--program")
    args = parser.parse_args()
    config = load(args.config)
    if args.action == "render":
        print(render(config), end="")
        return
    if args.action == "run":
        slot = slot_named(config, args.slots[0])
        if os.name == "nt":
            args_list = command(config, slot)
            args_list.insert(args_list.index("--"), "--watch-stdin")
            process = subprocess.Popen(args_list, env=environment(config, slot), stdin=subprocess.PIPE)
            try:
                result = process.wait()
            finally:
                process.stdin.close()
                try:
                    process.wait(timeout=config["operation_timeout_seconds"])
                except subprocess.TimeoutExpired:
                    process.kill()  # closes the supervisor's native Job handle
                    process.wait(timeout=5)
            raise SystemExit(result)
        os.execvpe(config["ku"], command(config, slot), environment(config, slot))
    with locked(args.config):
        if args.action == "check":
            print(json.dumps({"valid": True, "deployment_budget": config["deployment_budget"]}))
        elif args.action == "status":
            print(json.dumps({slot["name"]: status(config, slot) for slot in config["slots"]}, indent=2))
        elif args.action in ("start", "stop"):
            print(json.dumps(globals()[args.action](config, slot_named(config, args.slots[0]))))
        elif args.action == "roll":
            if len(args.slots) != 2 or not args.program:
                parser.error("roll requires OLD_SLOT NEW_SLOT --program PATH")
            print(json.dumps(roll(config, *args.slots, args.program)))
        elif args.action == "rollback":
            prior = config["last_deployment"]
            print(json.dumps(roll(config, prior["new_slot"], prior["old_slot"], prior["old_program"], rollback=True)))


if __name__ == "__main__":
    main()
