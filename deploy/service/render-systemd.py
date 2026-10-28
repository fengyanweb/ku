#!/usr/bin/env python3
"""Render bound Linux RC units, without installing, enabling or starting them."""
from __future__ import annotations

import argparse
import hashlib
import importlib.util
import json
import math
import os
from pathlib import Path
import platform
import re
import stat
import sys
import time

try:
    import grp
    import pwd
except ImportError:  # Importable for parser tests on other platforms.
    grp = pwd = None

sys.dont_write_bytecode = True
HERE = Path(__file__).resolve().parent
PREFIX = re.compile(r"ku-rc-[a-z0-9][a-z0-9-]{0,47}\Z")
ACCOUNT = re.compile(r"[A-Za-z_][A-Za-z0-9_-]{0,31}\Z")
SLOT = re.compile(r"[A-Za-z0-9_]{1,32}\Z")
MARKER = re.compile(r"@[A-Z_]+@")


class RenderError(ValueError):
    pass


def digest(path):
    result = hashlib.sha256()
    with Path(path).open("rb") as source:
        while chunk := source.read(1024 * 1024):
            result.update(chunk)
    return result.hexdigest()


def clean(value):
    if (not isinstance(value, str) or not value or value != value.strip()
            or any(ord(c) < 32 or ord(c) == 127 for c in value)
            or any(c in value for c in ('%', '$', '"', '\\'))):
        raise RenderError("unit input contains unsupported control/expansion/quote characters")
    return value


def argument(value):
    # ExecStart is an argv list, not a shell. Quote every atom, including spaces.
    return '"' + clean(str(value)) + '"'


def regular(path, executable=False):
    path = Path(clean(str(path)))
    if not path.is_absolute() or path.is_symlink():
        raise RenderError("associated paths must be absolute regular files, not symlinks")
    path = path.resolve(strict=True)
    info = path.stat()
    if not stat.S_ISREG(info.st_mode) or (executable and not info.st_mode & 0o111):
        raise RenderError(f"associated file is not regular/executable: {path}")
    return path


def readable_by(path, account, groups, execute=False):
    path = Path(path)
    def permission(entry, wanted):
        info = entry.stat()
        shift = 6 if info.st_uid == account.pw_uid else 3 if info.st_gid in groups else 0
        return ((info.st_mode >> shift) & wanted) == wanted
    if not permission(path, 4 | (1 if execute else 0)) or any(not permission(p, 1) for p in path.parents):
        raise RenderError(f"deployment user cannot traverse/read/execute required path by Unix mode bits: {path}")


def load_manager():
    path = HERE / "manage.py"
    spec = importlib.util.spec_from_file_location("ku_rc_systemd_manage", path)
    if spec is None or spec.loader is None:
        raise RenderError("matching trusted manage.py is missing")
    module = importlib.util.module_from_spec(spec); spec.loader.exec_module(module)
    return module, path


def json_object(path):
    if path.stat().st_size > 2 * 1024 * 1024:
        raise RenderError("configuration exceeds 2MiB")
    def unique(pairs):
        result = {}
        for key, value in pairs:
            if key in result:
                raise RenderError("duplicate configuration field")
            result[key] = value
        return result
    def invalid(_):
        raise RenderError("non-finite configuration number")
    result = json.loads(path.read_text(), object_pairs_hook=unique, parse_constant=invalid)
    if not isinstance(result, dict):
        raise RenderError("configuration root must be an object")
    return result


