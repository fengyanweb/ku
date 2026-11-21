#!/usr/bin/env python3
"""Exercise the bootstrap argv host and the independently emitted C."""

import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys
import tempfile

from build import executable, link_c, windows_process_owner


def execute(args, cwd, status=0):
    if os.name == "nt":
        command = [os.fspath(argument) for argument in args]
        result = windows_process_owner()(command, cwd, Path(command[0]).name, check=False)
    else:
        result = subprocess.run(args, cwd=cwd, capture_output=True, timeout=400)
    if result.returncode != status:
        raise AssertionError(
            f"{args}: expected exit {status}, got {result.returncode}\n"
            f"{result.stdout.decode(errors='replace')}"
            f"{result.stderr.decode(errors='replace')}"
        )
    return result


def exercise_emitted(cc, emitted, caller, stem, arguments, result):
    symbol = re.match(rb"/\* ([A-Za-z_][0-9A-Za-z_]*) \*/\n", emitted)
    assert symbol, emitted[:200]
    source = caller / (stem + ".c")
    invocation = symbol[1] + arguments
    source.write_bytes(
        emitted + b"\nint main(void) { return " + invocation + b" != " + result + b"; }\n"
    )
    program = stem + (".exe" if os.name == "nt" else "")
    link_c(cc, source.name, program, caller)
    execute([caller / program], caller)


