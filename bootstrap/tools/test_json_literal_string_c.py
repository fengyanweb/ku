#!/usr/bin/env python3
"""Compare checked JSON literal strings with the installed Stage0 native ABI."""

import argparse
import json
import os
from pathlib import Path
import shutil
import tempfile

from compiler_driver.build import executable
from test_char_prefix_c import run
from test_path_line_c import fixture


CODE = r'''
#include <assert.h>
#include <stdlib.h>
#include <string.h>

typedef KuResult_str (*Make)(void);
extern KuResult_str __ORACLE__(void);

static bool reject;
static unsigned calls;
static size_t reserved;
static bool reallocated;
void *tracked_malloc(size_t size) {
    ++calls;
    reserved = size;
    reallocated = false;
    return reject ? NULL : malloc(size);
}
void *tracked_realloc(void *value, size_t size) {
    ++calls;
    reserved = size;
    reallocated = true;
    if (reject) return NULL;
    return realloc(value, size);
}
static KuResult_str call(Make function, bool deny) {
    reject = deny;
    calls = 0;
    reserved = 0;
    reallocated = false;
    return function();
}
static bool same(KuString left, KuString right) {
    return left.storage == right.storage && left.capacity == right.capacity &&
        left.len == right.len && (!left.len || memcmp(left.ptr, right.ptr, left.len) == 0);
}
int main(void) {
    KuResult_str generated = call(__GENERATED__, false);
    unsigned count = calls;
    size_t size = reserved;
    bool via_realloc = reallocated;
    KuResult_str original = call(__ORACLE__, false);
    assert(generated.ok && original.ok && same(generated.value, original.value));
    assert(calls == count && reserved == size && reallocated == via_realloc);
    if (generated.value.storage == 1u) {
        assert(calls == 1u && generated.value.ptr != original.value.ptr);
        free(generated.value.ptr);
        free(original.value.ptr);
    } else { assert(!calls && !generated.value.len); }

    generated = call(__GENERATED__, true);
    count = calls;
    size = reserved;
    via_realloc = reallocated;
    original = call(__ORACLE__, true);
    assert(generated.ok == original.ok);
    assert(calls == count && reserved == size && reallocated == via_realloc);
    if (generated.ok) {
        assert(!calls && same(generated.value, original.value));
    } else {
        assert(calls == 1u && same(generated.error.domain, original.error.domain));
        assert(same(generated.error.code, original.error.code));
        assert(same(generated.error.message, original.error.message));
        assert(!generated.value.ptr && !original.value.ptr);
    }
    return 0;
}
'''


def target_flags(cc, flags, optimized):
    if cc.stem.lower() in {"cl", "clang-cl"}:
        return ["/nologo", "/std:c11", "/O2" if optimized else "/Od", *flags]
    return ["-std=c11", "-O2" if optimized else "-O0", *flags]


def instrument(cc):
    pre = "/D" if cc.stem.lower() in {"cl", "clang-cl"} else "-D"
    return [f"{pre}malloc=tracked_malloc", f"{pre}realloc=tracked_realloc"]


def compile_obj(cc, source, output, path, flags, optimized):
    options = target_flags(cc, flags, optimized)
    if cc.stem.lower() in {"cl", "clang-cl"}:
        run([cc, *options, "/c", source, f"/Fo{output}"], path)
    else:
        run([cc, *options, "-c", source, "-o", output], path)


def emit(tool, sandbox, function, output):
    (sandbox / "function.txt").write_text(function + "\n", encoding="utf-8")
    program = run([tool], sandbox).stdout
    flag = sandbox / "plan.txt"
    flag.touch()
    try:
        plan = json.loads(run([tool], sandbox).stdout)
    finally:
        flag.unlink()
    assert plan["export"]["source_name"] == function and not plan["dependencies"]
    assert plan["export"]["signature"]["result"]["c_type"] == "KuBootstrapResult_KuProjectionString"
    (sandbox / output).write_bytes(program)
    return plan["export"]["symbol"]


