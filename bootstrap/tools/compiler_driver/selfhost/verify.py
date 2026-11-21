#!/usr/bin/env python3
"""Validate a same-source Stage1 → Stage2 → Stage3 chain from an installed Ku."""
import argparse
from hashlib import sha256
import json
import os
from pathlib import Path
import runpy
import shutil
import subprocess
import sys
import tempfile

from verify_generation import dynamic_imports, frozen_builtin


TOOLS = Path(__file__).resolve().parent
EXE = ".exe" if os.name == "nt" else ""


def digest(path):
    return sha256(path.read_bytes()).hexdigest()


def executable(name):
    path = shutil.which(name)
    if not path:
        raise ValueError(f"executable not found: {name}")
    return Path(path).absolute()


def read_json(path):
    return json.loads(path.read_bytes())


def write_json(path, value):
    path.write_text(json.dumps(value, indent=2) + "\n", encoding="utf-8")


def file_tail(path, max_bytes):
    with path.open("rb") as stream:
        length = stream.seek(0, os.SEEK_END)
        stream.seek(max(0, length - max_bytes))
        return stream.read(max_bytes).decode("utf-8", errors="replace").strip()


def run_tool(directory, script, arguments, scratch):
    environment = dict(os.environ, TMPDIR=str(scratch), TEMP=str(scratch), TMP=str(scratch))
    stdout = directory / (script.stem + ".stdout")
    stderr = directory / (script.stem + ".stderr")
    with stdout.open("xb") as output, stderr.open("xb") as errors:
        command = [sys.executable, "-B", script, *arguments, "--temp-dir", scratch]
        result = subprocess.run(command, cwd=directory, env=environment, stdout=output, stderr=errors)
    if result.returncode:
        message = [f"{script.name} exited {result.returncode}"]
        for label, path, size in (("stderr", stderr, 2304), ("stdout", stdout, 768)):
            detail = file_tail(path, size)
            if detail:
                message.append(f"{label} tail:\n{detail}")
        raise ValueError("\n".join(message))


def run_stage(output, name, input_sha, action):
    stage = output / name
    stage.mkdir(exist_ok=True)
    checkpoint = stage / "complete.json"
    if checkpoint.is_file():
        record = read_json(checkpoint)
        if record["input_sha256"] != input_sha:
            raise ValueError(f"{name} checkpoint belongs to a different producer")
        for relative, expected in record["files"].items():
            path = (output / relative).resolve(strict=True)
            if not path.is_relative_to(stage) or digest(path) != expected:
                raise ValueError(f"{name} checkpoint has a changed artifact: {relative}")
        return record["receipt"]

    attempt = Path(tempfile.mkdtemp(prefix="attempt-", dir=stage))
    summary, files = action(attempt)
    record = {"input_sha256": input_sha, "receipt": summary,
              "files": {path.relative_to(output).as_posix(): digest(path) for path in files}}
    temporary = attempt / "complete.json"
    write_json(temporary, record)
    temporary.replace(checkpoint)
    return summary


