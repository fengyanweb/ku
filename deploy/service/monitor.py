#!/usr/bin/env python3
"""Bounded foreground service observations; emits alerts, never changes service state."""
import argparse
import collections
import csv
import io
import json
import math
import os
from pathlib import Path
import platform
import re
import signal
import subprocess
import time
import importlib.util
_SPEC = importlib.util.spec_from_file_location('ku_monitor_manage', Path(__file__).with_name('manage.py'))
M = importlib.util.module_from_spec(_SPEC); _SPEC.loader.exec_module(M)
MAX_SNAPSHOT = 262144
MAX_PIDS = 256
MAX_CGROUPS = 64
POLICY = {"draft_requires_application_contract": True, "automatic_actions": False,
    "collection_unavailable": {"minimum_samples": 1, "meaning": "unknown, not zero"},
    "invalid_metrics": {"minimum_samples": 1}, "oom": {"new_event_delta": 1},
    "restart_storm": {"restarts": 3, "window_seconds": 60},
    "FD_growth": {"minimum_samples": 3, "additional_FDs": 32, "ratio": 1.5, "draft_only": True},
    "queue_not_retiring": {"minimum_samples": 3, "minimum_seconds": 10, "condition": "positive and nondecreasing"},
    "observed_limit_exceeded": {"minimum_samples": 1, "meaning": "observed charged metric; not a claim kernel hard-limit accounting is RSS"},
    "business_error_rate_and_latency": "unknown unless separately supplied by application/load measurements; no p99 invented from low counts"}
PHASES = {"starting", "ready", "unready", "draining", "backoff", "stopped", "absent", "unreachable"}
SECTIONS = ("limits", "loops", "outcomes", "transport_closes", "buffer", "database", "tasks", "task_driver", "task_driver_timing", "task_blocking", "output")
def numbers(value, limit=64):
    if not isinstance(value, dict): return None
    return {key: item for key, item in list(value.items())[:limit]
            if isinstance(key, str) and not any(word in key.lower() for word in ('token', 'password', 'secret', 'payload', 'sql', 'environment')) and re.fullmatch(r"[a-zA-Z_][a-zA-Z_0-9]{0,63}", key)
            and (item is None or type(item) in (int, bool) or type(item) is float and math.isfinite(item))}
def safe_metrics(raw):
    if not isinstance(raw, dict): return {"available": False, "native": None}
    result = numbers(raw) or {}; result["available"] = raw.get("available") is True
    native = raw.get("native"); result["native"] = None
    if result["available"] and not isinstance(native, dict): result["available"] = False
    if result["available"] and isinstance(native, dict):
        kept = numbers(native) or {}
        for key in SECTIONS:
            value = native.get(key)
            if isinstance(value, list): kept[key] = [numbers(row) for row in value[:32]]
            elif isinstance(value, dict): kept[key] = numbers(value)
        # Fixed diagnostic counts only; never error samples, SQL, RID or bodies.
        if isinstance(native.get("errors"), dict): kept["errors"] = numbers(native["errors"].get("counts"))
        result["native"] = kept
    if isinstance(raw.get('error_type'), str) and re.fullmatch(r'[A-Za-z_]{1,64}', raw['error_type']): result['error_type'] = raw['error_type']
    return result
def status_once(config, slot):
    if not hasattr(signal, 'setitimer'): return {'available': False, 'phase': 'unknown', 'error_type': 'DeadlineUnsupported'}
    previous = signal.getsignal(signal.SIGALRM)
    def expired(_signal, _frame): raise TimeoutError('status collection deadline')
    signal.signal(signal.SIGALRM, expired); signal.setitimer(signal.ITIMER_REAL, 1)
    try: return M.status(config, slot)
    finally: signal.setitimer(signal.ITIMER_REAL, 0); signal.signal(signal.SIGALRM, previous)
def read_small(path, cap=8192, encoding="ascii"):
    with Path(path).open("rb") as stream: data = stream.read(cap+1)
    if len(data) > cap: raise ValueError("bounded observation file exceeded")
    return data.decode(encoding, errors="replace" if encoding == "utf-8" else "strict")
