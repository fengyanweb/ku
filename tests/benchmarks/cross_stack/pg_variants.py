#!/usr/bin/env python3
"""Derive an immutable, explicit PG contract manifest; never build or run it.

Table A keeps normal driver/session behavior. Table B enables the same-lease
reset contract for Go/Rust while leaving Ku's existing DISCARD contract alone.
Both output manifests pin BENCH_PG_MODE, so an inherited shell variable cannot
silently switch a normal measurement. The original manifest is never changed.
"""
from __future__ import annotations

import argparse
import copy
import hashlib
import json
from pathlib import Path


def digest(path):
    value = hashlib.sha256()
    with Path(path).open("rb") as stream:
        for block in iter(lambda: stream.read(1048576), b""):
            value.update(block)
    return value.hexdigest()


def derive(manifest_path, output, mode):
    manifest_path, output = Path(manifest_path), Path(output)
    if mode not in ("normal", "reset-equivalent"):
        raise ValueError("explicit normal or reset-equivalent mode required")
    if output.exists():
        raise ValueError("fresh output directory required; do not replace prior results")
    original = json.loads(manifest_path.read_text())
    manifest = copy.deepcopy(original)
    verified = {}
    for stack in ("go", "rust"):
        spec = manifest["stacks"][stack]
        build = spec["build"]
        if build.get("pg_modes") != ["normal", "reset-equivalent"]:
            raise ValueError(stack + " binary has no attested explicit PG mode support; rebuild it")
        sources = build.get("source_files_sha256", {})
        required = "pg_mode.go" if stack == "go" else "src/pg_mode.rs"
        if required not in sources or not sources:
            raise ValueError(stack + " per-stack source identity missing")
        directory = Path(original["source_root"]) / "tests/benchmarks/cross_stack" / stack
        actual_sources = {path.relative_to(directory).as_posix(): digest(path)
                          for path in sorted(directory.rglob("*"))
                          if path.is_file() and not path.is_symlink()
                          and not {"target", "node_modules", "__pycache__", ".git"}.intersection(path.relative_to(directory).parts)}
        if actual_sources != sources:
            raise ValueError(stack + " source inventory changed after binary build")
        command = spec["command"]
        if not command or digest(command[0]) != build.get("binary_sha256"):
            raise ValueError(stack + " compiled binary identity mismatch")
        spec["env"] = dict(spec.get("env", {}), BENCH_PG_MODE=mode)
        spec["pg_comparison_mode"] = mode
        verified[stack] = {"binary_sha256": build["binary_sha256"],
                           "source_files_sha256": sources}
    # This is a three-stack PG comparison, not a newly completed seven-stack
    # build. All paths remain absolute references to immutable original builds.
    manifest["stacks"] = {name: manifest["stacks"][name] for name in ("ku", "go", "rust")}
    manifest["build_complete"] = False
    manifest["pg_comparison_ready"] = True
    manifest["pg_comparison"] = {
        "mode": mode, "table": "A" if mode == "normal" else "B",
        "base_manifest": str(manifest_path.resolve()),
        "base_manifest_sha256": digest(manifest_path),
        "driver_sha256": digest(__file__), "verified": verified,
        "ku": "unchanged existing same-lease DISCARD ALL implementation",
        "query": "SELECT id,label,score FROM cross_stack_items WHERE id=$1",
        "go_result": "pgx Exec text protocol -> owned int/string; no JSON reparse",
        "rust_result": "query_typed binary INT4/String -> owned values; no JSON reparse",
        "reset_equivalent_contract": {
            "enabled": mode == "reset-equivalent", "operation_timeout_ms": 2000,
            "acquire_timeout_ms": 250, "reset_budget": "original operation remainder",
            "reset_lease": "same exclusive lease; drain before pool release",
            "reset_failure": "evict; preserve already owned query result",
            "query_retry": False, "future_cancel": "evict unreset Rust lease; close Go cancelled query lease",
        },
        "normal_deadlines": "unchanged: Go inherited 45s; Rust pool/create 1s and query 1s",
        "measurement_status": "not run; no correctness or performance result implied",
    }
    output.mkdir(parents=True)
    path = output / "manifest.json"
    path.write_text(json.dumps(manifest, ensure_ascii=False, indent=2) + "\n")
    return path


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--manifest", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--mode", choices=("normal", "reset-equivalent"), required=True)
    args = parser.parse_args()
    print(derive(args.manifest, args.output, args.mode))


if __name__ == "__main__":
    main()
