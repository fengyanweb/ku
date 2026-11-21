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


def exercise_glob_import(compiler, cc, caller):
    project = caller / "full-import"
    source, library = project / "src", project / "lib"
    source.mkdir(parents=True)
    library.mkdir()
    manifest = project / "modules.txt"
    manifest.write_text("src/main.ku\nlib/numbers.ku\nlib/private.ku\n", encoding="utf-8")
    main = source / "main.ku"
    private = library / "private.ku"
    private_source = "fn hidden(): int { return 8 }\n"
    private.write_text(private_source, encoding="utf-8")
    (library / "numbers.ku").write_text(
        "struct Counter { value: int }\n"
        "fn increment(value: int): int { return value + 1 }\n"
        "fn Step(value: int): int { return increment(increment(value)) }\n",
        encoding="utf-8",
    )
    valid = (
        'import "../lib/numbers"\n'
        'import "../lib/private.ku"\n'
        "struct Packet { item: Counter }\n"
        "fn Answer(): int { return Step(40) }\n"
        "fn main(): null! { println(str(Answer())); return ok(null) }\n"
    )
    main.write_text(valid, encoding="utf-8")
    assert execute([compiler, "check-project", manifest], caller).stdout == b""
    emitted = execute([compiler, "emit-project", manifest, "Answer"], caller).stdout
    exercise_emitted(cc, emitted, caller, "full-import-answer", b"()", b"42")
    plan = execute([compiler, "emit-unit-plan", manifest, "Answer"], caller)
    dependency, = json.loads(plan.stdout)["dependencies"]
    assert dependency["source_path"] == "lib/numbers.ku"
    assert dependency["source_name"] == "Step"
    build_program = Path(__file__).resolve().parents[1] / "build_program.py"
    binary = caller / ("full-import-app.exe" if os.name == "nt" else "full-import-app")
    execute([sys.executable, "-B", build_program, manifest, "--compiler", compiler,
             "--cc", cc, "-o", binary], caller)
    assert execute([binary], caller).stdout.replace(b"\r\n", b"\n") == b"42\n"

    for replacement, code in [
        ("Step()", b"call_arity"),
        ('Step("40")', b"call_argument_type"),
    ]:
        main.write_text(valid.replace("Step(40)", replacement), encoding="utf-8")
        rejected = execute([compiler, "check-project", manifest], caller, 1).stderr
        assert b"src/main.ku:" in rejected and code in rejected, rejected
    main.write_text(valid.replace("Step(40)", "increment(40)"), encoding="utf-8")
    rejected = execute([compiler, "emit-project", manifest, "Answer"], caller, 1)
    assert b"increment" in rejected.stderr and not rejected.stdout, rejected.stderr
    main.write_text(valid + "fn Step(value: int): int { return value }\n", encoding="utf-8")
    rejected = execute([compiler, "check-project", manifest], caller, 1).stderr
    assert b"src/main.ku:" in rejected and b"import_name_conflict" in rejected, rejected
    main.write_text(valid, encoding="utf-8")
    private.write_text('import "../src/main"\n' + private_source, encoding="utf-8")
    rejected = execute([compiler, "check-project", manifest], caller, 1).stderr
    assert b"cyclic_import" in rejected, rejected


def exercise_namespace_import(compiler, cc, caller):
    project = caller / "namespace-import"
    project.mkdir()
    manifest = project / "modules.txt"
    manifest.write_text("main.ku\ncalc.ku\n", encoding="utf-8")
    main = project / "main.ku"
    library = project / "calc.ku"
    library.write_text(
        "struct Counter { value: int }\n"
        "fn privateStep(value: int): int { return value - 1 }\n"
        "fn Step(value: int): int { return privateStep(value) + 3 }\n",
        encoding="utf-8",
    )
    header = 'import calc from "./calc"\n'
    entry = (
        "fn main(): null! {\n"
        "    first = calc.Step(40)\n"
        "    if (true) { calc = 7 println(str(calc)) }\n"
        "    last = calc.Step(39)\n"
        "    println(str(first))\n"
        "    println(str(last))\n"
        "    return ok(null)\n"
        "}\n"
    )
    main.write_text(header + entry, encoding="utf-8")
    assert not execute([compiler, "check-project", manifest], caller).stdout
    plan = execute([compiler, "emit-unit-plan", manifest, "main"], caller)
    dependency, = json.loads(plan.stdout)["dependencies"]
    assert (dependency["source_path"], dependency["source_name"]) == ("calc.ku", "Step")
    signature = dependency["signature"]
    assert signature["parameters"][0]["c_type"] == signature["result"]["c_type"] == "int64_t"
    assert signature["parameters"][0]["passing"] == "value" and not signature["returns_result"]
    builder = Path(__file__).resolve().parents[1] / "build_program.py"
    binary = caller / ("namespace-app.exe" if os.name == "nt" else "namespace-app")
    execute([sys.executable, "-B", builder, manifest, "--compiler", compiler,
             "--cc", cc, "-o", binary], caller)
    assert execute([binary], caller).stdout.replace(b"\r\n", b"\n") == b"7\n42\n41\n"

    for expression, diagnostic in [
        ("calc.Missing(40)", b"missing_export"),
        ("calc.privateStep(40)", b"missing_export"),
        ("calc.Counter(40)", b"namespace_member_not_function"),
        ("calc.Step()", b"call_arity"),
        ('calc.Step("40")', b"call_argument_type"),
    ]:
        main.write_text(header + f"fn Result(): int {{ return {expression} }}\n", encoding="utf-8")
        result = execute([compiler, "check-project", manifest], caller, 1)
        assert diagnostic in result.stderr and b"main.ku:" in result.stderr and not result.stdout
    main.write_text(header + "fn Shadow(calc: int): int { return calc.Step(40) }\n", encoding="utf-8")
    rejected_shadow = execute([compiler, "emit-unit-plan", manifest, "Shadow"], caller, 1)
    assert b"shadowed_linked_dependency" in rejected_shadow.stderr and not rejected_shadow.stdout

    main.write_text(header, encoding="utf-8")
    library.write_text('import entry from "./main"\n', encoding="utf-8")
    cycle = execute([compiler, "check-project", manifest], caller, 1)
    assert b"cyclic_import" in cycle.stderr and not cycle.stdout