def exercise_project(compiler, cc, root, caller):
    project = root / "项目 清单🙂"
    src, lib = project / "src", project / "lib"
    src.mkdir(parents=True)
    lib.mkdir()
    manifest = project / "modules.txt"
    library = lib / "数字🙂.ku"
    library.write_text(
        "struct Counter { value: int }\n"
        "fn Increment(value: int): int { return value + 1 }\n",
        encoding="utf-8",
    )
    main = src / "main.ku"
    valid = (
        'import { Counter as Record, Increment as Step } from "../lib/数字🙂.ku"\n'
        'struct Packet { item: Record }\n'
        'fn Answer(): int { return Step(41) }\n'
        'fn main(): null! { println(Answer()); return ok(null) }\n'
    )
    main.write_text(valid, encoding="utf-8")
    listed = "src/main.ku\r\nlib/数字🙂.ku\r\n".encode()
    manifest.write_bytes(listed)
    # A matching filename in the caller's directory must never substitute
    # for a manifest-relative project module.
    (caller / "src").mkdir()
    (caller / "src/main.ku").write_text("fn broken( ", encoding="utf-8")
    relative = Path("..") / project.name / manifest.name
    assert execute([compiler, "check-project", relative], caller).stdout == b""
    assert execute([compiler, "check-project", manifest], caller).stdout == b""
    if os.name == "nt":
        assert execute([compiler, "check-project", relative.as_posix()], caller).stdout == b""
    assert b"usage:" in execute(
        [compiler, "check-project", relative, "extra"], caller, 2
    ).stderr

    def project_error(code, logical=None):
        result = execute([compiler, "check-project", relative], caller, 1)
        assert not result.stdout and code in result.stderr, result.stderr
        if logical is not None:
            assert logical + b": " in result.stderr, result.stderr
            assert re.search(rb"[0-9]+:[0-9]+@[0-9]+\.\.[0-9]+:[0-9]+@[0-9]+", result.stderr), result.stderr

    for original, replacement, code in [
        ("Increment as Step", "Unknown as Step", b"missing_export"),
        ("Increment as Step", "Increment as Record", b"duplicate_import_alias"),
        ("item: Record", "item: Counter", b"undefined_type"),
        ("Step(41)", "Step()", b"call_arity"),
        ("Step(41)", 'Step("not an integer")', b"call_argument_type"),
    ]:
        main.write_text(valid.replace(original, replacement, 1), encoding="utf-8")
        project_error(code, b"src/main.ku")
    main.write_text("fn broken( ", encoding="utf-8")
    project_error(b"bootstrap.parser.stage3|unexpected_token", b"src/main.ku")
    main.write_text(valid, encoding="utf-8")

    manifest.write_bytes(b"src/main.ku\n")
    project_error(b"unresolved_import", b"src/main.ku")
    (caller / "only-in-caller.ku").write_text("fn Present(): int { return 1 }\n")
    manifest.write_bytes(b"only-in-caller.ku\n")
    project_error(b"only-in-caller.ku: ")
    (root / "escape.ku").write_text("fn Present(): int { return 1 }\n")
    manifest.write_bytes(b"../escape.ku\n")
    project_error(b"project_path")
    manifest.write_text("".join(f"unit{index}.ku\n" for index in range(65)))
    project_error(b"accepts at most 64 modules")
    manifest.write_bytes(b"x" * 32769)
    project_error(b"exceeds 32768 UTF-8 bytes")
    manifest.write_bytes(listed)
    assert execute([compiler, "check-project", relative], caller).stdout == b""
    emitted = execute([compiler, "emit-project", relative, "Answer"], caller).stdout
    exercise_emitted(cc, emitted, caller, "project-derived", b"()", b"42")

    library_manifest = project / "library.txt"
    library_manifest.write_text("lib/数字🙂.ku\n", encoding="utf-8")
    units = []
    plans = []
    for input_path, name in [(relative, "Answer"), (library_manifest, "Increment")]:
        plan = caller / f"{name}.plan.json"
        document = execute([compiler, "emit-unit-plan", input_path, name], caller)
        assert not document.stderr
        plan.write_bytes(document.stdout)
        parsed = json.loads(document.stdout)
        assert parsed["export"]["source_name"] == name
        plans.append(parsed)
        code = caller / f"{name}.c"
        record = execute([compiler, "emit-unit", input_path, name], caller)
        assert not record.stderr
        code.write_bytes(record.stdout)
        units.extend(["--unit", plan, code])

    entry = plans[0]["export"]
    dependency, = plans[0]["dependencies"]
    assert entry["source_path"] == "src/main.ku" and entry["signature"]["parameters"] == []
    assert (dependency["source_path"], dependency["node"]) == ("lib/数字🙂.ku", plans[1]["export"]["node"])
    host = caller / "linked-main.c"
    host.write_text(
        f'extern int64_t {entry["symbol"]}(void);\n'
        f'int main(void) {{ return {entry["symbol"]}() == 42 ? 0 : 1; }}\n',
        encoding="utf-8",
    )
    binary = caller / ("linked-answer.exe" if os.name == "nt" else "linked-answer")
    linker = Path(__file__).resolve().parents[1] / "link_c.py"
    execute([sys.executable, "-B", linker, *units, "--cc", cc, "--entry", entry["source_path"],
             str(entry["node"]), "--caller", host, "-o", binary], caller)
    assert execute([binary], caller).stdout == b""
    assert b"usage:" in execute([compiler, "emit-unit-plan", relative], caller, 2).stderr

    source = caller / "complete-program.c"
    whole = execute([compiler, "emit-program", relative], caller)
    assert not whole.stderr and b"int main(void)" in whole.stdout
    source.write_bytes(whole.stdout)
    program = "complete-program.exe" if os.name == "nt" else "complete-program"
    link_c(cc, source.name, program, caller)
    result = execute([caller / program], caller)
    assert result.stdout.replace(b"\r\n", b"\n") == b"42\n" and not result.stderr
    missing = execute([compiler, "emit-project", relative, "Unknown"], caller, 1)
    assert not missing.stdout and b"unknown_int_function" in missing.stderr, missing.stderr
    manifest.write_bytes(b"src/main.ku\n")
    unresolved = execute([compiler, "emit-project", relative, "Answer"], caller, 1)
    assert not unresolved.stdout and b"unresolved_import" in unresolved.stderr, unresolved.stderr
    unresolved_program = execute([compiler, "emit-program", relative], caller, 1)
    assert not unresolved_program.stdout and b"unresolved_import" in unresolved_program.stderr
    for command in ("emit-unit", "emit-unit-plan"):
        missing_unit = execute([compiler, command, relative, "Answer"], caller, 1)
        assert not missing_unit.stdout and b"unresolved_import" in missing_unit.stderr