def pairs(path):
    return {key: int(value) for key, value in (line.split() for line in read_small(path).splitlines())}
def value(path):
    text = read_small(path).strip()
    return text if text == "max" else int(text)
def proc_sample(pid):
    def stat():
        text = read_small(f"/proc/{pid}/stat", 16384, encoding="utf-8"); fields = text[text.rfind(")")+2:].split()
        return {"pid": pid, "ppid": int(fields[1]), "starttime_ticks": int(fields[19]),
                "user_cpu_ticks": int(fields[11]), "system_cpu_ticks": int(fields[12]),
                "rss_bytes": int(fields[21])*os.sysconf("SC_PAGESIZE"), "threads": int(fields[17]),
                "minor_faults": int(fields[7]), "major_faults": int(fields[9])}
    first = stat(); count = 0; fd_error = None
    try:
        with os.scandir(f"/proc/{pid}/fd") as entries:
            for _ in entries:
                count += 1
                if count > 8192: break
    except OSError as error: fd_error = type(error).__name__
    last = stat()
    if (first["starttime_ticks"], first["ppid"]) != (last["starttime_ticks"], last["ppid"]):
        raise ValueError("process identity changed during observation")
    first.update(fd_count=count if count <= 8192 and fd_error is None else None, fd_count_truncated=count > 8192, fd_error_type=fd_error)
    return first
def cgroup_sample(path):
    root = Path(path).resolve(strict=True)
    if root == Path("/sys/fs/cgroup") or not root.is_relative_to(Path("/sys/fs/cgroup")):
        raise ValueError("resource cgroup must be a dedicated child of /sys/fs/cgroup")
    row = {"memory_current": value(root/"memory.current"), "memory_max": value(root/"memory.max"),
           "pids_current": value(root/"pids.current"), "pids_max": value(root/"pids.max"),
           "cpu_stat": pairs(root/"cpu.stat"), "memory_events": pairs(root/"memory.events")}
    row['cgroup_inode'] = root.stat().st_ino; row['cgroup_events'] = pairs(root/'cgroup.events')
    quota, period = read_small(root/'cpu.max').split(); row['cpu_max'] = {'quota_usec': None if quota=='max' else int(quota), 'period_usec': int(period)}
    pids = set(); directories = 0; pending = [root]
    while pending:
        current = pending.pop(); directories += 1
        pids.update(int(pid) for pid in read_small(current/"cgroup.procs").split())
        if len(pids) > MAX_PIDS: raise ValueError("unit PID observation bound exceeded")
        with os.scandir(current) as entries:
            for entry in entries:
                if entry.is_dir(follow_symlinks=False):
                    if directories+len(pending) >= MAX_CGROUPS: raise ValueError("cgroup subtree observation bound exceeded")
                    pending.append(Path(entry.path))
    row.update(pids=sorted(pids), subtree_directories=directories)
    return row
class Log:
    def __init__(self, directory, max_bytes, files):
        self.root = directory; self.max_bytes = max_bytes; self.files = files
        self.counts = [0]*files; self.size = self.dropped = self.evicted = 0
    def append(self, record):
        raw = (json.dumps(record, ensure_ascii=True, allow_nan=False)+"\n").encode()
        if len(raw) > min(MAX_SNAPSHOT, self.max_bytes):
            self.dropped += 1
            raw = (json.dumps({"kind": "snapshot_dropped", "sequence": record.get("sequence"),
                "reason": "snapshot_byte_bound", "original_bytes": len(raw), "dropped_samples": self.dropped})+"\n").encode()
        if self.size+len(raw) > self.max_bytes:
            oldest = self.root/f"metrics.{self.files-1}.jsonl"
            if oldest.exists(): oldest.unlink(); self.evicted += self.counts[-1]
            for i in range(self.files-1, 0, -1):
                old = self.root/f"metrics.{i-1}.jsonl"
                if old.exists(): old.replace(self.root/f"metrics.{i}.jsonl")
            self.counts = [0]+self.counts[:-1]; self.size = 0
        with (self.root/"metrics.0.jsonl").open("ab") as stream: stream.write(raw)
        self.size += len(raw); self.counts[0] += 1