def exercise_qualified_types(compiler, cc, caller):
    project = caller / "qualified-types"
    project.mkdir()
    manifest = project / "modules.txt"
    manifest.write_text("main.ku\ncalc.ku\n", encoding="utf-8")
    main = project / "main.ku"
    provider = (
        "struct Counter { value: int }\n"
        "struct privateCounter { value: int }\n"
        "enum Mode { Ready }\n"
        "fn Make(): Counter { return Counter { value: 42 } }\n"
    )
    (project / "calc.ku").write_text(provider, encoding="utf-8")
    header = 'import calc from "./calc"\n'
    read = "fn Read(&value: calc.Counter): int { return value.value }\n"
    entry = (
        "fn main(): null! {\n"
        "    value = calc.Make()\n"
        "    first = Read(value)\n"
        "    again = Read(value)\n"
        "    println(str(first))\n"
        "    println(str(again))\n"
        "    return ok(null)\n"
        "}\n"
    )
    main.write_text(header + 'import { Counter } from "./calc"\n' + read.replace("calc.Counter", "Counter") + entry,
                    encoding="utf-8")
    named = execute([compiler, "emit-unit-plan", manifest, "Read"], caller)
    named_view, = json.loads(named.stdout)["export"]["signature"]["parameters"]
    qualified = header + read + entry
    main.write_text(qualified, encoding="utf-8")
    plan = execute([compiler, "emit-unit-plan", manifest, "Read"], caller)
    view, = json.loads(plan.stdout)["export"]["signature"]["parameters"]
    assert view["source_name"] == "Counter" and view["source_path"] == "calc.ku", view
    assert view["kind"] == "Struct" and view["passing"] == "view" and view["node"] > 0
    assert view == named_view, (view, named_view)
    builder = Path(__file__).resolve().parents[1] / "build_program.py"
    binary = caller / ("qualified-app.exe" if os.name == "nt" else "qualified-app")
    execute([sys.executable, "-B", builder, manifest, "--compiler", compiler, "--cc", cc, "-o", binary], caller)
    assert execute([binary], caller).stdout.replace(b"\r\n", b"\n") == b"42\n42\n"

    main.write_text(header + "fn ReadMode(value: calc.Mode): null! { return ok(null) }\n", encoding="utf-8")
    assert not execute([compiler, "check-project", manifest], caller).stdout
    for name in ("calc.Missing", "calc.privateCounter", "calc.Make", "other.Counter"):
        main.write_text(header + read.replace("calc.Counter", name), encoding="utf-8")
        rejected = execute([compiler, "check-project", manifest], caller, 1)
        assert b"undefined_type" in rejected.stderr and name.encode() in rejected.stderr and not rejected.stdout
    main.write_text('import "std.fs"\n' + read.replace("calc.Counter", "fs.Counter"), encoding="utf-8")
    rejected = execute([compiler, "check-project", manifest], caller, 1)
    assert b"undefined_type" in rejected.stderr and b"fs.Counter" in rejected.stderr, rejected.stderr

    (project / "other.ku").write_text(provider, encoding="utf-8")
    manifest.write_text("main.ku\ncalc.ku\nother.ku\n", encoding="utf-8")
    main.write_text('import other from "./other"\n' + qualified.replace("calc.Make()", "other.Make()"), encoding="utf-8")
    rejected = execute([compiler, "emit-unit-plan", manifest, "main"], caller, 1)
    assert b"linked_argument_type" in rejected.stderr and not rejected.stdout


def exercise_qualified_constructor(compiler, cc, caller):
    project = caller / "qualified-constructor"
    project.mkdir()
    manifest = project / "modules.txt"
    manifest.write_text("main.ku\nvalues.ku\n", encoding="utf-8")
    (project / "values.ku").write_text(
        "struct Entry { value: int, label: str }\n", encoding="utf-8",
    )
    entry = project / "main.ku"
    source = '''\
import values from "./values"
fn main(): null! {
    result = values.Entry { value: 41, label: "条目" + "🙂" }
    println(str(result.value + 1))
    println(result.label)
    return ok(null)
}
'''
    entry.write_text(source, encoding="utf-8")
    assert execute([compiler, "check-project", manifest], caller).stdout == b""
    build_program = Path(__file__).resolve().parents[1] / "build_program.py"
    binary = caller / ("qualified-constructor-app.exe" if os.name == "nt" else "qualified-constructor-app")
    execute([sys.executable, "-B", build_program, manifest, "--compiler", compiler,
             "--cc", cc, "-o", binary], caller)
    for original, replacement, diagnostic in (
        ("value: 41", "missing: 41", b"invalid_linked_literal_field"),
        ("value: 41", "value: true", b"linked_literal_field_type"),
        ("values.Entry", "values.Hidden", b"undefined_type"),
    ):
        entry.write_text(source.replace(original, replacement, 1), encoding="utf-8")
        rejected = execute([compiler, "emit-unit", manifest, "main"], caller, 1)
        assert diagnostic in rejected.stderr and not rejected.stdout, rejected.stderr
    entry.unlink()
    (project / "values.ku").unlink()
    result = execute([binary], caller)
    assert not result.stderr and result.stdout.replace(b"\r\n", b"\n") == "42\n条目🙂\n".encode("utf-8")


def exercise_checked_bool(compiler, cc, caller):
    source = caller / "boolean.ku"
    source.write_text('''\
fn Check(value: int): bool! {
    if (value < 0) { fail { domain: "predicate", code: "range", message: "negative test" } }
    return ok(value > 0)
}
fn UseGuard(value: int): int! {
    if (Check(value)?) { return ok(41) }
    return ok(27)
}
fn UseLocal(value: int): bool! {
    checked = Check(value)?
    return ok(checked)
}
fn InvalidGuard(value: int): null! {
    if (UseGuard(value)?) { return ok(null) }
    return ok(null)
}
''', encoding="utf-8")
    manifest = caller / "boolean.txt"
    manifest.write_text(source.name + "\n", encoding="utf-8")
    linker = Path(__file__).resolve().parents[1] / "link_c.py"
    link = [sys.executable, "-B", linker, "--cc", cc]
    units = {}
    declarations = {}
    for name in ("Check", "UseGuard", "UseLocal"):
        plan = caller / f"{name}.plan.json"
        code = caller / f"{name}.c"
        plan.write_bytes(execute([compiler, "emit-unit-plan", manifest, name], caller).stdout)
        code.write_bytes(execute([compiler, "emit-unit", manifest, name], caller).stdout)
        declarations[name] = json.loads(plan.read_bytes())["export"]
        units[name] = ["--unit", plan, code]
    for name in ("Check", "UseLocal"):
        signature = declarations[name]["signature"]
        assert signature["returns_result"] and signature["result"]["source_name"] == "bool"
        assert signature["result"]["c_type"] == "KuBootstrapResult_bool"

    for name, expected in [("UseGuard", (27, 41)), ("UseLocal", (0, 1))]:
        entry = declarations[name]
        symbol = entry["symbol"]
        carrier = entry["signature"]["result"]["c_type"]
        host = caller / "boolean-host.c"
        host.write_text(f'''\
#include <assert.h>
#include <stdlib.h>
#include <string.h>
int main(void) {{
    {carrier} failure = {symbol}(-1);
    assert(!failure.ok);
    assert(failure.error.domain.len == 9 && memcmp(failure.error.domain.ptr, "predicate", 9) == 0);
    assert(failure.error.code.len == 5 && memcmp(failure.error.code.ptr, "range", 5) == 0);
    assert(failure.error.message.len == 13 && memcmp(failure.error.message.ptr, "negative test", 13) == 0);
    if (failure.error.domain.storage == 1u) free(failure.error.domain.ptr);
    if (failure.error.code.storage == 1u) free(failure.error.code.ptr);
    if (failure.error.message.storage == 1u) free(failure.error.message.ptr);
    {carrier} first = {symbol}(0), second = {symbol}(1);
    assert(first.ok && first.value == {expected[0]});
    assert(second.ok && second.value == {expected[1]});
    return 0;
}}
''', encoding="ascii")
        binary = caller / ("linked-bool.exe" if os.name == "nt" else "linked-bool")
        execute([*link, *units[name], *units["Check"], "--entry", entry["source_path"], str(entry["node"]),
                 "--caller", host, "-o", binary], caller)
        assert execute([binary], caller).stdout == b""

    invalid = execute([compiler, "emit-unit", manifest, "InvalidGuard"], caller, 1)
    assert not invalid.stdout and b"linked_guard_type" in invalid.stderr
    _, plan, code = units["Check"]
    wrong = json.loads(plan.read_bytes())
    wrong["export"]["signature"]["result"]["c_type"] = "KuBootstrapResult_int64_t"
    bad = caller / "wrong-bool.plan.json"
    bad.write_text(json.dumps(wrong), encoding="utf-8")
    rejected = execute([*link, "--unit", bad, code, "--check-only"], caller, 1)
    assert b"checked bool requires the KuBootstrapResult_bool carrier" in rejected.stderr


