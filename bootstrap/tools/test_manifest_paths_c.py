#!/usr/bin/env python3
"""Compare the emitted manifest scanner and line preparer with Stage0."""

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
from test_path_line_c import fixture


def emit(tool, sandbox, function, stem):
    (sandbox / "function.txt").write_text(function + "\n", encoding="utf-8")
    source = run([tool], sandbox).stdout
    flag = sandbox / "plan.txt"
    flag.touch()
    try:
        plan = run([tool], sandbox).stdout
    finally:
        flag.unlink()
    (sandbox / f"{stem}.c").write_bytes(source)
    (sandbox / f"{stem}.json").write_bytes(plan)
    return source.decode("ascii"), json.loads(plan)


def stage0_oracle(ku, cc, sandbox, flags):
    path = sandbox / "oracle.ku"
    path.write_text('''import { ManifestPaths } from "./semantics/project_manifest.ku"
import { CheckProjectManifestPath } from "./semantics/project_paths.ku"
import { RejectName } from "./semantics/names.ku"
import { Span } from "./stage2/span.ku"
fn OracleManifest(raw: str, at: Span): [str]! { return ManifestPaths(raw, at) }
fn OracleValidator(&path: str, at: Span): null! { return CheckProjectManifestPath(path, at) }
fn OracleReject(code: str, message: str, at: Span): null! { return RejectName(code, message, at) }
fn main() {}
''', encoding="ascii")
    run([ku, "build", "--native", "--offline", path.name], sandbox)
    source = path.with_suffix(".c").read_text(encoding="utf-8")
    span = re.search(r"^KuResult_array_str OracleManifest\(KuString raw, (\w+) at\);$", source, re.M)[1]
    span_definition = re.search(r"^struct " + span + r" \{[^}]+\};$", source, re.M)[0]
    c_decl = "\n".join((
        span_definition,
        re.search(r"^typedef struct \{[^}]+\} KuArray_str;$", source, re.M)[0],
        re.search(r"^struct KuResult_array_str \{[^}]+\};$", source, re.M)[0],
    ))
    if cc.stem.lower() in {"cl", "clang-cl"}:
        obj = sandbox / "oracle.obj"
        run([cc, "/nologo", "/std:c11", "/Od", "/Dmain=ku_stage0_main",
             *flags, "/c", "oracle.c", f"/Fo{obj}"], sandbox)
    else:
        obj = sandbox / "oracle.o"
        run([cc, "-std=c11", "-O0", "-Dmain=ku_stage0_main",
             *flags, "-c", "oracle.c", "-o", obj], sandbox)
    return obj, span, span_definition, c_decl


def stage0_providers(sandbox, source, plan, line_plan, span, span_definition):
    export = plan["export"]
    prepare, reject = plan["dependencies"]
    validator, = line_plan["dependencies"]
    context = export["signature"]["parameters"][1]["c_type"]
    assert prepare["source_name"] == "PrepareManifestPath"
    assert validator["source_name"] == "CheckProjectManifestPath"
    assert reject["source_name"] == "RejectName"
    layout = re.search(r"^typedef struct \{[^}]+\} " + context + r";$", source, re.M)[0]
    # This adapter is test-only. The checked parameter identity is shared, but
    # the generated layout and Stage0 can use different C struct tags.
    common = f'''#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>
typedef struct KuString {{ uint8_t* ptr; size_t len; size_t capacity; uint8_t storage; }} KuString;
typedef KuString KuProjectionString;
typedef struct KuError {{ KuString domain; KuString code; KuString message; }} KuError;
typedef struct KuResult_null {{ bool ok; uint8_t value; KuError error; }} KuBootstrapResultNull;
{layout}
{span_definition}
'''
    providers = {
        "validate": (validator, f'''
extern KuBootstrapResultNull OracleValidator(const KuProjectionString*, struct {span});
KuBootstrapResultNull {validator["symbol"]}(const KuProjectionString* line, {context} at) {{
    return OracleValidator(line, (struct {span}){{at.f0, at.f1, at.f2, at.f3, at.f4, at.f5}});
}}
'''),
        "reject": (reject, f'''
extern KuBootstrapResultNull OracleReject(KuProjectionString, KuProjectionString, struct {span});
KuBootstrapResultNull {reject["symbol"]}(KuProjectionString code, KuProjectionString message, {context} at) {{
    return OracleReject(code, message, (struct {span}){{at.f0, at.f1, at.f2, at.f3, at.f4, at.f5}});
}}
'''),
    }
    for name, (decl, body) in providers.items():
        signature = decl["signature"]
        assert signature["parameters"][-1] == export["signature"]["parameters"][-1]
        assert signature["result"]["c_type"] == "KuBootstrapResultNull"
        (sandbox / f"{name}.c").write_text(common + body, encoding="ascii")
        (sandbox / f"{name}.json").write_text(json.dumps({
            "schema": link_c.PLAN, "mode": "stage0-test-provider",
            "export": decl, "dependencies": [],
        }), encoding="ascii")
    return context