def bootstrap_chain(repo, source, ku, cc, output, scratch, limit, sanitizers, gnu_stage1=None):
    common = [repo, source, "--cc", cc]
    installed_sha = digest(ku)
    seed_options = ["--gnu-stage1-clang", gnu_stage1["clang"]["path"]] if gnu_stage1 else []

    def build_seed(attempt):
        binary = attempt / ("ku-bootstrap-stage1" + EXE)
        identity = attempt / "build-identity.json"
        run_tool(attempt, TOOLS / "rebuild_stage1.py", [*common, ku, binary, *seed_options], scratch)
        seed = read_json(identity)
        imports = dynamic_imports(binary)
        if (seed["commit"] != source or seed["seed_sha256"] != installed_sha or seed["sha256"] != digest(binary) or
                any("libku" in item.lower() for item in imports)):
            raise ValueError(f"Stage1 identity or imports do not match: {identity}")
        result = {"binary": binary.relative_to(output).as_posix(), "sha256": seed["sha256"],
                  "source_snapshot_sha256": seed["source_snapshot_sha256"], "dynamic_imports": imports}
        if os.name == "nt":
            sys.path.insert(0, str(repo / "bootstrap/tools/compiler_driver"))
            from build import gnu_stage1_commands, require_static_gnu_imports, windows_stack_reserve
            result["stack"] = windows_stack_reserve(binary)
            if gnu_stage1:
                host = seed["gnu_host"]
                compile_command, link_command = gnu_stage1_commands(gnu_stage1, "host.c", "ku-bootstrap.exe")
                if (host["toolchain"] != gnu_stage1 or seed["c_target"] != gnu_stage1["gcc"]["target"] or
                        host["compile_argv"] != compile_command or host["link_argv"] != link_command or
                        host["probe"]["status"] != "passed"):
                    raise ValueError("Stage1 Clang/MinGW receipt does not match the frozen inputs")
                require_static_gnu_imports(imports, "Stage1 host")
                result["gnu_host"] = host
        return result, [binary, identity]

    def build_generation(attempt, producer):
        units = attempt / "units"
        ledger = units / "ledger.json"
        verified = attempt / "verified"
        run_tool(attempt, TOOLS / "resolve_units.py",
                 [*common, "--producer", output / producer["binary"], "--limit", str(limit), "--output-dir", units],
                 scratch)
        run_tool(attempt, TOOLS / "verify_generation.py",
                 [*common, "--exact-ledger", ledger, "--output-dir", verified, "--sanitizers", sanitizers], scratch)
        binary = verified / ("ku-bootstrap" + EXE)
        identity = verified / "build-identity.json"
        receipt = verified / "verification.json"
        measured = read_json(receipt)
        image = read_json(identity)
        if (image["commit"] != source or image["producer_sha256"] != producer["sha256"] or
                image["sha256"] != digest(binary) or image["ledger_sha256"] != digest(ledger) or
                measured["source_commit"] != source or measured["ledger_sha256"] != image["ledger_sha256"] or
                measured["profiles"]["strict_o2"]["sha256"] != image["sha256"]):
            raise ValueError(f"linked compiler does not match the frozen inputs: {identity}")
        summary = {"binary": binary.relative_to(output).as_posix(), "sha256": image["sha256"],
                   "ledger": ledger.relative_to(output).as_posix(), "ledger_sha256": image["ledger_sha256"],
                   "units": measured["input_units"], "builtins": measured["builtins"],
                   "profiles": measured["profiles"], "sanitizers": measured["sanitizers"]}
        return summary, [binary, identity, ledger, receipt]

    seed_input_sha = installed_sha
    if gnu_stage1:
        seed_inputs = {"installed_sha256": installed_sha, "gnu_stage1_host": gnu_stage1}
        seed_input_sha = sha256(json.dumps(seed_inputs, sort_keys=True).encode()).hexdigest()
    stage1 = run_stage(output, "stage1", seed_input_sha, build_seed)
    stage2 = run_stage(output, "stage2", stage1["sha256"], lambda path: build_generation(path, stage1))
    stage3 = run_stage(output, "stage3", stage2["sha256"], lambda path: build_generation(path, stage2))
    return stage1, stage2, stage3


def compare_units(first, second):
    def fingerprints(ledger):
        result = {}
        for unit in read_json(ledger)["providers"]:
            for field in ("c", "plan"):
                path = ledger.with_name(unit[field])
                if digest(path) != unit[field + "_sha256"]:
                    raise ValueError(f"changed provider artifact: {path}")
            result[tuple(unit["key"])] = unit
        return result

    earlier, later = fingerprints(first), fingerprints(second)
    if earlier.keys() != later.keys():
        raise ValueError("Stage2 and Stage3 selected different provider identities")
    for key, details in earlier.items():
        for field in ("c", "plan"):
            second_record = later[key]
            if details[field + "_sha256"] == second_record[field + "_sha256"]:
                continue
            a = first.with_name(details[field]).read_bytes()
            b = second.with_name(second_record[field]).read_bytes()
            difference = next((i for i, (x, y) in enumerate(zip(a, b)) if x != y), min(len(a), len(b)))
            start = max(0, difference - 24)
            stop = difference + 72
            raise ValueError(f"Stage2/Stage3 {field} mismatch for {key!r} "
                             f"({details['source_name']}) at byte {difference}, lengths {len(a)}/{len(b)}: "
                             f"{a[start:stop]!r} != {b[start:stop]!r}")
    objects_identical = all(item["object_sha256"] == later[key]["object_sha256"] for key, item in earlier.items())
    return {"providers": len(earlier), "c_and_plans_identical": True, "objects_identical": objects_identical}