def exercise_linked_nullable_int(compiler, cc, caller):
    project = caller / "linked-nullable-int"
    project.mkdir()
    (project / "row.ku").write_text('''\
struct Record { line: int }
fn SelectLine(&row: Record, gate: bool): int | null {
    if (!gate) { return null }
    return row.line
}
fn IsMissing(line: null | int): bool {
    if (line == null) { return true }
    return false
}
''', encoding="utf-8")
    (project / "select.ku").write_text('''\
import { Record, SelectLine, IsMissing } from "./row.ku"
fn SelectFrom(value: int, gate: bool): int | null {
    row = Record { line: value }
    chosen = SelectLine(row, gate)
    missing = IsMissing(chosen)
    if (missing) { return null }
    return chosen
}
fn Unsupported(value: float | null): float | null { return value }
''', encoding="utf-8")
    manifest = project / "modules.txt"
    manifest.write_text("select.ku\nrow.ku\n", encoding="utf-8")
    provider_manifest = project / "row-modules.txt"
    provider_manifest.write_text("row.ku\nselect.ku\n", encoding="utf-8")
    linker = Path(__file__).resolve().parents[1] / "link_c.py"
    link = [sys.executable, "-B", linker, "--cc", cc]
    units = []
    declarations = {}
    for function in ("SelectFrom", "SelectLine", "IsMissing"):
        plan = project / f"{function}.plan.json"
        code = project / f"{function}.c"
        selected = manifest if function == "SelectFrom" else provider_manifest
        plan.write_bytes(execute([compiler, "emit-unit-plan", selected, function], caller).stdout)
        code.write_bytes(execute([compiler, "emit-unit", selected, function], caller).stdout)
        declarations[function] = json.loads(plan.read_bytes())["export"]
        units.extend(["--unit", plan, code])
    entry = declarations["SelectFrom"]
    expected = {"kind": "NullableInt", "source_name": "int", "source_path": None, "node": 0,
                "c_type": "KuBootstrapOptionalInt", "passing": "value"}
    assert entry["signature"]["result"] == expected and not entry["signature"]["returns_result"]
    assert declarations["SelectLine"]["signature"]["result"] == expected
    assert declarations["IsMissing"]["signature"]["parameters"] == [expected]

    host = project / "host.c"
    symbol = entry["symbol"]
    host.write_text(f'''\
#include <assert.h>
int main(void) {{
    KuBootstrapOptionalInt missing = {symbol}(42, false);
    KuBootstrapOptionalInt zero = {symbol}(0, true);
    KuBootstrapOptionalInt positive = {symbol}(7, true);
    assert(missing.is_null && missing.value == 0);
    assert(!zero.is_null && zero.value == 0);
    assert(!positive.is_null && positive.value == 7);
    return 0;
}}
''', encoding="ascii")
    binary = project / ("linked-nullable-int.exe" if os.name == "nt" else "linked-nullable-int")
    options = [*link, *units, "--entry", entry["source_path"], str(entry["node"]), "--caller", host, "-o", binary]
    execute(options, caller)
    assert execute([binary], caller).stdout == b""

    original = project / "SelectLine.c"
    drift = project / "wrong-tag.c"
    code = original.read_bytes()
    assert code.count(b"bool is_null;") == 1
    drift.write_bytes(code.replace(b"bool is_null;", b"uint8_t is_null;"))
    rejected_carrier = execute([drift if item == original else item for item in options], caller, 1)
    assert b"link-plan C nullable int has a different carrier" in rejected_carrier.stderr
    invalid = execute([compiler, "emit-unit", manifest, "Unsupported"], caller, 1)
    assert not invalid.stdout and b"unsupported_layout_type" in invalid.stderr