def caller(sandbox, export, context, span, c_decl):
    limit = "".join(f"file-{i}.ku\n" for i in range(64))
    long = "a" * 509 + ".ku"
    samples = [
        ("", "project_path", 0),
        ("\r\n\n\r", "project_path", 0),
        ("alpha.ku\n名/中🙂.ku\r\nwith space.ku", "", 3),
        (long + "\r\n", "", 1),
        ("good.ku\n" + long + "\r\r\n", "project_path", 0),
        ("good.ku\n../bad.ku\n" + long + "\r\r", "project_path", 0),
        ("good.ku\n名/中🙂.ku\n名/中🙂.ku\r\n" + long + "\r\r", "ambiguous_module", 0),
        ("good.ku\nno\0name.ku", "project_path", 0),
        (limit, "", 64),
        (limit + "../bad.ku", "project_path", 0),
        (limit + "file-0.ku", "project_limit", 0),
    ]
    samples = ",\n".join(f"    {{{c_string(raw)}, {len(raw.encode('utf-8'))}u, \"{code}\", {count}u}}"
                         for raw, code, count in samples)
    (sandbox / "caller.c").write_text(f'''
#include <assert.h>
{c_decl}
extern struct KuResult_array_str OracleManifest(KuString, struct {span});

static const struct {{ const char* text; size_t len; const char* error; size_t count; }} samples[] = {{
{samples}
}};

static KuString owner(const char* text, size_t len) {{
    uint8_t* copy = malloc(len ? len : 1u);
    assert(copy);
    if (len) memcpy(copy, text, len);
    return (KuString){{copy, len, len, 1u}};
}}
static bool same(KuString a, KuString b) {{
    return a.len == b.len && (!a.len || !memcmp(a.ptr, b.ptr, a.len));
}}
static void retire(KuError error) {{
    ku_scalar_string_drop(error.domain);
    ku_scalar_string_drop(error.code);
    ku_scalar_string_drop(error.message);
}}
static void verify(const char* text, size_t len, const char* error, size_t count) {{
    const {context} at = {{37, 13, 123, 37, 18, 128}};
    const struct {span} original_at = {{37, 13, 123, 37, 18, 128}};
    KuBootstrapResultStringArray out = {export["symbol"]}(owner(text, len), at);
    struct KuResult_array_str ref = OracleManifest(owner(text, len), original_at);
    assert(out.ok == ref.ok && out.ok == !*error);
    if (out.ok) {{
        assert(out.value.len == count && ref.value.len == count);
        for (size_t i = 0; i < count; ++i) {{
            assert(same(out.value.data[i], ref.value.data[i]));
            ku_scalar_string_drop(out.value.data[i]);
            ku_scalar_string_drop(ref.value.data[i]);
        }}
        free(out.value.data);
        free(ref.value.data);
    }} else {{
        assert(!out.value.len && !out.value.data && !out.value.capacity);
        assert(ku_scalar_string_equal(out.error.code.ptr, out.error.code.len,
                                      (const uint8_t*)error, strlen(error)));
        assert(same(out.error.domain, ref.error.domain)
            && same(out.error.code, ref.error.code) && same(out.error.message, ref.error.message));
        retire(out.error);
        retire(ref.error);
    }}
}}
static bool inject_growth_failure;
static size_t growth_calls;
static void* live[16];
static size_t live_count;

static void check_failure_cleanup(void) {{
    assert(growth_calls == 2u && live_count == 0u);
}}

int main(int argc, char** argv) {{
    (void)argv;
    if (argc > 1) {{
        const {context} at = {{37, 13, 123, 37, 18, 128}};
        const char* text = "a.ku\\nb.ku\\nc.ku\\nd.ku\\ne.ku";
        inject_growth_failure = true;
        assert(atexit(check_failure_cleanup) == 0);
        (void){export["symbol"]}(owner(text, strlen(text)), at);
        assert(!"expected array growth failure");
    }}
    for (size_t i = 0; i < sizeof(samples) / sizeof(samples[0]); ++i)
        verify(samples[i].text, samples[i].len, samples[i].error, samples[i].count);
    char file[32769];
    memset(file, '\\n', sizeof(file));
    verify(file, 32768u, "project_path", 0u);
    verify(file, sizeof(file), "project_limit", 0u);
    return 0;
}}

#undef malloc
#undef realloc
#undef free
extern void* malloc(size_t);
extern void* realloc(void*, size_t);
extern void free(void*);

void* ku_manifest_test_malloc(size_t size) {{
    void* pointer = malloc(size);
    if (inject_growth_failure && pointer) {{
        assert(live_count < sizeof(live) / sizeof(live[0]));
        live[live_count++] = pointer;
    }}
    return pointer;
}}

void* ku_manifest_test_realloc(void* pointer, size_t size) {{
    if (!inject_growth_failure) return realloc(pointer, size);
    if (++growth_calls == 2u) {{
        assert(pointer && live_count == 7u);
        return NULL;
    }}
    assert(!pointer);
    return ku_manifest_test_malloc(size);
}}

void ku_manifest_test_free(void* pointer) {{
    if (inject_growth_failure && pointer) {{
        size_t index = 0;
        while (index < live_count && live[index] != pointer) ++index;
        assert(index < live_count);
        live[index] = live[--live_count];
    }}
    free(pointer);
}}
''', encoding="ascii")