def exercise(ku, cc, root, sanitize):
    bootstrap = Path(__file__).resolve().parents[1]
    for stage in ("stage1", "stage2", "stage3", "semantics"):
        shutil.copytree(bootstrap / stage, root / stage)
    sandbox = root / "tools"
    sandbox.mkdir()
    fixture(bootstrap, sandbox)
    shutil.copyfile(bootstrap / "tools/json_literal_string_c.ku", sandbox / "json_literal_string_c.ku")
    suffix = ".exe" if os.name == "nt" else ""
    tool = sandbox / f"emitter{suffix}"
    run([ku, "build", "--debug", "--offline", "json_literal_string_c.ku", "-o", tool], sandbox)
    paths = (sandbox / "modules.txt").read_text(encoding="utf-8").splitlines()

    def select(entry):
        (sandbox / "modules.txt").write_text(
            "\n".join([entry, *(p for p in paths if p != entry)]) + "\n", encoding="utf-8"
        )

    literals = {
        "Escaped": r'"\uD83D\uDE42/甲\n"',
        "Plain": '"甲/🙂"',
        "Empty": '  ""  ',
        "Bound": '"\\u0000' + ("x" * 63) + '"',
    }
    variants = ['import "std.json"']
    for name, raw in literals.items():
        variants.append(f'fn {name}(): str! {{ box = json.parse({json.dumps(raw, ensure_ascii=False)})?\n'
                        '    return box.as_str() }')
    variants.append('fn WrongType(): str! { box = json.parse("null")? return box.as_str() }')
    variants.append('fn Malformed(): str! { box = json.parse("invalid")? return box.as_str() }')
    raw = '"' + "x" * 65 + '"'
    variants.append(f'fn Oversize(): str! {{ box = json.parse({json.dumps(raw)})? return box.as_str() }}')
    variants.append('fn Dynamic(): str! { box = json.parse(str(5))? return box.as_str() }')
    variants.append('fn Shadowed(): str! { json = json.parse("\\\"abc\\\"")? return json.as_str() }')
    (sandbox / "cases.ku").write_text("\n".join(variants) + "\n", encoding="utf-8")
    functions = {"ProjectPathNul": "semantics/project_paths.ku", **dict.fromkeys(literals, "cases.ku")}
    oracle = sandbox / "oracle.ku"
    oracle.write_text('import { ProjectPathNul } from "./semantics/project_paths.ku"\n'
                      'import { ' + ", ".join(literals) + ' } from "./cases.ku"\n'
                      + "".join(f"fn Ref{name}(): str! {{ return {name}() }}\n" for name in functions)
                      + "fn main() {}\n", encoding="ascii")
    run([ku, "build", "--native", "--offline", "oracle.ku"], sandbox)
    msvc = cc.stem.lower() in {"cl", "clang-cl"}
    obj = ".obj" if msvc else ".o"
    flags = ["-fsanitize=address,undefined", "-fno-sanitize-recover=all"] if sanitize else []
    oracle_obj = sandbox / ("oracle" + obj)
    define = "/D" if msvc else "-D"
    compile_obj(cc, "oracle.c", oracle_obj, sandbox,
                [*flags, *instrument(cc), f"{define}main=ku_stage0_main"], False)
    for name, entry in functions.items():
        select(entry)
        source = sandbox / f"{name}.c"
        symbol = emit(tool, sandbox, name, source.name)
        native_obj = sandbox / (name + obj)
        warning = ["/WX"] if msvc else ["-pedantic-errors", "-Wall", "-Wextra", "-Werror"]
        compile_obj(cc, source, native_obj, sandbox, [*flags, *warning, *instrument(cc)], True)
        program = source.read_text(encoding="ascii")
        end = program.index(f"KuResult_str {symbol}(")
        harness = program[:end] + f"extern KuResult_str {symbol}(void);\n" + CODE
        harness = harness.replace("__ORACLE__", "Ref" + name).replace("__GENERATED__", symbol)
        (sandbox / "caller.c").write_text(harness, encoding="ascii")
        binary = sandbox / f"compare{suffix}"
        args = [cc, *target_flags(cc, flags + warning, True), "caller.c", native_obj, oracle_obj]
        run([*args, f"/Fe{binary}"] if msvc else [*args, "-o", binary], sandbox)
        run([binary], sandbox)

    select("cases.ku")
    for name in ("WrongType", "Malformed", "Oversize", "Dynamic", "Shadowed"):
        (sandbox / "function.txt").write_text(name + "\n", encoding="ascii")
        rejected = run([tool], sandbox, 1)
        assert not rejected.stdout and b"unsupported_json_literal_string" in rejected.stderr


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--ku", default="ku")
    parser.add_argument("--cc", default="cc")
    parser.add_argument("--sanitize", action="store_true")
    options = parser.parse_args()
    with tempfile.TemporaryDirectory(prefix="ku-bootstrap-json-literal-") as workspace:
        exercise(executable(options.ku), executable(options.cc), Path(workspace), options.sanitize)
    print("Checked JSON literal C matches Stage0 decoded bytes, owned results and allocation errors")