def exercise_linked_owned_reader(compiler, cc, caller):
    project = caller / "linked-owned-reader"
    project.mkdir()
    (project / "apply.ku").write_text('''\
struct Row { line: int }
struct Other { line: int }
fn Apply(filter: fn(&Row): int | null, &row: Row): int | null {
    return filter(row)
}
fn Different(filter: fn(&Other): int | null, &row: Other): int | null {
    return filter(row)
}
''', encoding="utf-8")
    (project / "forward.ku").write_text('''\
import { Apply, Other, Row } from "./apply.ku"
fn Forward(filter: fn(&Row): int | null, &row: Row): int | null {
    return Apply(filter, row)
}
fn Bad(filter: fn(&Other): int | null, &row: Row): int | null {
    return Apply(filter, row)
}
''', encoding="utf-8")
    local = project / "forward.txt"
    local.write_text("forward.ku\napply.ku\n", encoding="utf-8")
    imported = project / "apply.txt"
    imported.write_text("apply.ku\nforward.ku\n", encoding="utf-8")
    linker = Path(__file__).resolve().parents[1] / "link_c.py"
    link = [sys.executable, "-B", linker, "--cc", cc]
    units = []
    exports = {}
    for function, manifest in [("Forward", local), ("Apply", imported)]:
        plan = project / f"{function}.plan.json"
        code = project / f"{function}.c"
        plan.write_bytes(execute([compiler, "emit-unit-plan", manifest, function], caller).stdout)
        code.write_bytes(execute([compiler, "emit-unit", manifest, function], caller).stdout)
        exports[function] = json.loads(plan.read_bytes())["export"]
        units.extend(["--unit", plan, code])
    entry = exports["Forward"]
    callback, view = entry["signature"]["parameters"]
    assert callback["kind"] == "NullableRecordViewReader" and callback["passing"] == "value"
    assert (callback["source_path"], callback["node"]) == (view["source_path"], view["node"])
    assert view["passing"] == "view" and view["kind"] == "Struct"
    assert entry["signature"]["result"]["kind"] == "NullableInt"
    row = view["c_type"].removeprefix("const ").removesuffix("*")
    carrier = callback["c_type"]
    assert carrier == "KuBootstrapNullableRecordViewReader_" + row
    host = project / "host.c"
    host.write_text(f'''\
#include <assert.h>
#include <stdlib.h>
#include <string.h>
static void *active;
static unsigned int drops;
static bool invalid;
void check_reader_free(void *ptr) {{
    assert(ptr == active && drops == 0);
    ++drops;
    active = NULL;
    free(ptr);
    if (invalid) puts("capture dropped");
}}
static KuBootstrapOptionalInt pick(const KuProjectionString *capture, const {row} *input) {{
    assert(capture->len == 1 && capture->ptr == active && capture->ptr[0] == 'x');
    if (input->f0 < 0) return (KuBootstrapOptionalInt){{true, 0}};
    return (KuBootstrapOptionalInt){{false, input->f0}};
}}
int main(int argc, char **argv) {{
    invalid = argc > 1 && strcmp(argv[1], "invalid") == 0;
    const bool track = argc > 1;
    const int64_t values[] = {{-1, 0, 7}};
    for(size_t i = 0; i < sizeof(values)/sizeof(*values); ++i) {{
        uint8_t *storage = malloc(1);
        assert(storage);
        storage[0] = 'x';
        active = storage;
        drops = 0;
        {row} input = {{ .f0 = values[i] }};
        KuBootstrapOptionalInt result = {entry["symbol"]}(
            ({carrier}){{invalid ? NULL : pick, {{storage, 1, 1, 1}}}}, &input);
        assert(result.is_null == (values[i] < 0));
        assert(result.value == (values[i] < 0 ? 0 : values[i]));
        if (track) assert(drops == 1 && active == NULL);
    }}
    return 0;
}}
''', encoding="ascii")
    binary = project / ("owned-reader.exe" if os.name == "nt" else "owned-reader")
    args = ["--entry", entry["source_path"], str(entry["node"]), "--caller", host, "-o", binary]
    execute([*link, *units, *args], caller)
    assert execute([binary], caller).stdout == b""
    instrumentation = b"#include <stdlib.h>\nextern void check_reader_free(void *);\n#define free check_reader_free\n"
    tracked = []
    for item in units:
        if isinstance(item, Path) and item.suffix == ".c":
            checked = item.with_suffix(".tracked.c")
            checked.write_bytes(instrumentation + item.read_bytes() + b"\n#undef free\n")
            item = checked
        tracked.append(item)
    execute([*link, *tracked, *args], caller)
    assert execute([binary, "track"], caller).stdout == b""
    absent = execute([binary, "invalid"], caller, 1)
    assert absent.stdout.strip() == b"capture dropped" and b"invalid owned reader callback" in absent.stderr

    original = project / "Apply.c"
    invalid_c = project / "wrong-entry.c"
    code = original.read_bytes()
    signature = b"(*entry)(const KuProjectionString*,"
    assert code.count(signature) == 1
    invalid_c.write_bytes(code.replace(signature, b"(*entry)(KuProjectionString*,"))
    refused = execute([*link, *[invalid_c if item == original else item for item in units], *args], caller, 1)
    assert b"link-plan C callback has a different carrier" in refused.stderr

    wrong = json.loads(execute([compiler, "emit-unit-plan", imported, "Different"], caller).stdout)
    forward = project / "Forward.plan.json"
    plan = json.loads(forward.read_bytes())
    plan["dependencies"][0]["signature"]["parameters"][0] = wrong["export"]["signature"]["parameters"][0]
    swapped = project / "wrong-nominal.plan.json"
    swapped.write_text(json.dumps(plan), encoding="utf-8")
    refused = execute([*link, *[swapped if item == forward else item for item in units], "--check-only"], caller, 1)
    assert b"signature mismatch" in refused.stderr
    refused_source = execute([compiler, "emit-unit", local, "Bad"], caller, 1)
    assert not refused_source.stdout and b"linked_argument_type" in refused_source.stderr


def exercise_static_reader_argument(compiler, cc, caller):
    project = caller / "static-reader-argument"
    project.mkdir()
    provider = (
        "struct Row { line: int }\n"
        "fn Pick(&row: Row): int | null { if (row.line == 1) { return null } return row.line }\n"
        "fn ByValue(row: Row): int | null { return row.line }\n"
        "fn Apply(filter: fn(&Row): int | null, &row: Row): int | null { return filter(row) }\n"
    )
    (project / "reader.ku").write_text(provider, encoding="utf-8")
    manifest = project / "modules.txt"
    manifest.write_text("main.ku\nreader.ku\n", encoding="utf-8")
    main = project / "main.ku"
    header = 'import picker from "./reader"\nimport { Row, Apply, Pick } from "./reader.ku"\n'
    main.write_text(header + (
        "fn Named(&row: Row): int | null { return Apply(Pick, row) }\n"
        "fn Qualified(&row: Row): int | null { return Apply(picker.Pick, row) }\n"
        "fn Declared(&row: Row): int | null {\n"
        "    reader: fn(&Row): int | null = picker.Pick\n"
        "    if (row.line == 1) { return null }\n"
        "    return Apply(reader, row)\n"
        "}\n"
        "fn main(): null! {\n"
        "    miss = Row { line: 1 }\n"
        "    zero = Row { line: 0 }\n"
        "    hit = Row { line: 7 }\n"
        "    println(str(Named(miss)))\n"
        "    println(str(Qualified(miss)))\n"
        "    println(str(Declared(miss)))\n"
        "    println(str(Named(zero)))\n"
        "    println(str(Qualified(zero)))\n"
        "    println(str(Declared(zero)))\n"
        "    println(str(Named(hit)))\n"
        "    println(str(Qualified(hit)))\n"
        "    println(str(Declared(hit)))\n"
        "    return ok(null)\n"
        "}\n"
    ), encoding="utf-8")
    for function in ("Named", "Qualified", "Declared"):
        plan = execute([compiler, "emit-unit-plan", manifest, function], caller)
        dependencies = {item["source_name"]: item for item in json.loads(plan.stdout)["dependencies"]}
        assert dependencies.keys() == {"Apply", "Pick"}
        provider_view, = dependencies["Pick"]["signature"]["parameters"]
        assert provider_view["source_path"] == "reader.ku" and provider_view["passing"] == "view"
        code = execute([compiler, "emit-unit", manifest, function], caller).stdout
        assert b"static KuBootstrapOptionalInt ku_static_reader_" in code and b".capture={0}" in code
        if function == "Declared":
            assert re.search(rb"ku_projection_string_drop\(&l[0-9]+\.capture\)", code), code
    builder = Path(__file__).resolve().parents[1] / "build_program.py"
    binary = caller / ("static-reader.exe" if os.name == "nt" else "static-reader")
    execute([sys.executable, "-B", builder, manifest, "--compiler", compiler, "--cc", cc, "-o", binary], caller)
    assert execute([binary], caller).stdout.replace(b"\r\n", b"\n") == b"null\n" * 3 + b"0\n" * 3 + b"7\n" * 3

    (project / "other.ku").write_text(provider, encoding="utf-8")
    manifest.write_text("main.ku\nreader.ku\nother.ku\n", encoding="utf-8")
    header += 'import other from "./other"\nimport { ByValue } from "./reader.ku"\n'
    for source, diagnostic in [
        ("fn Bad(&row: Row): int | null { return Apply(other.Pick, row) }", b"linked_static_reader_signature"),
        ("fn Bad(&row: Row): int | null { return Apply(ByValue, row) }", b"linked_static_reader_signature"),
        ("fn Bad(picker: int, &row: Row): int | null { return Apply(picker.Pick, row) }", b"linked_field_type"),
        ("fn Bad(Pick: fn(&Row): int | null, &row: Row): int | null {\n"
         "    first = Apply(Pick, row)\n"
         "    return Apply(Pick, row)\n"
         "}", b"unavailable_linked_binding"),
        ("fn Bad(&row: Row): int | null {\n"
         "    reader: fn(&Row): int | null = picker.Pick\n"
         "    first = Apply(reader, row)\n"
         "    return Apply(reader, row)\n"
         "}", b"unavailable_linked_binding"),
        ("fn Bad(prefix: str): fn(&Row): int | null {\n"
         "    nested = fn(&row: Row) {\n"
         "        if (prefix.len() == 0) { return null }\n"
         "        return Apply(picker.Pick, row)\n"
         "    }\n"
         "    return nested\n"
         "}", b"linked_static_reader_scope"),
    ]:
        main.write_text(header + source + "\n", encoding="utf-8")
        rejected = execute([compiler, "emit-unit-plan", manifest, "Bad"], caller, 1)
        assert diagnostic in rejected.stderr and not rejected.stdout, (source, rejected.stderr)


