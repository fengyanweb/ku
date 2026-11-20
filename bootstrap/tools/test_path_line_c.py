#!/usr/bin/env python3
"""Exercise the emitted manifest line with the actual Stage0 path validator."""

import argparse
import json
import os
from pathlib import Path
import re
import shutil
import tempfile

import link_c
from compiler_driver.build import executable
from test_char_prefix_c import c_string, run


def fixture(bootstrap, path):
    entry = (bootstrap / "semantics/project_manifest.ku").resolve()
    routes = [entry]
    seen = {entry}
    for source in routes:
        target = path / source.relative_to(bootstrap)
        target.parent.mkdir(parents=True, exist_ok=True)
        shutil.copyfile(source, target)
        for relative in re.findall(r'(?:from|import) "([.][^"\n]+\.ku)"', source.read_text(encoding="utf-8")):
            child = (source.parent / relative).resolve()
            if child not in seen:
                routes.append(child)
                seen.add(child)
    (path / "modules.txt").write_text("".join(source.relative_to(bootstrap).as_posix() + "\n"
                                              for source in routes), encoding="utf-8")
    (path / "function.txt").write_text("PrepareManifestPath\n", encoding="ascii")


def stage0_oracle(ku, cc, path):
    (path / "oracle.ku").write_text('''import { PrepareManifestPath } from "./semantics/project_manifest.ku"
import { CheckProjectManifestPath } from "./semantics/project_paths.ku"
import { Span } from "./stage2/span.ku"
fn OracleValidator(&path: str, at: Span): null! { return CheckProjectManifestPath(path, at) }
fn OracleLine(path: str, at: Span): str! { return PrepareManifestPath(path, at) }
fn main() {}
''', encoding="ascii")
    run([ku, "build", "--native", "--offline", "oracle.ku"], path)
    if cc.stem.lower() in {"cl", "clang-cl"}:
        output = path / "oracle.obj"
        run([cc, "/nologo", "/std:c11", "/Od", "/Dmain=ku_stage0_main", "/c", "oracle.c",
             f"/Fo{output}"], path)
    else:
        output = path / "oracle.o"
        run([cc, "-std=c11", "-O0", "-Dmain=ku_stage0_main", "-c", "oracle.c", "-o", output], path)
    return output


def native_dependency(source, export, dependency, path):
    context = export["signature"]["parameters"][1]["c_type"]
    assert dependency["signature"]["parameters"][1] == export["signature"]["parameters"][1]
    end = source.index(f"}} {context};") + len(context) + 3
    layout = source[source.rfind("typedef struct", 0, end):end]
    (path / "provider.c").write_text(f'''#include <stdint.h>
#include <stdbool.h>
#include <stddef.h>
typedef struct KuString {{ uint8_t* ptr; size_t len; size_t capacity; uint8_t storage; }} KuString;
typedef KuString KuProjectionString;
typedef struct KuError {{ KuString domain; KuString code; KuString message; }} KuError;
typedef struct KuResult_null {{ bool ok; uint8_t value; KuError error; }} KuBootstrapResultNull;
{layout}
extern KuBootstrapResultNull OracleValidator(const KuProjectionString*, {context});
KuBootstrapResultNull {dependency['symbol']}(const KuProjectionString* line, {context} at) {{
    return OracleValidator(line, at);
}}
''', encoding="ascii")
    (path / "provider.json").write_text(json.dumps({
        "schema": link_c.PLAN, "mode": "stage0-validator-fixture", "export": dependency, "dependencies": [],
    }), encoding="ascii")
    return context