def render(args):
    if platform.system() != "Linux" or pwd is None or grp is None:
        raise RenderError("render against the actual Linux target account and file permissions")
    if not PREFIX.fullmatch(args.unit_prefix) or not ACCOUNT.fullmatch(args.user):
        raise RenderError("invalid unit prefix or service account name")
    account = pwd.getpwnam(args.user)
    if account.pw_uid == 0:
        raise RenderError("RC service must use an existing non-root account")
    group = grp.getgrgid(account.pw_gid).gr_name
    if not ACCOUNT.fullmatch(group):
        raise RenderError("unsupported service primary group name")
    groups = set(os.getgrouplist(args.user, account.pw_gid))
    limits = {"memory_max_bytes": args.memory_max_bytes, "cpu_quota_percent": args.cpu_quota_percent,
              "tasks_max": args.tasks_max, "nofile": args.nofile, "timeout_stop_seconds": args.timeout_stop_seconds}
    if any(type(value) is not int or value <= 0 for value in limits.values()):
        raise RenderError("resource limits must be positive integers")
    if (not 1024 * 1024 <= args.memory_max_bytes <= 2**50 or args.tasks_max > 1000000
            or args.nofile > 1048576 or args.cpu_quota_percent > max(100, (os.cpu_count() or 1) * 100)):
        raise RenderError("resource limits exceed this renderer's supported target bounds")
    config_path = regular(args.config)
    initial_config_sha = digest(config_path); raw = json_object(config_path)
    # manage.command normalizes underscores to hyphens; reject both spellings.
    forbidden = {"worker-memory-bytes", "total-memory-bytes", "cgroup-root"}
    options = {key.replace("_", "-") for key in raw.get("service_options", {})}
    if forbidden & options:
        raise RenderError("do not move workers to another hard-limit domain outside the systemd unit")
    for slot in raw.get("slots", []):
        if any(key in slot for key in ("memory_limit_bytes", "cgroup_root")):
            raise RenderError("ku worker hard-limit mode cannot be combined with these whole-unit limits")
        if any("CGROUP" in key.upper() or key.upper() in ("KU_WORKER_MEMORY_BYTES", "KU_TOTAL_MEMORY_BYTES") for key in slot.get("env", {})):
            raise RenderError("slot environment may not move work out of the whole-unit resource domain")
    manager, manager_source = load_manager()
    config = manager.load(config_path)
    drain = config.get("service_options", {}).get("drain_timeout_ms", 10000)
    if type(drain) is not int or drain < 1 or args.timeout_stop_seconds < math.ceil(drain / 1000) + 5:
        raise RenderError("TimeoutStopSec must leave the original drain timeout plus 5s cleanup; business deadlines are not changed")
    per = config["deployment_budget"]["per_instance"]
    if args.memory_max_bytes > per["reserved_memory_bytes"] or args.tasks_max < per["total_threads"]:
        raise RenderError("unit memory ceiling exceeds reserved memory or task ceiling is below declared process/thread allowance")
    ku = regular(config["ku"], executable=True)
    managed = regular(args.manage or ku.parent.parent / "tools/manage.py")
    if digest(managed) != digest(manager_source):
        raise RenderError("installed manage.py differs from this renderer's trusted parser")
    token = regular(config["control_token_file"])
    if token.stat().st_uid != account.pw_uid or stat.S_IMODE(token.stat().st_mode) & 0o077:
        raise RenderError("control token must belong to the deployment account and deny group/other access")
    for path, execute in ((config_path, False), (managed, False), (ku, True), (token, False)):
        readable_by(path, account, groups, execute)
    template = args.template
    if template is None:
        template = HERE.parent / "production-rc/ku-rc@.service"
        if not template.is_file():
            template = HERE.parent / "templates/ku-rc@.service"
    template = regular(template)
    text = template.read_text(encoding="utf-8")
    expected = {"@SLOT@", "@USER@", "@GROUP@", "@WORKING_DIRECTORY@", "@EXEC_START@", "@TIMEOUT_STOP@",
                "@MEMORY_MAX@", "@CPU_QUOTA@", "@TASKS_MAX@", "@NOFILE@", "@IDENTIFIER@"}
    if set(MARKER.findall(text)) != expected:
        raise RenderError("unit rendering source does not contain the expected fixed marker set")
    output = Path(args.output).absolute()
    if os.path.lexists(output) or not output.parent.is_dir():
        raise RenderError("unit output must be a fresh directory under an existing parent")
    units = []; payload = {}
    for slot in config["slots"]:
        name = slot["name"]
        if not SLOT.fullmatch(name):
            raise RenderError("invalid slot name for a concrete systemd unit")
        filename = f"{args.unit_prefix}-{name}.service"
        for operation, key in (("start", "manager_start"), ("stop", "manager_stop")):
            allowed = (["systemctl", operation, filename], ["/usr/bin/systemctl", operation, filename],
                       ["sudo", "-n", "/usr/bin/systemctl", operation, filename])
            if slot.get(key) not in allowed:
                raise RenderError(f"{key} must address this exact rendered unit using the fixed systemctl command")
        program = regular(slot["program"], executable=True)
        readable_by(program, account, groups, True)
        working = clean(str(program.parent))
        replacements = {"@SLOT@": name, "@USER@": args.user, "@GROUP@": group,
            "@WORKING_DIRECTORY@": working,
            "@EXEC_START@": " ".join(argument(v) for v in ("/usr/bin/python3", managed, "run", config_path, name)),
            "@TIMEOUT_STOP@": str(args.timeout_stop_seconds), "@MEMORY_MAX@": str(args.memory_max_bytes),
            "@CPU_QUOTA@": str(args.cpu_quota_percent), "@TASKS_MAX@": str(args.tasks_max),
            "@NOFILE@": str(args.nofile), "@IDENTIFIER@": args.unit_prefix + "-" + name}
        rendered = text
        for marker, value in replacements.items():
            rendered = rendered.replace(marker, value)
        if MARKER.search(rendered):
            raise RenderError("unit has an unresolved rendering marker")
        payload[filename] = rendered.encode()
        units.append({"slot": name, "filename": filename, "sha256": hashlib.sha256(payload[filename]).hexdigest(),
            "working_directory": working, "program": str(program), "program_sha256": digest(program),
            "configured_business_address": slot["listen_addr"], "supervisor_control_address": slot["control_addr"]})
    if digest(config_path) != initial_config_sha:
        raise RenderError("configuration changed while units were rendered")
    receipt = {"schema": "ku-rc-systemd-render-v1", "status": "rendered_not_installed", "unix_seconds": time.time(),
        "platform": {"system": platform.system(), "architecture": platform.machine(), "kernel": platform.release()},
        "account": {"name": args.user, "uid": account.pw_uid, "group": group, "gid": account.pw_gid},
        "config_path": str(config_path), "config_sha256": initial_config_sha, "limits_per_unit": limits,
        "source_sha256": {"renderer": digest(Path(__file__)), "template": digest(template), "manage": digest(managed), "ku": digest(ku)},
        "units": units, "kill_mode": "mixed", "business_deadlines_modified": False,
        "scope": "MemoryMax/TasksMax/CPUQuota cover supervisor, launcher, native and contained descendants; no Ku external worker domain enabled. LimitNOFILE applies per process, not aggregate FDs.",
        "permission_scope": "Unix ownership/mode-bit traversal check; ACLs, mounts, systemd support and actual user startup still require target verification.",
        "journal_scope": "stdout/stderr use journal with rate limiting; journal disk budget belongs to the host and is not an exclusive per-application hard disk cap.",
        "actions_performed": {"installed": False, "enabled": False, "started": False, "systemd_verify": False},
        "next": "Run systemd-analyze verify and actual isolated start/drain/SIGTERM/recovery tests; render success is not deployment acceptance."}
    output.mkdir(mode=0o700)
    for filename, value in payload.items():
        path = output / filename; path.write_bytes(value); path.chmod(0o644)
    (output / "render-receipt.json").write_text(json.dumps(receipt, ensure_ascii=False, indent=2) + "\n")
    return receipt


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ("config", "output"):
        parser.add_argument("--" + name, type=Path, required=True)
    for name in ("user", "unit-prefix"):
        parser.add_argument("--" + name, required=True)
    parser.add_argument("--manage", type=Path)
    parser.add_argument("--template", type=Path)
    for name, default in (("memory-max-bytes", 268435456), ("cpu-quota-percent", 200),
                          ("tasks-max", 64), ("nofile", 8192), ("timeout-stop-seconds", 16)):
        parser.add_argument("--" + name, type=int, default=default)
    try:
        receipt = render(parser.parse_args())
    except (OSError, ValueError, KeyError, TypeError) as error:
        print(f"systemd rendering failed: {error}", file=sys.stderr)
        return 1
    print(json.dumps(receipt, ensure_ascii=False, sort_keys=True))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