def exercise_linked_reader_factory(compiler, cc, caller):
    project = caller / "linked-reader-factory"
    project.mkdir()
    (project / "select.ku").write_text('''\
struct Row { text: str, line: int }
fn Select(prefix: str): fn(&Row): int | null {
    read = fn(&row: Row) {
        if (!row.text.starts_with(prefix)) { return null }
        return row.line
    }
    reader = read
    return reader
}
fn EarlyUse(prefix: str): fn(&Row): int | null {
    read = fn(&row: Row) {
        if (!row.text.starts_with(prefix)) { return null }
        return row.line
    }
    println(prefix)
    return read
}
''', encoding="utf-8")
    (project / "use.ku").write_text('''\
import { Row, Select } from "./select.ku"
fn Choose(one: str, two: str, &row: Row): int | null {
    first = Select(one)
    second = Select(two)
    hit1 = first(row)
    hit2 = second(row)
    if (hit1 == null) { return hit2 }
    return hit1
}
''', encoding="utf-8")
    manifests = {"Choose": project / "use.txt", "Select": project / "select.txt"}
    manifests["Choose"].write_text("use.ku\nselect.ku\n", encoding="utf-8")
    manifests["Select"].write_text("select.ku\nuse.ku\n", encoding="utf-8")
    linker = Path(__file__).resolve().parents[1] / "link_c.py"
    link = [sys.executable, "-B", linker, "--cc", cc]
    units = []
    exports = {}
    for function, manifest in manifests.items():
        plan = project / f"{function}.plan.json"
        code = project / f"{function}.c"
        plan.write_bytes(execute([compiler, "emit-unit-plan", manifest, function], caller).stdout)
        code.write_bytes(execute([compiler, "emit-unit", manifest, function], caller).stdout)
        exports[function] = json.loads(plan.read_bytes())["export"]
        units.extend(["--unit", plan, code])
    entry = exports["Choose"]
    view = entry["signature"]["parameters"][2]
    produced = exports["Select"]["signature"]["result"]
    assert produced["kind"] == "NullableRecordViewReader" and not exports["Select"]["signature"]["returns_result"]
    assert (produced["source_path"], produced["node"]) == (view["source_path"], view["node"])
    row = view["c_type"].removeprefix("const ").removesuffix("*")
    host = project / "host.c"
    host.write_text(f'''\
#include <assert.h>
#include <stdlib.h>
#include <string.h>
static unsigned int dropped;
static uint8_t *owned[2];
void check_factory_free(void *ptr) {{
    assert(ptr && (ptr == owned[0] || ptr == owned[1]));
    if (ptr == owned[0]) owned[0] = NULL;
    else owned[1] = NULL;
    ++dropped;
    free(ptr);
}}
static KuProjectionString prefix(const char *text, size_t slot) {{
    size_t len = strlen(text);
    uint8_t *storage = malloc(len);
    assert(storage);
    memcpy(storage, text, len);
    owned[slot] = storage;
    return (KuProjectionString){{storage, len, len, 1}};
}}
int main(void) {{
    const char *one = "\\345\\211\\215\\360\\237\\231\\202";
    const char *two = "\\345\\210\\253\\360\\237\\207\\250\\360\\237\\207\\263";
    const char *texts[] = {{one, two, "none"}};
    const int64_t lines[] = {{0, 7, 2}};
    for(size_t i = 0; i < sizeof(lines)/sizeof(*lines); ++i) {{
        KuProjectionString first = prefix(one, 0);
        KuProjectionString second = prefix(two, 1);
        {row} input = {{ .f0 = {{(uint8_t *)texts[i], strlen(texts[i]), 0, 0}}, .f1 = lines[i] }};
        dropped = 0;
        KuBootstrapOptionalInt out = {entry["symbol"]}(first, second, &input);
        assert(out.is_null == (i == 2) && out.value == (i == 2 ? 0 : lines[i]));
        assert(dropped == 2 && !owned[0] && !owned[1]);
    }}
    return 0;
}}
''', encoding="ascii")
    binary = project / ("owned-reader-factory.exe" if os.name == "nt" else "owned-reader-factory")
    instrumentation = b"#include <stdlib.h>\nextern void check_factory_free(void *);\n#define free check_factory_free\n"
    tracked = []
    for item in units:
        if isinstance(item, Path) and item.suffix == ".c":
            checked = item.with_suffix(".tracked.c")
            checked.write_bytes(instrumentation + item.read_bytes() + b"\n#undef free\n")
            item = checked
        tracked.append(item)
    execute([*link, *tracked, "--entry", entry["source_path"], str(entry["node"]), "--caller", host, "-o", binary], caller)
    assert execute([binary], caller).stdout == b""
    used_later = execute([compiler, "emit-unit", manifests["Select"], "EarlyUse"], caller, 1)
    assert not used_later.stdout and b"capture is still used" in used_later.stderr


def exercise_linked_owned_string_for(compiler, cc, caller):
    project = caller / "linked-string-for"
    project.mkdir()
    (project / "main.ku").write_text('''\
import { string } from "std"
fn ReadAfter(): null! {
    items = string.chars("甲🙂")
    for item in items { println(item) }
    println(items[0])
    return ok(null)
}
fn ReplaceDuring(): null! {
    items = string.chars("乙🙂")
    for item in items {
        println(item)
        items = string.chars("替")
    }
    println(items[0])
    return ok(null)
}
fn First(items: [str]): str {
    for item in items { return item }
    return "empty"
}
fn main(): null! {
    ReadAfter()?
    ReplaceDuring()?
    println(First(string.chars("早🙂")))
    println(First(string.chars("")))
    return ok(null)
}
''', encoding="utf-8")
    manifest = project / "modules.txt"
    manifest.write_text("main.ku\n", encoding="utf-8")
    builder = Path(__file__).resolve().parents[1] / "build_program.py"
    binary = project / ("linked-string-for.exe" if os.name == "nt" else "linked-string-for")
    execute([sys.executable, "-B", builder, "--compiler", compiler, "--cc", cc, manifest, "-o", binary], caller)
    assert execute([binary], caller).stdout.decode().splitlines() == ["甲", "🙂", "甲", "乙", "🙂", "替", "早", "empty"]


