#!/usr/bin/env python3
"""Compare an emitted reverse character scan with both Stage0 paths."""

import argparse
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile

import link_c
from compiler_driver.build import executable, windows_process_owner


def run(args, cwd, status=0):
    command = [os.fspath(arg) for arg in args]
    if os.name == "nt":
        result = windows_process_owner()(command, cwd, Path(command[0]).name, check=False)
    else:
        result = subprocess.run(command, cwd=cwd, capture_output=True, timeout=120)
    assert result.returncode == status, (command, result.stdout, result.stderr)
    return result


def c_string(text):
    return '"' + "".join(f"\\{byte:03o}" for byte in text.encode("utf-8")) + '"'


def emit(tool, fixtures, selected, function):
    (fixtures / "modules.txt").write_text("\n".join(selected) + "\n", encoding="utf-8")
    (fixtures / "function.txt").write_text(function + "\n", encoding="utf-8")
    source = run([tool], fixtures).stdout
    flag = fixtures / "plan.txt"
    flag.touch()
    try:
        plan = run([tool], fixtures).stdout
    finally:
        flag.unlink()
    return source, plan


def link_and_compare(ku, cc, tool, fixtures, consumer, selected, function, values, expected):
    source, plan = emit(tool, fixtures, selected, function)
    identity = json.loads(plan)
    export = identity["export"]
    assert export["source_path"] == selected[0] and export["source_name"] == function
    assert not identity["dependencies"]
    signature = export["signature"]
    assert signature["returns_result"] and signature["result"]["c_type"] == "KuResult_str"

    oracle = fixtures / "oracle.ku"
    oracle.write_text(
        f'import {{ {function} }} from "./{selected[0]}"\nfn main(): null! {{\n'
        + "".join(f'    print({function}({json.dumps(value, ensure_ascii=False)})?) print("|")\n'
                  for value in values)
        + "    return ok(null)\n}\n",
        encoding="utf-8",
    )
    truth = ("|".join(expected) + "|").encode("utf-8")
    assert run([ku, "run", oracle], fixtures).stdout == truth
    binary = fixtures / f"oracle-{function}{'.exe' if os.name == 'nt' else ''}"
    run([ku, "build", "--release", "--offline", oracle, "-o", binary], fixtures)
    assert run([binary], consumer).stdout == truth

    c_path = consumer / "prefix.c"
    plan_path = consumer / "plan.json"
    c_path.write_bytes(source)
    plan_path.write_bytes(plan)
    call_path = consumer / "call.c"
    call_path.write_text("""
#include "fs_read.h"
extern KuResult_str call_prefix(const KuBootstrapString*);
KuResult_str consume_prefix(const KuBootstrapString* path) { return call_prefix(path); }
""", encoding="ascii")
    # Exercise the concrete ABI from another translation unit using the
    # same tagged Result contract as other bootstrap providers.
    entry = dict(export, source_path="fixture/consumer.ku", node=1,
                 source_name="ConsumePrefix", symbol="consume_prefix")
    call_plan = consumer / "call.json"
    call_plan.write_text(json.dumps({
        "schema": link_c.PLAN, "mode": "fixture", "export": entry,
        "dependencies": [dict(export, symbol="call_prefix")],
    }), encoding="ascii")
    caller = consumer / "caller.c"
    rows = ",\n".join(f"    {{{c_string(value)}, {len(value.encode('utf-8'))}u}}" for value in values)
    caller.write_text(f"""
#include <stdio.h>
#include <stdlib.h>
static const struct {{ const char* data; size_t len; }} inputs[] = {{
{rows}
}};
int main(void) {{
    for (size_t i = 0; i < sizeof(inputs) / sizeof(inputs[0]); ++i) {{
        KuBootstrapString path = {{(uint8_t*)inputs[i].data, inputs[i].len, 0u, 0u}};
        KuResult_str result = consume_prefix(&path);
        if (!result.ok) return 1;
        if (result.value.len && fwrite(result.value.ptr, 1u, result.value.len, stdout) != result.value.len) return 2;
        if (result.value.storage == 1u) free(result.value.ptr);
        if (putchar('|') == EOF) return 3;
    }}
    return 0;
}}
""", encoding="ascii")
    linked = consumer / f"prefix{'.exe' if os.name == 'nt' else ''}"
    include = "/I" if cc.stem.lower() in {"cl", "clang-cl"} else "-I"
    assert link_c.main([
        "--unit", str(plan_path), str(c_path), "--unit", str(call_plan), str(call_path),
        "--entry", entry["source_path"], str(entry["node"]), "--caller", str(caller),
        "--output", str(linked), "--cc", str(cc), f"--cflag={include}{consumer}",
    ]) == 0
    assert run([linked], consumer).stdout == truth