class Receipt:
    """Linux live collector identity only; never a business success receipt."""
    def __init__(self, state, config, output, continuous, interval, module=None):
        if platform.system() != 'Linux': raise ValueError('live RC observer identity requires Linux /proc')
        if module is None:
            here = Path(__file__).resolve().parent
            source = here/'production_rc.py' if (here/'production_rc.py').is_file() else here.parent/'production_rc.py'
            spec = importlib.util.spec_from_file_location('ku_monitor_rc', source)
            module = importlib.util.module_from_spec(spec); spec.loader.exec_module(module)
        self.rc = module; _, manifest, _ = module.state(state)
        process = Path('/proc')/str(os.getpid())
        first = int(read_small(process/'stat', 16384, encoding='utf-8').rsplit(') ', 1)[1].split()[19])
        with (process/'cmdline').open('rb') as stream: command = stream.read(65537)
        if len(command)>65536: raise ValueError('observer command identity bound')
        import hashlib
        identity = {'pid': os.getpid(), 'start_ticks': first, 'cmdline_sha256': hashlib.sha256(command).hexdigest(),
                    'executable_sha256': module.digest(Path(os.readlink(process/'exe')))}
        if int(read_small(process/'stat', 16384, encoding='utf-8').rsplit(') ', 1)[1].split()[19]) != first: raise ValueError('observer PID identity changed')
        self.record = {'schema': 'ku-production-observer-v1', 'binding': module.binding(manifest),
            'config_sha256': module.digest(config), 'target_machine_id_sha256': module.digest(Path('/etc/machine-id')),
            'identity': identity, 'continuous_collection_running': False, 'last_sample_clock_monotonic_ns': None}
        self.path = output/'observer.json'; self.continuous = continuous; self.interval = interval
    def publish(self, running, row=None):
        self.record['continuous_collection_running'] = bool(self.continuous and running and row is not None
            and row['missed_intervals']==0 and row['collection_elapsed_ms']<=self.interval*1000)
        if row is not None: self.record['last_sample_clock_monotonic_ns'] = time.clock_gettime_ns(time.CLOCK_MONOTONIC)
        raw = (json.dumps(self.record, sort_keys=True)+'\n').encode()
        if len(raw)>8192: raise ValueError('observer receipt bound')
        temporary = self.path.with_suffix('.json.new'); temporary.write_bytes(raw); temporary.replace(self.path)