def exercise_linked_program(compiler, cc, caller):
    project = caller / "linked-application"
    project.mkdir()
    main = project / "main.ku"
    provider = project / "读取.ku"
    main.write_text('import { CloneLabel, Location, ReadSource, Note } from "./读取"\n'
                    'fn main(): null! { source = ReadSource("input.txt")?; println(source);\n'
                    '    unused = 7; noted = Note()?;\n'
                    '    println(CloneLabel());\n'
                    '    line = Location(0, true); if (line != null) { println(str(line)) }\n'
                    '    println(str(Location(0, false))); return ok(null) }\n',
                    encoding="utf-8")
    provider.write_text('import { fs, string } from "std"\n'
                        'fn Clean(&text: str): str { return string.trim(text) }\n'
                        'fn ReadSource(path: str): str! { raw = fs.read(path)?; result = Clean(raw);'
                        ' raw = "released"; return ok(result) }\n'
                        'fn Note(): int! { println("note"); return ok(7) }\n'
                        'fn CloneLabel(): str { return "前缀🙂".clone() }\n'
                        'fn Location(line: int, present: bool): int | null { println("lookup");\n'
                        '    if (!present) { return null }; return line }\n', encoding="utf-8")
    manifest = project / "modules.txt"
    valid = f"{main.name}\r\n{provider.name}\r\n".encode("utf-8")
    manifest.write_bytes(valid)
    builder = Path(__file__).resolve().parents[1] / "build_program.py"
    options = [sys.executable, "-B", builder, "--compiler", compiler, "--cc", cc, manifest]
    binary = caller / ("linked-app.exe" if os.name == "nt" else "linked-app")
    execute([*options, "-o", binary], caller)
    original = hashlib.sha256(binary.read_bytes()).digest()

    # A partially listed project and an output alias must leave the valid
    # executable and manifest untouched.
    manifest.write_bytes(f"{main.name}\n".encode())
    failed = execute([*options, "-o", binary], caller, 1)
    assert b"import" in failed.stderr and hashlib.sha256(binary.read_bytes()).digest() == original
    manifest.write_bytes(valid)
    refused = execute([*options, "-o", manifest], caller, 1)
    assert b"output refers to an input file" in refused.stderr and manifest.read_bytes() == valid
    reserved = project / "std/fs.ku"
    reserved.parent.mkdir()
    reserved.write_bytes(b'fn read(path: str): str! { return ok(path) }\n')
    manifest.write_bytes(valid + b"std/fs.ku\r\n")
    collision = execute([*options, "-o", binary], caller, 1)
    assert b"linked_builtin_collision" in collision.stderr
    assert hashlib.sha256(binary.read_bytes()).digest() == original

    main.unlink()
    provider.unlink()
    input_file = caller / "input.txt"
    for raw, expected in (("\t　清单🙂\u200b\u00a0\r\n", "清单🙂\u200b\n"),
                          ("\u0085　\u00a0", "\n")):
        input_file.write_bytes(raw.encode("utf-8"))
        success = execute([binary], caller)
        assert success.stdout.replace(b"\r\n", b"\n") == (expected + "note\n前缀🙂\nlookup\n0\nlookup\nnull\n").encode("utf-8")
        assert not success.stderr
    input_file.unlink()
    missing = execute([binary], caller, 1)
    assert not missing.stdout and b"failed to read file" in missing.stderr

    bare = caller / "linked-bare-checked-app"
    bare.mkdir()
    bare_source, bare_manifest = bare / "main.ku", bare / "modules.txt"
    bare_manifest.write_text("main.ku\n", encoding="utf-8")
    bare_binary = bare / binary.name
    for source, status, stderr in (
        ('fn main(): null! { skip = 7; return ok(null) }\n', 0, b""),
        ('fn main(): null! { fail { domain: "entry", code: "failed", message: "错误🙂" } }\n',
         1, "错误🙂\n".encode("utf-8")),
    ):
        bare_source.write_text(source, encoding="utf-8")
        execute([sys.executable, "-B", builder, "--compiler", compiler, "--cc", cc,
                 bare_manifest, "-o", bare_binary], caller)
        result = execute([bare_binary], caller, status)
        assert not result.stdout and result.stderr.replace(b"\r\n", b"\n") == stderr


def exercise_direct_owned_moves(compiler, caller):
    project = caller / "direct-owned-moves"
    project.mkdir()
    source = project / "source.ku"
    provider = project / "provider.ku"
    manifest = project / "modules.txt"
    prelude = ('struct Pair { left: str right: str }\n'
               'fn Take(value: str): null! { println(value); return ok(null) }\n'
               'fn Read(&value: str): null! { println(value); return ok(null) }\n'
               'fn Inspect(&value: Pair): null! { println(value.left); return ok(null) }\n')
    simple = 'fn main(): null! { %s; return ok(null) }\n'
    branches = ('fn Visit(flag: bool): null! { word = "first"; %s; return ok(null) }\n'
                'fn main(): null! { return Visit(false) }\n')
    pair = 'pair = Pair { left: "l", right: "r" }; Take(pair.left)?; '
    object_field = 'item = { text: "text", other: "another" }; Take(item.text)?; '
    loop = simple % 'values = [1, 2]; %s'
    cases = [
        (simple % 'word = "first"; Take(word)?; Read(word)?', b"'word'"),
        (simple % (pair + 'Read(pair.left)?'), b"'pair.left'"),
        (simple % ('pair = Pair { left: "old", right: "r" }; '
                   + pair + 'Read(pair.left)?'), b"'pair.left'"),
        (simple % ('item: Pair = Pair { left: "l", right: "r" }; Take(item.left)?; Read(item.left)?'), b"'item.left'"),
        ('fn Use(value: Pair): null! { Take(value.left)?; Read(value.left)?; return ok(null) }\n'
         + simple % 'println("main")', b"'value.left'"),
        (branches % 'if (flag) { Take(word)? }; Read(word)?', b"'word'"),
        (simple % (object_field + 'println(item.text)'), b""),
        (simple % (object_field + 'Take(item.text)?'), b""),
        (simple % 'word = "first"; Read(word)?; Read(word)?', b""),
        (simple % 'word = "first"; Take(word.clone())?; Read(word)?', b""),
        (simple % 'word = "first"; Take(word)?; word = "new"; Read(word)?', b""),
        (branches % 'Take(word)?; if (flag) { word: str = "shadow"; Read(word)? }', b""),
        (branches % 'if (flag) { Take(word)? } else { Read(word)? }', b""),
        (branches % 'if (flag) { Take(word)?; return ok(null) }; Read(word)?', b""),
        (simple % (pair + 'Read(pair.right)?; pair.left = "new"; Read(pair.left)?'), b""),
        (simple % (pair + 'Inspect(pair)?'), b""),
        (loop % 'for tick in values { word = "item"; Take(word)?; Read(word)? }', b"'word'"),
        (loop % 'for tick in values { word = "item"; Take(word)?; println(str(tick)) }', b""),
        (loop % 'for tick in values { word = "item"; Take(word)?; word = "new"; Read(word)?; println(str(tick)) }', b""),
        (loop % ('word = "outside"; for tick in values { Read(word)?; '
                 'if (tick == 1) { word: str = "inner"; Take(word)? }; Read(word)? }; Read(word)?'), b""),
        (loop % ('word = "outside"; reader = fn(): null! { Read(word)?; return ok(null) }; '
                 'for tick in values { local = "item"; Take(local)?; reader()? }; Read(word)?'), b""),
    ]
    manifest.write_text("source.ku\n", encoding="ascii")
    for body, diagnostic in cases:
        source.write_text(prelude + body, encoding="ascii")
        for command, argument in [("check", source), ("check-project", manifest)]:
            result = execute([compiler, command, argument], caller, int(bool(diagnostic)))
            assert not result.stdout
            if diagnostic:
                assert b"read_after_owned_transfer" in result.stderr and diagnostic in result.stderr
            else:
                assert not result.stderr

    source.write_text(prelude + 'fn Use(&value: Pair): null! { Take(value.left)?; return ok(null) }\n'
                      + simple % 'println("main")', encoding="ascii")
    for command, argument in [("check", source), ("check-project", manifest)]:
        result = execute([compiler, command, argument], caller, 1)
        assert b"owned_transfer_from_view" in result.stderr

    provider.write_text('fn Take(value: str): null! { println(value); return ok(null) }\n', encoding="ascii")
    source.write_text('import { Take as Sink } from "./provider.ku"\n'
                      + simple % 'word = "first"; Sink(word)?; println(word)', encoding="ascii")
    manifest.write_text("source.ku\nprovider.ku\n", encoding="ascii")
    result = execute([compiler, "check-project", manifest], caller, 1)
    assert b"read_after_owned_transfer" in result.stderr and b"'word'" in result.stderr