def verify_stage3_applications(repo, source, binary, cc, caller):
    driver = repo / "bootstrap/tools/compiler_driver/test_driver.py"
    frozen_builtin(repo, source, driver, driver.read_bytes())
    sys.path.insert(0, str(driver.parent))
    try:
        consumer = runpy.run_path(str(driver))
        checks = {
            "exercise_glob_import": (cc, caller),
            "exercise_direct_owned_moves": (caller,),
            "exercise_plain_null_entry": (cc, caller),
            "exercise_linked_program": (cc, caller),
            "exercise_namespace_import": (cc, caller),
            "exercise_qualified_types": (cc, caller),
            "exercise_qualified_constructor": (cc, caller),
            "exercise_static_reader_argument": (cc, caller),
            "exercise_linked_owned_reader": (cc, caller),
            "exercise_linked_reader_factory": (cc, caller),
            "exercise_linked_unit_enum": (cc, caller),
            "exercise_linked_unit_enum_branch": (cc, caller),
            "exercise_linked_unit_enum_match": (cc, caller),
            "exercise_linked_checked_enum_match": (cc, caller),
            "exercise_linked_unit_enum_result": (cc, caller),
        }
        for name, arguments in checks.items():
            try:
                consumer[name](binary, *arguments)
            except (AssertionError, OSError, subprocess.SubprocessError) as error:
                raise ValueError(f"verified Stage3 {name} failed: {error}") from error
        return dict.fromkeys(checks, "passed")
    finally:
        sys.path.pop(0)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--repo", type=Path, default=TOOLS.parents[3])
    parser.add_argument("--revision", default="HEAD")
    parser.add_argument("--ku", required=True, help="installed Ku CLI")
    parser.add_argument("--cc", required=True, help="matching target C11 compiler executable")
    parser.add_argument("--gnu-stage1-clang", help="compile only the Windows GNU Stage1 host with this Clang")
    parser.add_argument("--output-dir", required=True, type=Path, help="private directory for receipts and resume")
    parser.add_argument("--temp-dir", type=Path, help="existing directory for an isolated scratch subdirectory")
    parser.add_argument("--sanitizers", choices=("auto", "required", "off"), default="auto")
    parser.add_argument("--limit", type=int, default=768, help="maximum provider attempts per generation")
    parser.add_argument("--stage3-application-checks", action="store_true",
                        help="check scoped applications and ABI contracts after Stage3 verification")
    args = parser.parse_args()
    if not __debug__:
        parser.error("verification requires Python assertions")
    if args.limit < 1:
        parser.error("--limit must be positive")
    if args.gnu_stage1_clang and os.name != "nt":
        parser.error("--gnu-stage1-clang requires the x64 Windows GNU host")

    repo = args.repo.resolve(strict=True)
    source = subprocess.check_output(["git", "rev-parse", "--verify", args.revision + "^{commit}"],
                                     cwd=repo, text=True).strip()
    ku, cc = executable(args.ku), executable(args.cc)
    target = subprocess.check_output([cc, "-dumpmachine"], text=True, timeout=20).strip()
    version = subprocess.check_output([cc, "--version"], timeout=20)
    output = args.output_dir.resolve()
    output.mkdir(parents=True, exist_ok=True, mode=0o700)
    inputs = {"source_commit": source, "ku_sha256": digest(ku), "c_compiler": str(cc),
              "c_target": target, "c_version_sha256": sha256(version).hexdigest(), "sanitizers": args.sanitizers,
              "provider_limit": args.limit}
    gnu_stage1 = None
    if args.gnu_stage1_clang:
        sys.path.insert(0, str(TOOLS.parent))
        from build import gnu_stage1_toolchain
        gnu_stage1 = gnu_stage1_toolchain(args.gnu_stage1_clang, cc, repo)
        inputs["gnu_stage1_host"] = gnu_stage1
    snapshot = output / "inputs.json"
    if snapshot.is_file():
        if read_json(snapshot) != inputs:
            raise ValueError("the output directory belongs to a different source, installed CLI, toolchain or policy")
    else:
        if any(output.iterdir()):
            raise ValueError("choose an empty output directory or a matching self-host run")
        write_json(snapshot, inputs)
    stage_parent = args.temp_dir.resolve(strict=True) if args.temp_dir else output
    with tempfile.TemporaryDirectory(prefix=".scratch-", dir=stage_parent) as temporary:
        stage1, stage2, stage3 = bootstrap_chain(repo, source, ku, cc, output, Path(temporary), args.limit,
                                                args.sanitizers, gnu_stage1)
    comparison = compare_units(output / stage2["ledger"], output / stage3["ledger"])
    comparison["strict_o2_identical"] = stage2["sha256"] == stage3["sha256"]
    windows_coff = "-windows-msvc" in target or target.endswith(("-mingw32", "-mingw64", "-windows-gnu"))
    if windows_coff and not (comparison["objects_identical"] and comparison["strict_o2_identical"]):
        raise ValueError(f"reproducible COFF/PE raw comparison failed: {comparison}; "
                         f"PE SHA256 {stage2['sha256']}, {stage3['sha256']}")
    summary = {"status": "passed", **inputs, "stage1": stage1, "stage2": stage2, "stage3": stage3,
               "comparison": comparison}
    if args.stage3_application_checks:
        with tempfile.TemporaryDirectory(prefix=".stage3-applications-", dir=stage_parent) as temporary:
            checks = verify_stage3_applications(repo, source, output / stage3["binary"], cc, Path(temporary))
        summary["stage3_application_checks"] = {
            "status": "passed", "source_commit": source, "stages": 3, "compiler_sha256": stage3["sha256"],
            "checks": checks,
        }
    write_json(output / "summary.json", summary)
    print(json.dumps(summary))
    return 0


if __name__ == "__main__":
    try:
        sys.exit(main())
    except (OSError, ValueError, KeyError, subprocess.SubprocessError) as error:
        sys.exit(f"self-host verification: {error}")