def exercise(ku, cc, root):
    original = Path(__file__).resolve().parents[1]
    for stage in ("stage1", "stage2", "stage3", "semantics"):
        shutil.copytree(original / stage, root / stage)
    fixtures, consumer = root / "tools", root / "outside"
    fixtures.mkdir()
    consumer.mkdir()
    shutil.copyfile(original / "tools/compiler_driver/fs_read.h", consumer / "fs_read.h")
    tool_source = fixtures / "char_prefix_c.ku"
    shutil.copyfile(original / "tools" / tool_source.name, tool_source)
    tool = fixtures / f"char-prefix{'.exe' if os.name == 'nt' else ''}"
    run([ku, "build", "--release", "--offline", tool_source, "-o", tool], root)

    project = ["semantics/project_paths.ku", "semantics/names.ku", "stage2/span.ku",
               "stage2/ast.ku", "stage2/diagnostic.ku", "stage1/token.ku"]
    for relative in project:
        target = fixtures / relative
        target.parent.mkdir(exist_ok=True)
        shutil.copyfile(original / relative, target)
    link_and_compare(ku, cc, tool, fixtures, consumer, project, "ProjectParent",
                     ["", "word", "/", "a/b", "a/b/", "/甲/🙂", "a//b", "C:/目录/模块.ku",
                      "//server/share/file.ku", "x\\y/z", "路径\\孤立"],
                     ["", "", "", "a", "a/b", "/甲", "a/", "C:/目录", "//server/share", "x\\y", ""])

    unicode_source = '''import "std.string"
fn BeforeLastSmile(&input: str): str! {
    units = string.chars(input.clone())
    pos = units.len()
    while (pos > 0 && units[pos - 1] != "🙂") { pos = pos - 1 }
    if (pos == 0) { return ok("缺") }
    return string.slice(input.clone(), 0, pos - 1)
}
'''
    variant = fixtures / "variant.ku"
    variant.write_text(unicode_source, encoding="utf-8")
    link_and_compare(ku, cc, tool, fixtures, consumer, ["variant.ku"], "BeforeLastSmile",
                     ["", "中🫠", "🙂", "甲🙂乙", "甲🙂乙🙂"], ["缺", "缺", "", "甲", "甲🙂乙"])
    for changed in (unicode_source.replace("units[pos - 1]", "units[pos]"),
                    unicode_source.replace("pos = pos - 1 }", 'pos = pos - 1 print("extra") }'),
                    unicode_source.replace("0, pos - 1", "0, pos")):
        variant.write_text(changed, encoding="utf-8")
        failure = run([tool], fixtures, 1)
        assert not failure.stdout and b"unsupported_char_prefix" in failure.stderr


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--ku", default="ku")
    parser.add_argument("--cc", default="cc")
    options = parser.parse_args()
    with tempfile.TemporaryDirectory(prefix="ku-bootstrap-char-prefix-") as scratch:
        exercise(executable(options.ku), executable(options.cc), Path(scratch))
    print("ProjectParent and alternate Unicode scan match Stage0 and linked C")