def exercise_partial_record_view(compiler, cc, caller):
    project = caller / "partial-record-view"
    project.mkdir()
    (project / "main.ku").write_text('''\
struct Pair { left: str right: str }
fn Take(value: str): null! { println(value); return ok(null) }
fn Inspect(&value: Pair): null! { println(value.left); return ok(null) }
fn main(): null! {
    pair = Pair { left: "old", right: "sibling" }
    Take(pair.left)?
    Inspect(pair)?
    pair.left = "replaced"
    moved = pair.left
    println(moved)
    Inspect(pair)?
    return ok(null)
}
''', encoding="utf-8")
    manifest = project / "modules.txt"
    manifest.write_text("main.ku\n", encoding="utf-8")
    builder = Path(__file__).resolve().parents[1] / "build_program.py"
    binary = project / ("partial-record-view.exe" if os.name == "nt" else "partial-record-view")
    execute([sys.executable, "-B", builder, "--compiler", compiler, "--cc", cc, manifest, "-o", binary], caller)
    assert execute([binary], caller).stdout.splitlines() == [b"old", b"old", b"replaced", b"replaced"]


def exercise_plain_null_entry(compiler, cc, caller):
    project = caller / "plain-null-entry"
    project.mkdir()
    (project / "main.ku").write_text('''\
import { Note, Find } from "./helpers"
fn main(): null {
    done = Note("noted")
    println(str(Find(true)))
    println(str(Find(false)))
    return done
}
''', encoding="utf-8")
    (project / "helpers.ku").write_text('''\
fn Note(text: str): null { println(text); return null }
fn Find(present: bool): int | null { if (!present) { return null }; return 0 }
''', encoding="utf-8")
    manifest = project / "modules.txt"
    manifest.write_text("main.ku\nhelpers.ku\n", encoding="utf-8")
    root = Path(__file__).resolve().parents[1]
    execute([compiler, "check-project", manifest], caller)
    plan = json.loads(execute([compiler, "emit-unit-plan", manifest, "main"], caller).stdout)
    signature = plan["export"]["signature"]
    assert signature["parameters"] == [] and not signature["returns_result"]
    assert signature["result"]["source_name"] == "null" and signature["result"]["c_type"] == "uint8_t"
    dependencies = {decl["source_name"]: decl["signature"] for decl in plan["dependencies"]}
    assert not dependencies["Note"]["returns_result"] and dependencies["Note"]["result"] == signature["result"]
    assert not dependencies["Find"]["returns_result"] and dependencies["Find"]["result"]["kind"] == "NullableInt"

    binary = project / ("plain-null-entry.exe" if os.name == "nt" else "plain-null-entry")
    execute([sys.executable, "-B", root / "build_program.py", "--compiler", compiler,
             "--cc", cc, manifest, "-o", binary], caller)
    assert execute([binary], caller).stdout.splitlines() == [b"noted", b"0", b"null"]

    # A plain result and a checked result share the Ku name null, but their
    # C carriers differ. Reject a receipt with those carriers mixed up.
    signature["result"]["c_type"] = "KuBootstrapResultNull"
    invalid = project / "invalid-plan.json"
    invalid.write_text(json.dumps(plan), encoding="utf-8")
    emitted = project / "main.c"
    emitted.write_bytes(execute([compiler, "emit-unit", manifest, "main"], caller).stdout)
    failed = execute([sys.executable, "-B", root / "link_c.py", "--unit", invalid, emitted, "--check-only"], caller, 1)
    assert not failed.stdout and b"null requires the by-value uint8_t carrier" in failed.stderr