def exercise(ku, cc, root, sanitize):
    bootstrap = Path(__file__).resolve().parents[1]
    for stage in ("stage1", "stage2", "stage3", "semantics"):
        shutil.copytree(bootstrap / stage, root / stage)
    sandbox = root / "tools"
    sandbox.mkdir()
    fixture(bootstrap, sandbox)
    for name in ("manifest_paths_c.ku", "manifest_paths_shape.ku", "path_line_c.ku"):
        shutil.copyfile(bootstrap / "tools" / name, sandbox / name)
    suffix = ".exe" if os.name == "nt" else ""
    manifest = sandbox / f"manifest-paths{suffix}"
    line = sandbox / f"path-line{suffix}"
    # The emitted units use the strict C11 O2 linker even when their Ku
    # producer is built with the development profile to bound test time.
    run([ku, "build", "--debug", "--offline", "manifest_paths_c.ku", "-o", manifest], sandbox)
    run([ku, "build", "--debug", "--offline", "path_line_c.ku", "-o", line], sandbox)
    source, plan = emit(manifest, sandbox, "ManifestPaths", "manifest")
    _, line_plan = emit(line, sandbox, "PrepareManifestPath", "line")
    flags = ["-fsanitize=address,undefined", "-fno-sanitize-recover=all"] if sanitize else []
    original, span, span_definition, c_decl = stage0_oracle(ku, cc, sandbox, flags)
    context = stage0_providers(sandbox, source, plan, line_plan, span, span_definition)
    export = plan["export"]
    assert export["signature"]["result"]["c_type"] == "KuBootstrapResultStringArray"
    caller(sandbox, export, context, span, c_decl)
    linked = root / f"linked{suffix}"
    args = []
    for unit in ("manifest", "line", "validate", "reject"):
        args += ["--unit", str(sandbox / f"{unit}.json"), str(sandbox / f"{unit}.c")]
    defines = "/D" if cc.stem.lower() in {"cl", "clang-cl"} else "-D"
    tracked = [f"{defines}{name}=ku_manifest_test_{name}" for name in ("malloc", "realloc", "free")]
    assert link_c.main([
        *args, "--entry", export["source_path"], str(export["node"]),
        "--caller", str(sandbox / "caller.c"), "--cc", str(cc), "-o", str(linked),
        f"--ldflag={original}", *(f"--cflag={flag}" for flag in flags + tracked),
        *(f"--ldflag={flag}" for flag in flags),
    ]) == 0
    run([linked], root)
    injected = run([linked, "inject-growth-failure"], root, 1)
    assert not injected.stdout and b"array allocation failed" in injected.stderr
    # Keep the CLI's own input manifest usable while mutating only the
    # scanned function. This lowering must reject different accumulation.
    entry = sandbox / "semantics/project_manifest.ku"
    original_source = entry.read_text(encoding="utf-8")
    changed = original_source.replace("line = line + character", "line = character + line", 1)
    assert changed != original_source
    entry.write_text(changed, encoding="utf-8")
    (sandbox / "function.txt").write_text("ManifestPaths\n", encoding="ascii")
    rejected = run([manifest], sandbox, 1)
    assert not rejected.stdout and b"unsupported_manifest_scan" in rejected.stderr


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--ku", default="ku")
    parser.add_argument("--cc", default="cc")
    parser.add_argument("--sanitize", action="store_true")
    options = parser.parse_args()
    with tempfile.TemporaryDirectory(prefix="ku-bootstrap-manifest-paths-") as scratch:
        exercise(executable(options.ku), executable(options.cc), Path(scratch), options.sanitize)
    print("ManifestPaths: ordered results and diagnostics match the actual Stage0 program")
