#!/usr/bin/env python3
"""Required Linux Task TSan gate; no compiler fallback or unavailable-runtime skip."""
from __future__ import annotations

import argparse
import importlib.util
import json
import os
from pathlib import Path
import platform
import re
import shutil
import sys

sys.dont_write_bytecode = True
REPO = Path(__file__).resolve().parents[2]
SPEC = importlib.util.spec_from_file_location(
    "ku_tsan_bounds", Path(__file__).with_name("verify-native-three-os.py")
)
assert SPEC is not None and SPEC.loader is not None
BOUNDS = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(BOUNDS)

# One exact Rust test per invocation: zero matches must never pass this gate.
TARGETS = (
    ("native_task_tsan_probe_test", "native_task_tsan_instrumentation_and_runtime_probe", True),
    ("native_task_driver_test", "native_task_driver_two_real_workers_overlap_without_same_task_overlap", False),
    ("native_task_control_test", "native_task_control_frame_arbitration_and_references_execute_in_c", False),
    ("native_task_expression_pending_test", "native_task_expression_repeated_pending_preserves_lhs_and_starts_rhs_once", False),
    ("native_task_publishing_deadline_test", "native_task_publishing_cancel_tightens_unacked_child_after_timeout_result_race", False),
    ("native_task_cleanup_commit_test", "native_task_requested_cancel_tightens_unacked_child_before_cleanup_commit", False),
)
TSAN_OPTIONS = "halt_on_error=1:exitcode=66"


def require_pass(output: bytes, *, canary: bool = False) -> None:
    text = output.decode("utf-8", errors="replace")
    results = re.findall(
        r"^test result: (\w+)\. (\d+) passed; (\d+) failed; (\d+) ignored;",
        text, re.MULTILINE,
    )
    if results != [("ok", "1", "0", "0")] or re.search(r"\bskip(?:ped)?\s*:", text, re.I):
        raise RuntimeError("TSan gate requires exactly one executed passing test, with no skip")
    if not canary and ("WARNING: ThreadSanitizer:" in text or "FATAL: ThreadSanitizer:" in text):
        raise RuntimeError("Task path emitted a ThreadSanitizer diagnostic")


def run_command(command: list[str], logs: Path, label: str, timeout: int) -> bytes:
    previous = BOUNDS.COMMAND_TIMEOUT_SECONDS
    BOUNDS.COMMAND_TIMEOUT_SECONDS = timeout
    (logs / f"{label}.command.json").write_text(json.dumps(command), encoding="utf-8")
    try:
        completed = BOUNDS.run_bounded(command, REPO, label)
    except (SystemExit, OSError) as error:
        # The reused runner includes bounded stdout/stderr for nonzero exits.
        (logs / f"{label}.failure.txt").write_text(str(error), encoding="utf-8")
        raise RuntimeError(f"{label} failed: {error}") from error
    finally:
        BOUNDS.COMMAND_TIMEOUT_SECONDS = previous
    output = completed.stdout + completed.stderr
    (logs / f"{label}.log").write_bytes(output)
    print(output.decode("utf-8", errors="replace"), end="", flush=True)
    return output


def require_host() -> str:
    if platform.system() != "Linux" or platform.machine() not in ("x86_64", "AMD64"):
        raise RuntimeError("Task TSan gate requires the actual Linux x86_64 runner")
    compiler = shutil.which("clang")
    if compiler is None:
        raise RuntimeError("required Clang executable is unavailable; no fallback")
    return str(Path(compiler).resolve(strict=True))


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--logs", type=Path, required=True)
    args = parser.parse_args()
    # A rerun must not inherit old success logs. No recursive cleanup is needed.
    args.logs.mkdir(parents=True, exist_ok=False)
    compiler = require_host()
    os.environ["KU_CC"] = compiler  # Executable only: harness owns all C/link flags.
    os.environ["KU_NATIVE_REQUIRE_TSAN"] = "1"
    os.environ["TSAN_OPTIONS"] = TSAN_OPTIONS
    os.environ.pop("KU_NATIVE_TSAN_EXPECT_REJECTION", None)
    metadata = {
        "host": platform.platform(),
        "compiler": compiler,
        "required": os.environ["KU_NATIVE_REQUIRE_TSAN"],
        "tsan_options": TSAN_OPTIONS,
        "targets": TARGETS,
    }
    (args.logs / "configuration.json").write_text(json.dumps(metadata, indent=2), encoding="utf-8")
    for label, command in (
        ("commit", ["git", "rev-parse", "HEAD"]),
        ("rust-version", ["rustc", "--version", "--verbose"]),
        ("clang-version", [compiler, "--version"]),
    ):
        run_command(command, args.logs, label, 30)
    build = ["cargo", "test", "--locked", "--no-run"]
    for target, _, _ in TARGETS:
        build += ["--test", target]
    run_command(build, args.logs, "build", 300)
    for target, name, canary in TARGETS:
        command = [
            "cargo", "test", "--locked", "--test", target, name,
            "--", "--exact", "--nocapture", "--test-threads=1",
        ]
        if canary:
            command.append("--ignored")
        output = run_command(command, args.logs, target, 300)
        require_pass(output, canary=canary)
    print("Linux Task TSan selected-path gate passed; not full-runtime or performance coverage.", flush=True)


if __name__ == "__main__":
    main()