class Monitor:
    def __init__(self, config, output, max_bytes, files):
        self.config = config; self.output = output; self.log = Log(output, max_bytes, files)
        self.previous = {}; self.queues = {}; self.restart_events = {}; self.fds = {}; self.sequence = 0
        self.manifest = {"state": "running", "policy": POLICY, "samples_written": 0,
            "sample_gaps": 0, "read_errors": 0, "automatic_actions": False,
            "log_max_bytes_each": max_bytes, "log_max_files": files,
            "snapshot_max_bytes": min(MAX_SNAPSHOT, max_bytes), "metadata_max_bytes": 65536,
            "RSS_and_cgroup_separate": True, "RSS_sum_contains_shared_pages": True,
            "driver_CPU_not_service_CPU": "only declared cgroup members; unit CPU includes every unit process, native CPU separately identified",
            "business_p99_and_correctness": "not observed by this collector"}
    def save(self):
        self.manifest.update(dropped_samples=self.log.dropped, retention_evicted_samples=self.log.evicted)
        raw = (json.dumps(self.manifest, sort_keys=True)+"\n").encode()
        if len(raw) > 16384: raise ValueError("monitor metadata bound")
        temporary = self.output/"run.json.new"; temporary.write_bytes(raw); temporary.replace(self.output/"run.json")
    def alert(self, alerts, kind, scope, **counts):
        if len(alerts) < 64: alerts.append({"kind": kind, "scope": scope, **counts})
    def sample(self, scheduled, skipped):
        now = time.monotonic(); self.sequence += 1; self.manifest["sample_gaps"] += skipped
        row = {"kind": "snapshot", "sequence": self.sequence, "unix_ns": time.time_ns(),
               "monotonic": now, "scheduled_monotonic": scheduled, "missed_intervals": skipped,
               "start_lateness_ms": max(0, now-scheduled)*1000, "slots": {}, "alerts": []}
        launchers = {}; alerts = row["alerts"]
        for slot in self.config["slots"]:
            name = slot["name"]
            try:
                status = status_once(self.config, slot); state = numbers(status) or {}
                state["phase"] = status.get("phase") if status.get("phase") in PHASES else "unknown"
            except Exception as error: state = {"available": False, "error_type": type(error).__name__}
            try: metrics = safe_metrics(M.metrics(self.config, slot))
            except Exception as error: metrics = {'available': False, 'native': None, 'error_type': type(error).__name__}
            row["slots"][name] = {"status": state, "metrics": metrics}
            consistent = metrics.get('generation') == state.get('generation') and type(state.get('generation')) is int
            row['slots'][name]['generation_consistent'] = consistent
            if metrics['available'] and not consistent: self.alert(alerts, 'generation_changed_during_collection_unknown', name)
            if consistent and type(state.get("pid")) is int and state["pid"] > 0: launchers[state["pid"]] = name
            if not metrics["available"]: self.alert(alerts, "metrics_unavailable_unknown", name)
            previous = self.previous.get(name, {})
            if state.get('generation') != previous.get('generation'):
                self.queues = {key: history for key, history in self.queues.items() if key[0] != name}
            if type(state.get("restarts")) is int and type(previous.get("restarts")) is int:
                delta = state["restarts"]-previous["restarts"]
                if delta > 0: self.alert(alerts, "restarts_observed", name, delta=delta)
                events = self.restart_events.setdefault(name, collections.deque(maxlen=64))
                if delta > 0: events.append((now, delta))
                while events and events[0][0] < now-60: events.popleft()
                if sum(item[1] for item in events) >= 3: self.alert(alerts, "restart_storm_draft", name)
            self.previous[name] = state
            native = metrics.get("native") or {}
            invalid = [key for key, data in native.items() if isinstance(data, dict)
                       and (data.get("valid") is False or data.get("metrics_valid") is False or data.get("pg_client_metrics_valid") is False)]
            if native.get("valid") is False or native.get("metrics_valid") is False: invalid.append("native")
            if invalid: self.alert(alerts, "invalid_metrics_unknown", name, sections=invalid)
            def total(section, key):
                values = native.get(section)
                if not isinstance(values, list) or not values or not all(isinstance(x, dict) and type(x.get(key)) is int for x in values): return None
                return sum(x[key] for x in values if isinstance(x, dict))
            queue_values = {} if "native" in invalid else {"http_pending": total("loops", "pending"), "task_ready": total("tasks", "ready"),
                            "blocking_queued": (native.get("task_blocking") or {}).get("queued"),
                            "database_pg_client_waiters": (native.get("database") or {}).get("pg_client_waiters")}
            for key, current in queue_values.items():
                if key.startswith("database") and "database" in invalid: continue
                history = self.queues.setdefault((name, key), collections.deque(maxlen=3)); history.append((now, current))
                if (len(history) == 3 and history[-1][0]-history[0][0] >= 10
                        and all(type(x[1]) is int and x[1] > 0 for x in history)
                        and history[0][1] <= history[1][1] <= history[2][1]):
                    self.alert(alerts, "queue_not_retiring_draft", name, queue=key, observations=3, current=current)
        groups = {}; pids = set()
        if platform.system() == "Linux":
            for name, path in self.config.get("resource_cgroups", {}).items():
                try:
                    data = cgroup_sample(path); groups[name] = data; pids.update(data["pids"])
                    for metric in ("memory", "pids"):
                        current, maximum = data[metric+"_current"], data[metric+"_max"]
                        if type(maximum) is int and current > maximum: self.alert(alerts, "observed_limit_exceeded", name, metric=metric, current=current, maximum=maximum)
                    key = ("oom", str(Path(path).resolve())); count = data["memory_events"].get("oom_kill")
                    identity_key = ('cgroup_identity', str(Path(path).resolve())); identity = data['cgroup_inode']
                    previous = self.previous.get(key)
                    if self.previous.get(identity_key, identity) != identity: self.alert(alerts, 'cgroup_identity_reset_unknown', name); previous = None
                    self.previous[identity_key] = identity; self.previous[key] = count
                    if type(count) is int and previous is None and count > 0: self.alert(alerts, 'historical_oom_total', name, total=count)
                    if type(count) is int and type(previous) is int and count < previous: self.alert(alerts, 'cgroup_counter_reset_unknown', name)
                    if type(count) is int and type(previous) is int and count > previous: self.alert(alerts, "oom_kill_observed", name, delta=count-previous)
                except Exception as error: groups[name] = {"available": False, "error_type": type(error).__name__}
            if not groups: groups["collection"] = {"available": False, "reason": "resource_cgroups_not_configured"}
            row['processes_truncated'] = len(pids)>MAX_PIDS; row['unit_PID_count_observed'] = len(pids)
            if row['processes_truncated']: self.alert(alerts, 'PID_observation_bound_unknown', 'unit')
            processes = []
            for pid in sorted(pids)[:MAX_PIDS]:
                try: processes.append(proc_sample(pid))
                except Exception as error: processes.append({"pid": pid, "available": False, "error_type": type(error).__name__})
            by_pid = {item["pid"]: item for item in processes}
            for item in processes:
                pid, parent = item["pid"], item.get("ppid")
                item["role"] = "launcher" if pid in launchers else ("native" if parent in launchers else "unit_process")
            for pid in launchers:
                parent = by_pid.get(pid, {}).get('ppid')
                if parent in by_pid: by_pid[parent]['role'] = 'supervisor' 
            row["processes"] = processes; row["cpu_ticks_per_second"] = os.sysconf("SC_CLK_TCK")
        else:
            groups["collection"] = {"available": False, "reason": "OS_hard_domain_unsupported_by_this_platform_collector"}
            try:
                raw = subprocess.check_output(['ps', '-axo', 'pid=,ppid=,lstart=,rss=,time='], text=True, timeout=1)
                listing = {}
                for line in raw.splitlines():
                    fields = line.split()
                    if len(fields) == 9:
                        listing[int(fields[0])] = {'pid': int(fields[0]), 'ppid': int(fields[1]), 'start_ps_second_precision': ' '.join(fields[2:7]), 'rss_bytes': int(fields[7])*1024, 'CPU_time_ps': fields[8] if re.fullmatch(r'[0-9:.\-]+', fields[8]) else None, 'threads': None, 'fd_count': None}
                selected = set(launchers)|{listing[pid]['ppid'] for pid in launchers if pid in listing}
                for _ in range(8): selected |= {pid for pid, item in listing.items() if item['ppid'] in selected}
                row['processes'] = [listing[pid] for pid in sorted(selected)[:MAX_PIDS] if pid in listing]
                row['processes_truncated'] = len(selected)>MAX_PIDS
                row['process_identity_limit'] = 'macOS ps start time has1second precision; no cgroup/precise thread/FD guarantee'
            except Exception as error: row['processes'] = None; row['process_error_type'] = type(error).__name__
        live = {(item['pid'], item.get('starttime_ticks')) for item in row.get('processes') or []}; self.fds = {key: data for key, data in self.fds.items() if key in live}
        for item in row.get('processes') or []:
            if type(item.get('fd_count')) is not int: continue
            history = self.fds.setdefault((item['pid'], item.get('starttime_ticks')), collections.deque(maxlen=3)); history.append(item['fd_count'])
            if len(history)==3 and history[-1]>history[0]+32 and history[-1]>history[0]*1.5: self.alert(alerts, 'FD_growth_draft', 'unit', pid=item['pid'], first=history[0], current=history[-1])
        row["cgroups"] = groups; row['cgroup_metrics_not_summed'] = True
        observed = row.get('processes')
        row['sum_observed_RSS_bytes'] = sum(item['rss_bytes'] for item in observed) if observed and all('rss_bytes' in item for item in observed) else None
        if self.config.get("proxy_kind", "haproxy") == "haproxy":
            try:
                raw = M.proxy_command(self.config, "show stat"); header, *lines = raw.splitlines()
                reader = csv.DictReader(io.StringIO("\n".join([header.lstrip("# ")]+lines)))
                accepted = {slot["name"] for slot in self.config["slots"]}|{"FRONTEND", "BACKEND"}; proxy = []
                for item in reader:
                    if item.get("svname") not in accepted or item.get("pxname") not in {"public", "ku"}: continue
                    record = {key: int(item[key]) if item.get(key, "").isdigit() else None
                              for key in ("qcur", "scur", "stot", "econ", "eresp", "hrsp_5xx")}
                    record.update(proxy=item["pxname"], slot=item["svname"], status=item.get("status") if item.get("status") in {"UP", "DOWN", "MAINT", "DRAIN", "OPEN", "FULL", "no check"} else "unknown")
                    proxy.append(record)
                    if len(proxy) >= 32: break
                row["proxy"] = {"available": True, "rows": proxy}
            except Exception as error: row["proxy"] = {"available": False, "error_type": type(error).__name__}
        else: row["proxy"] = {"available": False, "reason": "HAProxy_runtime_only"}
        row["collection_elapsed_ms"] = (time.monotonic()-now)*1000
        unavailable = sum(not item['metrics']['available'] for item in row['slots'].values())
        row['unavailable_metric_observations'] = unavailable
        self.manifest['read_errors'] += sum(bool(item.get('error_type') or item.get('fd_error_type')) for item in row.get('processes') or []) + unavailable + sum('error_type' in group for group in groups.values()) + int(not row.get('proxy', {}).get('available', False))
        self.log.append(row); self.manifest["samples_written"] += 1; self.save(); return row