def exercise_parameter_bindings(compiler, cc, caller):
    source = caller / "parameter-binding.ku"
    manifest = caller / "parameter-binding.txt"
    manifest.write_text(source.name + "\n", encoding="utf-8")
    rejected_writes = [
        "fn Check(value: int): null! { value = value + 1; return ok(null) }",
        "fn Check(value: [int]): null! { value = value.push(42); return ok(null) }",
        'fn Check(value: str): null! { value += "suffix"; return ok(null) }',
        "fn Check(value: int): null! { while (value > 0) { value = value - 1 }; return ok(null) }",
        'fn Check(value: str): null! { if (true) { value += "suffix" }; return ok(null) }',
        "fn Check(value: int, gate: bool): null! { if (gate) { value: int = 1; value += 1 }"
        " else { value = 4 }; return ok(null) }",
        'fn Check(value: int): null! { try { fail "problem" } catch (other) { value = 0 } return ok(null) }',
        "fn Check(value: int): null! { fn write(): int { value = 1; return value }; return ok(null) }",
        "fn Check(value: int): null! { write = fn(): int { value += 1; return value }; return ok(null) }",
        "fn Check(value: int): null! { for item in [1] { break; value = item }; return ok(null) }",
        "fn Check(value: int): null! { {field: value} = {field: 1}; return ok(null) }",
    ]
    diagnostic = b"cannot assign to immutable parameter 'value'"
    for body in rejected_writes:
        source.write_text(body + "\n", encoding="utf-8")
        for args in (("check", source), ("check-project", manifest),
                     ("emit-unit", manifest, "Check"), ("emit-unit-plan", manifest, "Check")):
            result = execute([compiler, *args], caller, 1)
            assert not result.stdout and diagnostic in result.stderr, result.stderr

    accepted_checks = [
        "fn Check(value: int): int { for value in [1, 2] { value += 1 }; return value }",
        "fn Check(value: int, gate: bool): int { if (gate) { value: int = 1; value += 1 }; return value }",
        'fn Check(value: int): int { try { fail "problem" } catch (value) { println(value.message) } return value }',
        "fn Check(value: int): int { fn read(): int { value: int = 1; value += 1; return value }; return read() }",
        "fn Check(value: int, gate: bool): int { while (gate) { break; value += 1 }; return value }",
        "fn Check(value: int, gate: bool): int { while (gate) { if (gate) { break } else { continue };"
        " value += 1 }; return value }",
        'fn Check(value: int): int { try { fail "problem"; value += 1 } catch (err) { println(err.message) } return value }',
    ]
    for body in accepted_checks:
        source.write_text(body + "\n", encoding="utf-8")
        execute([compiler, "check", source], caller)
        execute([compiler, "check-project", manifest], caller)
    source.write_text('''\
fn Check(value: int): int! {
    local = value
    while (local > 0) { local = local - 1 }
    return ok(local)
}
''', encoding="utf-8")
    execute([compiler, "check-project", manifest], caller)
    plan = caller / "parameter-binding.json"
    code = caller / "parameter-binding.c"
    plan.write_bytes(execute([compiler, "emit-unit-plan", manifest, "Check"], caller).stdout)
    code.write_bytes(execute([compiler, "emit-unit", manifest, "Check"], caller).stdout)
    entry = json.loads(plan.read_bytes())["export"]
    host = caller / "parameter-binding-host.c"
    host.write_text(f'''
int main(void) {{
    KuBootstrapResult_int64_t result = {entry["symbol"]}(3);
    return !(result.ok && result.value == 0);
}}
''', encoding="ascii")
    linker = Path(__file__).resolve().parents[1] / "link_c.py"
    program = caller / ("parameter-binding.exe" if os.name == "nt" else "parameter-binding")
    execute([sys.executable, "-B", linker, "--cc", cc, "--unit", plan, code,
             "--entry", entry["source_path"], str(entry["node"]), "--caller", host, "-o", program], caller)
    execute([program], caller)


def exercise_owned_recovery(compiler, cc, caller):
    (caller / "owners.ku").write_text('''\
struct Slot { name: str, live: bool }
struct Pool { slots: [Slot], note: str }
struct Detail { texts: [str], ready: bool }
struct Frame { body: Detail, key: str }
fn Replace(): int! {
    slots: [Slot] = [Slot { name: "old", live: true }]
    pool = Pool { slots: slots, note: "active" }
    pool.slots[0].name = "new value"
    if (pool.slots[0].name == "new value" && pool.note == "active" && pool.slots[0].live) { return ok(15) }
    return ok(0)
}
fn Recover(abort: bool): int! {
    try {
        if (abort) { fail { domain: "fixture", code: "recover", message: "leave catch" } }
        return ok(11)
    } catch (err) {
        if (err.code != "recover") { fail err }
    }
    return ok(29)
}
fn CloneNested(): int! {
    texts: [str] = []
    text = "item" + str(8)
    texts = texts.push(text)
    source = Frame { body: Detail { texts: texts, ready: true }, key: "key" }
    copy = source.clone()
    source.key = "changed"
    if (copy.body.ready && copy.body.texts[0] == "item8" && copy.key == "key" && source.key == "changed") {
        return ok(38)
    }
    return ok(0)
}
fn CloneRows(): int! {
    name = "task" + str(6)
    rows: [Slot] = [Slot { name: name, live: true }]
    source = Pool { slots: rows, note: "active" }
    copy = source.clone()
    source.slots[0].name = "finished"
    if (copy.slots[0].name == "task6" && copy.slots[0].live && source.slots[0].name == "finished") {
        return ok(56)
    }
    return ok(0)
}
''', encoding="utf-8")
    manifest = caller / "owners.txt"
    manifest.write_text("owners.ku\n", encoding="utf-8")
    linker = Path(__file__).resolve().parents[1] / "link_c.py"
    cases = {
        "Replace": [("", 15)],
        "Recover": [("false", 11), ("true", 29)],
        "CloneNested": [("", 38)],
        "CloneRows": [("", 56)],
    }
    for name, expected in cases.items():
        plan = caller / f"{name}.plan.json"
        code = caller / f"{name}.c"
        plan.write_bytes(execute([compiler, "emit-unit-plan", manifest, name], caller).stdout)
        code.write_bytes(execute([compiler, "emit-unit", manifest, name], caller).stdout)
        entry = json.loads(plan.read_bytes())["export"]
        signature = entry["signature"]
        assert signature["returns_result"] and signature["result"]["source_name"] == "int"
        checks = []
        for i, (args, value) in enumerate(expected):
            checks.append(
                f'    KuBootstrapResult_int64_t r{i} = {entry["symbol"]}({args});\n'
                f"    assert(r{i}.ok && r{i}.value == {value});\n")
        host = caller / "owners-host.c"
        host.write_text("#include <assert.h>\nint main(void) {\n" +
                        "".join(checks) + "    return 0;\n}\n", encoding="ascii")
        binary = caller / ("linked-owners.exe" if os.name == "nt" else "linked-owners")
        execute([sys.executable, "-B", linker, "--cc", cc, "--unit", plan, code,
                 "--entry", entry["source_path"], str(entry["node"]), "--caller", host, "-o", binary], caller)
        assert execute([binary], caller).stdout == b""


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
        exercise_glob_import(compiler, cc, caller)
        exercise_namespace_import(compiler, cc, caller)
        exercise_qualified_types(compiler, cc, caller)
        exercise_qualified_constructor(compiler, cc, caller)
        exercise_linked_program(compiler, cc, caller)
        exercise_partial_record_view(compiler, cc, caller)
        exercise_plain_null_entry(compiler, cc, caller)
        exercise_checked_bool(compiler, cc, caller)
        exercise_linked_nullable_int(compiler, cc, caller)
        exercise_linked_owned_reader(compiler, cc, caller)
        exercise_static_reader_argument(compiler, cc, caller)
        exercise_linked_reader_factory(compiler, cc, caller)
        exercise_linked_owned_string_for(compiler, cc, caller)
        exercise_parameter_bindings(compiler, cc, caller)
        exercise_direct_owned_moves(compiler, caller)
        exercise_owned_recovery(compiler, cc, caller)
        print("bootstrap argv, module checks, signature and standalone C verified")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--ku", default="ku")
    parser.add_argument("--cc", default="cc")
    args = parser.parse_args()
    exercise(executable(args.ku), executable(args.cc))


if __name__ == "__main__":
    main()