def exercise(ku, cc):
    here = Path(__file__).resolve().parent
    with tempfile.TemporaryDirectory(prefix="ku-bootstrap-argv-test-") as temp:
        root = Path(temp)
        sources = root / "source copy"
        bootstrap = sources / "bootstrap"
        tools = bootstrap / "tools" / "compiler_driver"
        shutil.copytree(here, tools)
        for stage in ("stage1", "stage2", "stage3", "semantics"):
            shutil.copytree(here.parents[1] / stage, bootstrap / stage)
        if os.name == "nt":
            owner = Path(".github/scripts/verify-native-three-os.py")
            (sources / owner.parent).mkdir(parents=True)
            shutil.copyfile(here.parents[2] / owner, sources / owner)
        installed = root / "installed compiler"
        installed.mkdir()
        compiler = installed / ("ku-bootstrap.exe" if os.name == "nt" else "ku-bootstrap")
        command = [sys.executable, "-B", tools / "build.py", "--ku", ku, "--cc", cc,
                   "-o", compiler]
        execute(command, root)
        assert list(installed.iterdir()) == [compiler]

        # The checked C signature must reject a generated by-value parameter.
        source = tools / "driver.ku"
        original = source.read_text(encoding="utf-8")
        changed = original.replace("&symbol: str", "symbol: str", 1)
        assert changed != original
        source.write_text(changed, encoding="utf-8")
        before = hashlib.sha256(compiler.read_bytes()).digest()
        failure = execute(command, root, 1)
        assert b"bootstrap driver must provide BootstrapEntry" in failure.stderr
        assert hashlib.sha256(compiler.read_bytes()).digest() == before
        assert list(installed.iterdir()) == [compiler]
        shutil.rmtree(sources)

        caller = root / "调用目录 🙂"
        caller.mkdir()
        source = caller / "含空 格🙂.ku"
        source.write_text("fn AddOne(value: int): int { return value + 1 }\n", encoding="utf-8")
        assert execute([compiler, "check", source.name], caller).stdout == b""
        assert execute([compiler, "check", source], caller).stdout == b""
        invalid = caller / "malformed.ku"
        invalid.write_text("fn broken( ", encoding="utf-8")
        diagnostic = execute([compiler, "check", invalid.name], caller, 1)
        assert diagnostic.stdout == b""
        assert b"bootstrap.parser.stage3|unexpected_token" in diagnostic.stderr
        bad_call = caller / "bad call.ku"
        bad_call.write_text(
            'fn Takes(value: int): int { return value }\n'
            'fn Wrong(): int { return Takes("text") }\n',
            encoding="utf-8",
        )
        assert b"call_argument_type" in execute(
            [compiler, "check", bad_call.name], caller, 1
        ).stderr
        missing = execute([compiler, "check", "never-created.ku"], caller, 1)
        assert missing.stdout == b"" and missing.stderr
        assert b"usage:" in execute([compiler, "check", source.name, "extra"], caller, 2).stderr
        assert b"1 to 512 bytes of UTF-8" in execute(
            [compiler, "check", "x" * 513], caller, 2
        ).stderr
        malformed = "\ud800.ku" if os.name == "nt" else b"\xff.ku"
        assert b"1 to 512 bytes of UTF-8" in execute(
            [compiler, "check", malformed], caller, 2
        ).stderr
        assert b"1 to 512 bytes of UTF-8" in execute(
            [compiler, "emit", source.name, "界" * 171], caller, 2
        ).stderr
        valid_width = execute([compiler, "emit", source.name, "界" * 170 + "ab"], caller, 1)
        assert b"1 to 512 bytes of UTF-8" not in valid_width.stderr
        if os.name == "nt":
            verbatim = "\\\\?\\" + str(source)
            assert b"UTF-8 path limit" in execute([compiler, "check", verbatim], caller, 2).stderr
        no_function = execute([compiler, "emit", source.name, "Unknown"], caller, 1)
        assert no_function.stdout == b"" and no_function.stderr

        emitted = execute([compiler, "emit", source.name, "AddOne"], caller).stdout
        exercise_emitted(cc, emitted, caller, "derived", b"(41)", b"42")
        exercise_project(compiler, cc, root, caller)
        print("bootstrap argv, module checks, signature and standalone C verified")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--ku", default="ku")
    parser.add_argument("--cc", default="cc")
    args = parser.parse_args()
    exercise(executable(args.ku), executable(args.cc))


if __name__ == "__main__":
    main()