def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--config", required=True, type=Path); parser.add_argument("--output-dir", required=True, type=Path)
    parser.add_argument("--state", type=Path, help="optional bound Linux RC state; receipt alone does not pass business checks")
    parser.add_argument("--interval", type=float, default=5); parser.add_argument("--samples", type=int, default=12, help="0 explicitly runs until interrupted")
    parser.add_argument("--max-log-bytes", type=int, default=1048576); parser.add_argument("--max-files", type=int, default=3)
    args = parser.parse_args()
    if not 1 <= args.interval <= 3600 or args.samples < 0 or not 4096 <= args.max_log_bytes <= 16777216 or not 1 <= args.max_files <= 10:
        parser.error("interval1..3600, samples>=0, log bytes4096..16777216, files1..10 required")
    if args.config.stat().st_size > 65536: parser.error('monitor configuration exceeds65536bytes')
    config = M.load(args.config)
    if len(config.get('resource_cgroups', {}))>16 or any(len(slot['name'])>64 for slot in config['slots']): parser.error('monitor scope exceeds fixed bound')
    args.output_dir.mkdir(mode=0o700, parents=False, exist_ok=False)
    monitor = Monitor(config, args.output_dir, args.max_log_bytes, args.max_files)
    receipt = Receipt(args.state, args.config, args.output_dir, args.samples==0, args.interval) if args.state else None
    if receipt: receipt.publish(False)
    (args.output_dir/"policy.json").write_text(json.dumps(POLICY, indent=2)+"\n")
    def stop(_signal, _frame): raise KeyboardInterrupt
    prior = {sig: signal.signal(sig, stop) for sig in (signal.SIGTERM, signal.SIGINT)}
    monitor.save(); scheduled = time.monotonic()
    try:
        while args.samples == 0 or monitor.sequence < args.samples:
            time.sleep(max(0, scheduled-time.monotonic())); skipped = max(0, int((time.monotonic()-scheduled)//args.interval))
            row = monitor.sample(scheduled, skipped); scheduled += (skipped+1)*args.interval
            if receipt: receipt.publish(True, row)
        monitor.manifest["state"] = "finished"
    except KeyboardInterrupt: monitor.manifest["state"] = "interrupted"
    except Exception as error: monitor.manifest.update(state="failed", error_type=type(error).__name__)
    finally:
        if receipt: receipt.publish(False)
        for sig, handler in prior.items(): signal.signal(sig, handler)
        monitor.save()
    print(json.dumps(monitor.manifest, sort_keys=True))
    return 1 if monitor.manifest["state"] == "failed" else 0
if __name__ == "__main__": raise SystemExit(main())