def caller(export, context, path):
    samples = ["", "\r", "a.ku", "名/中🙂.ku\r", "a.ku\r\r", "../bad.ku",
               "a//b.ku", "a\\b.ku", "bad", "bad\0name.ku", "dir/ spaced .ku"]
    rows = ",\n".join(f"    {{{c_string(s)}, {len(s.encode('utf-8'))}u}}" for s in samples)
    (path / "caller.c").write_text(f'''
#include <assert.h>
extern KuResult_str OracleLine(KuProjectionString, {context});
static const struct {{ const char* text; size_t len; }} sample[] = {{
{rows}
}};
static bool same(KuString a, KuString b) {{
    return a.len == b.len && (!a.len || !memcmp(a.ptr, b.ptr, a.len));
}}
static KuString owner(const char* raw, size_t len) {{
    uint8_t* bytes = malloc(len ? len : 1u);
    assert(bytes);
    if (len) memcpy(bytes, raw, len);
    return (KuString){{bytes, len, len, 1u}};
}}
static void retire(KuResult_str out) {{
    if (out.ok) ku_scalar_string_drop(out.value);
    else {{
        ku_scalar_string_drop(out.error.domain);
        ku_scalar_string_drop(out.error.code);
        ku_scalar_string_drop(out.error.message);
    }}
}}
int main(void) {{
    {context} at = {{37, 13, 123, 37, 18, 128}};
    for (size_t i = 0; i < sizeof(sample) / sizeof(sample[0]); ++i) {{
        KuString line = owner(sample[i].text, sample[i].len);
        uintptr_t address = (uintptr_t)line.ptr;
        KuResult_str out = {export['symbol']}(line, at);
        KuResult_str ref = OracleLine(owner(sample[i].text, sample[i].len), at);
        assert(out.ok == ref.ok);
        if (out.ok) {{
            assert(same(out.value, ref.value));
            if (sample[i].len && sample[i].text[sample[i].len - 1] != '\\r')
                assert((uintptr_t)out.value.ptr == address);
        }} else {{
            assert(!out.value.ptr && same(out.error.domain, ref.error.domain)
                && same(out.error.code, ref.error.code) && same(out.error.message, ref.error.message));
        }}
        retire(out);
        retire(ref);
    }}
    return 0;
}}
''', encoding="ascii")


def exercise(ku, cc, root):
    bootstrap = Path(__file__).resolve().parents[1]
    for stage in ("stage1", "stage2", "stage3", "semantics"):
        shutil.copytree(bootstrap / stage, root / stage)
    path = root / "tools"
    path.mkdir()
    fixture(bootstrap, path)
    shutil.copyfile(bootstrap / "tools/path_line_c.ku", path / "path_line_c.ku")
    suffix = ".exe" if os.name == "nt" else ""
    emitter = path / f"path-line{suffix}"
    # Only the producer uses the faster development build. The emitted C
    # and both typed link wrappers below go through the linker's C11 O2 gate.
    run([ku, "build", "--debug", "--offline", "path_line_c.ku", "-o", emitter], path)
    source = run([emitter], path).stdout
    (path / "plan.txt").touch()
    raw = run([emitter], path).stdout
    (path / "plan.txt").unlink()
    plan = json.loads(raw)
    export = plan["export"]
    dependency, = plan["dependencies"]
    assert export["source_name"] == "PrepareManifestPath"
    assert dependency["source_name"] == "CheckProjectManifestPath"
    context = native_dependency(source.decode("ascii"), export, dependency, path)
    caller(export, context, path)
    (path / "line.c").write_bytes(source)
    (path / "line.json").write_bytes(raw)
    oracle = stage0_oracle(ku, cc, path)
    linked = root / f"linked{suffix}"
    assert link_c.main([
        "--unit", str(path / "line.json"), str(path / "line.c"),
        "--unit", str(path / "provider.json"), str(path / "provider.c"),
        "--entry", export["source_path"], str(export["node"]), "--caller", str(path / "caller.c"),
        "--cc", str(cc), "-o", str(linked), f"--ldflag={oracle}",
    ]) == 0
    run([linked], root)
    entry = path / "semantics/project_manifest.ku"
    source_ku = entry.read_text(encoding="utf-8")
    changed = source_ku.replace("line.len() - 1", "line.len() - 2", 1)
    assert changed != source_ku
    entry.write_text(changed, encoding="utf-8")
    rejected = run([emitter], path, 1)
    assert not rejected.stdout and b"unsupported_path_line" in rejected.stderr


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--ku", default="ku")
    parser.add_argument("--cc", default="cc")
    options = parser.parse_args()
    with tempfile.TemporaryDirectory(prefix="ku-bootstrap-path-line-") as scratch:
        exercise(executable(options.ku), executable(options.cc), Path(scratch))
    print("PrepareManifestPath matches the actual Stage0 validator and owned result")
