import contextlib
import copy
import hashlib
import io
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest
from unittest.mock import patch

import link_c


ATOM_CALLER = r"""
static KuProjectionString owner(const char *text) {
    size_t n = strlen(text);
    uint8_t *p = malloc(n ? n : 1);
    if (!p) exit(2);
    memcpy(p, text, n);
    return (KuProjectionString){p,n,n,KU_PROJECTION_STRING_OWNED};
}
static __TOKEN__ fixture(const char *kind, const char *text) {
    return (__TOKEN__){.f0=owner(kind),.f1=owner(text),.f2=7,.f3=7,
        .f4=4,.f5=11,.f6=8,.f7=1,.f8=13};
}
static void show(KuProjectionString value, int separator) {
    if (value.len) fwrite(value.ptr,1,value.len,stdout);
    if (separator) putchar(separator);
}
static int read_one(const char *kind,const char *text,bool stage3,bool accept) {
    __RESULT__ value = __ATOM__(fixture(kind,text),stage3);
    if (value.ok != accept) return 3;
    if (value.ok) {
        show(value.value.f0,'|');
        show(value.value.f1,'|');
        printf("%lld\n",(long long)value.value.f2);
    } else {
        show(value.error.domain,'|');
        show(value.error.code,'|');
        show(value.error.message,'\n');
    }
    __ATOM___drop(&value);
    return 0;
}
int main(void) {
    return read_one("Int","",false,true) ||
           read_one("Ident","\346\270\205\345\215\225\360\237\231\202",true,true) ||
           read_one("Unsupported","\344\273\273\345\212\241\360\237\231\202",false,false) ||
           read_one("Unsupported","\344\273\273\345\212\241\360\237\231\202",true,false);
}
"""


def builtin(name="int", c_type="int64_t"):
    return {"kind": "Builtin", "source_name": name, "source_path": None,
            "node": 0, "c_type": c_type, "passing": "value"}


def view(c_type, path="types/point.ku"):
    return {"kind": "Struct", "source_name": "Point", "source_path": path,
            "node": 9, "c_type": c_type, "passing": "view"}


def declaration(path, node, symbol, parameters):
    return {"source_path": path, "node": node, "source_name": "Read",
            "symbol": symbol, "signature": {"parameters": parameters,
                                            "result": builtin(), "returns_result": False}}


def plan(export, dependencies=()):
    return {"schema": link_c.PLAN, "mode": "test", "export": export,
            "dependencies": list(dependencies)}


class LinkTests(unittest.TestCase):
    def setUp(self):
        scratch = tempfile.TemporaryDirectory(prefix="ku-link-test-")
        self.addCleanup(scratch.cleanup)
        self.root = Path(scratch.name)
        self.output = self.root / ("run.exe" if os.name == "nt" else "run")
        self.left_path = "tools/左.ku"
        self.right_path = "tools/右.ku"
        self.left = declaration(self.left_path, 17, "ku_read", [view("const Provider*"), builtin()])
        self.right = declaration(self.right_path, 17, "ku_read", [view("const Provider*"), builtin()])
        self.input = declaration("app.ku", 58, "ku_use", [view("const Receiver *")])
        params = [view("const Receiver*"), builtin()]
        self.left_import = declaration(self.left_path, 17, "ku_left", params)
        self.right_import = declaration(self.right_path, 17, "ku_right", params)
        self.entry_args = self.unit(
            "input", plan(self.input, [self.left_import, self.right_import]),
            """#include <stdint.h>
               typedef struct { int64_t field; } Receiver;
               extern int64_t ku_left(const Receiver*, int64_t);
               extern int64_t ku_right(const Receiver*, int64_t);
               int64_t ku_use(const Receiver *v) { return ku_left(v, 4) + ku_right(v, 3); }\n""",
        )
        self.provider_code = """#include <stdint.h>
             typedef struct { int64_t field; } Provider;
             int64_t ku_read(const Provider *v, int64_t arg) { return v->field FACTOR arg; }\n"""
        self.left_args = self.unit("left", plan(self.left), self.provider_code.replace("FACTOR", "+"))
        self.right_args = self.unit("right", plan(self.right), self.provider_code.replace("FACTOR", "*"), receipt=True)
        self.caller = self.root / "caller.c"
        self.caller.write_text("int main(void) { Receiver data = {10}; return ku_use(&data) != 44; }\n", encoding="ascii")
        self.build_args = ["--entry", "app.ku", "58", "--caller", str(self.caller), "-o", str(self.output)]

    def unit(self, base, metadata, code, receipt=False):
        src = self.root / f"{base}.c"
        src.write_bytes(code.encode("utf-8"))
        path = self.root / f"{base}.json"
        if receipt:
            metadata = dict(metadata, schema=link_c.RECEIPT, c_sha256=hashlib.sha256(src.read_bytes()).hexdigest())
        path.write_text(json.dumps(metadata, ensure_ascii=False), encoding="utf-8")
        return ["--unit", str(path), str(src)]

    def change_plan(self, args, apply):
        path = Path(args[1])
        data = json.loads(path.read_text(encoding="utf-8"))
        apply(data)
        path.write_text(json.dumps(data), encoding="utf-8")

    def assert_preflight_error(self, units, diagnostic):
        old = b"preexisting binary"
        self.output.write_bytes(old)
        err = io.StringIO()
        with patch.object(link_c.subprocess, "run") as cc, contextlib.redirect_stderr(err):
            result = link_c.main([*units, *self.build_args, "--cc", "must-not-start"])
        self.assertEqual(result, 1)
        self.assertIn(diagnostic, err.getvalue())
        cc.assert_not_called()
        self.assertEqual(self.output.read_bytes(), old)

    def test_builtin_integer_array_view_has_its_own_carrier(self):
        integer = dict(builtin("int", "const KuArrayInt*"), kind="BuiltinArray", passing="view")
        strings = dict(builtin("str", "const KuGuardStringArray*"), kind="BuiltinArray", passing="view")
        self.assertNotEqual(link_c.TypeRef.read(integer, "view").semantic(),
                            link_c.TypeRef.read(strings, "view").semantic())
        with self.assertRaises(link_c.LinkError):
            link_c.TypeRef.read(dict(integer, c_type=strings["c_type"]), "view")
        owned = dict(integer, c_type="KuArrayInt", passing="value")
        self.assertNotEqual(link_c.TypeRef.read(integer, "view").semantic(),
                            link_c.TypeRef.read(owned, "value").semantic())
        with self.assertRaises(link_c.LinkError):
            link_c.TypeRef.read(owned, "checked", checked_result=True)
        with self.assertRaises(link_c.LinkError):
            link_c.TypeRef.read(dict(owned, c_type="KuGuardStringArray"), "value")

    def test_fs_read_provider_links_and_preserves_bytes_and_errors(self):
        path_type = dict(builtin("str", "const KuString*"), passing="view")
        result_type = builtin("str", "KuResult_str")
        provider = declaration("std/fs.ku", 1, "KuBootstrapFsRead", [path_type])
        provider["signature"].update(result=result_type, returns_result=True)
        dependency = dict(provider, symbol="read_builtin")
        consumer = declaration("manifest.ku", 1, "read_manifest", [path_type])
        consumer["signature"].update(result=result_type, returns_result=True)

        units = self.unit("consumer", plan(consumer, [dependency]), """
#include "fs_read.h"
extern KuResult_str read_builtin(const KuString*);
KuResult_str read_manifest(const KuString* path) { return read_builtin(path); }
""")
        self.assert_preflight_error(units, "missing provider for std/fs.ku#1")
        fs_builtin = ["--builtin", "fs.read"]
        duplicate = self.unit("extra-provider", plan(provider), "/* never compiled */\n", receipt=True)
        self.assert_preflight_error([*units, *fs_builtin, *duplicate], "duplicate provider for std/fs.ku#1")
        self.assert_preflight_error([*units, *fs_builtin, *fs_builtin], "duplicate provider for std/fs.ku#1")

        filename = "条目🙂.txt"
        content = "清单🙂".encode("utf-8") + b"\0\r\n"
        (self.root / filename).write_bytes(content)
        (self.root / "empty").write_bytes(b"")
        (self.root / "plain").write_bytes(b"not truncated")
        (self.root / "not-utf8").write_bytes(b"\xed\xa0\x80")
        (self.root / "exact-limit").write_bytes(b"x" * 1_000_000)
        (self.root / "over-limit").write_bytes(b"x" * 1_000_001)

        def c_bytes(value):
            return '"' + "".join(f"\\{byte:03o}" for byte in value) + '"'

        self.caller.write_text(r"""
#include <stdlib.h>
#include <string.h>
static KuString view(const char* bytes, size_t len) {
    return (KuString){(uint8_t*)bytes, len, 0, KU_STRING_STATIC};
}
static bool equal(KuString actual, const char* text) {
    size_t len = strlen(text);
    return actual.storage == KU_STRING_STATIC && actual.len == len &&
           !memcmp(actual.ptr, text, len);
}
static int fail(KuString path, const char* code) {
    KuResult_str out = read_manifest(&path);
    return out.ok || out.value.ptr || out.value.len || out.value.capacity ||
           !equal(out.error.domain, "fs") || !equal(out.error.code, code) ||
           !out.error.message.len;
}
int main(void) {
    static const char name[] = __NAME__;
    static const char expected[] = __CONTENT__;
    KuString path = view(name, sizeof(name) - 1u);
    KuResult_str out = read_manifest(&path);
    if (!out.ok || out.value.storage != KU_STRING_OWNED ||
        out.value.len != sizeof(expected) - 1u ||
        memcmp(out.value.ptr, expected, sizeof(expected) - 1u)) return 1;
    free(out.value.ptr);
    path = view("empty", 5u);
    out = read_manifest(&path);
    if (!out.ok || out.value.len) return 2;
    if (out.value.storage == KU_STRING_OWNED) free(out.value.ptr);
    path = view("exact-limit", 11u);
    out = read_manifest(&path);
    if (!out.ok || out.value.len != 1000000u ||
        out.value.storage != KU_STRING_OWNED || out.value.ptr[999999] != 'x') return 3;
    free(out.value.ptr);
    return fail(view("not-utf8", 8u), "read_failed") ||
           fail(view("plain\0ignored", 13u), "read_failed") ||
           fail(view("\377", 1u), "read_failed") ||
           fail(view("missing", 7u), "read_failed") ||
           fail(view("over-limit", 10u), "file_too_large");
}
""".replace("__NAME__", c_bytes(filename.encode("utf-8")))
   .replace("__CONTENT__", c_bytes(content)), encoding="ascii")
        self.assertEqual(link_c.main([
            *units, *fs_builtin, "--entry", "manifest.ku", "1", "--caller", str(self.caller),
            "-o", str(self.output),
        ]), 0)
        subprocess.run([self.output], cwd=self.root, check=True, timeout=5)

        def change_result(p):
            p["dependencies"][0]["signature"].update(returns_result=False)

        self.change_plan(units, change_result)
        self.assert_preflight_error([*units, *fs_builtin], "signature mismatch for std/fs.ku#1")

    def test_missing_and_duplicate_providers_are_rejected_before_compiling(self):
        self.assert_preflight_error([*self.entry_args, *self.left_args], "missing provider for tools/右.ku#17")
        self.assert_preflight_error([*self.entry_args, *self.left_args, *self.right_args, *self.left_args],
                                    "duplicate provider for tools/左.ku#17")

    def test_matching_spelling_does_not_make_structs_interchangeable(self):
        self.change_plan(self.left_args, lambda p: p["export"]["signature"]["parameters"][0].update(source_path="other/point.ku"))
        self.assert_preflight_error([*self.entry_args, *self.left_args, *self.right_args], "signature mismatch")

    def test_readonly_struct_array_identity_is_not_its_element_identity(self):
        def shape(metadata):
            metadata["export"]["signature"]["parameters"][0].update(kind="StructArray", c_type="const Array*")
        self.change_plan(self.left_args, shape)
        self.assert_preflight_error([*self.entry_args, *self.left_args, *self.right_args], "signature mismatch")

    def test_struct_array_value_links_and_view_cannot_replace_it(self):
        def owned_array(c_type):
            return dict(view(c_type), kind="StructArray", passing="value")

        entry = declaration("app.ku", 58, "consume", [owned_array("ReceiverArray")])
        imported = declaration(self.left_path, 17, "take", [owned_array("ReceiverArray")])
        provider = declaration(self.left_path, 17, "receive", [owned_array("ProviderArray")])
        common = """#include <stdint.h>
                    #include <stdlib.h>
                    typedef struct { int64_t value; } Item;
                    typedef struct {size_t len; Item *data; size_t capacity;} TYPE;
                 """
        units = self.unit(
            "owned-entry", plan(entry, [imported]), common.replace("TYPE", "ReceiverArray") + """
                extern int64_t take(ReceiverArray);
                int64_t consume(ReceiverArray items) { return take(items); }
            """,
        )
        exported = self.unit(
            "owned-provider", plan(provider), common.replace("TYPE", "ProviderArray") + """
                int64_t receive(ProviderArray items) {
                    int64_t result = items.data[0].value;
                    free(items.data);
                    return result;
                }
            """,
        )
        self.caller.write_text(
            "int main(void) { Item *item = malloc(sizeof(*item)); if (!item) return 2; "
            "*item = (Item){42}; return consume((ReceiverArray){1,item,1}) != 42; }\n",
            encoding="ascii",
        )
        self.assertEqual(link_c.main([*units, *exported, *self.build_args]), 0)
        self.assertEqual(subprocess.run([self.output], check=True, timeout=5).returncode, 0)
        self.change_plan(exported, lambda p: p["export"]["signature"]["parameters"][0].update(
            c_type="const ProviderArray*", passing="view"))
        self.assert_preflight_error([*units, *exported], "signature mismatch")

    def test_typed_int_pair_callback_forwards_the_live_environment(self):
        def callback(module):
            return dict(view(f"KuBootstrapIntPairCallback_KuStruct_m{module}_9"),
                        kind="IntPairResultCallback", passing="value")

        layout = """#include <stdbool.h>
#include <stdint.h>
typedef struct { int64_t value; } TOKEN;
typedef struct { bool ok; TOKEN value; int64_t error; } KuBootstrapResult_TOKEN;
typedef struct {
    KuBootstrapResult_TOKEN (*entry)(void*, int64_t, int64_t);
    void *environment;
} KuBootstrapIntPairCallback_TOKEN;
"""
        entry = declaration("app.ku", 58, "forward", [callback(1)])
        requested = declaration(self.left_path, 17, "apply", [callback(1)])
        supplied = declaration(self.left_path, 17, "invoke", [callback(4)])
        units = self.unit("callback-entry", plan(entry, [requested]), (layout + """
extern int64_t apply(KuBootstrapIntPairCallback_TOKEN);
int64_t forward(KuBootstrapIntPairCallback_TOKEN function) { return apply(function); }
""").replace("TOKEN", "KuStruct_m1_9"))
        provider_source = (layout + """
int64_t invoke(KuBootstrapIntPairCallback_TOKEN function) {
    KuBootstrapResult_TOKEN result = function.entry(function.environment, 4, 7);
    return result.ok ? result.value.value : result.error;
}
""").replace("TOKEN", "KuStruct_m4_9")
        provider = self.unit("callback-provider", plan(supplied), provider_source)
        self.caller.write_text("""
static KuBootstrapResult_KuStruct_m1_9 check(void *environment, int64_t a, int64_t b) {
    int64_t *owner = environment;
    *owner += 1;
    return (KuBootstrapResult_KuStruct_m1_9){true, {*owner + 2 * a - b}, 0};
}
int main(void) {
    int64_t environment = 39;
    const KuBootstrapIntPairCallback_KuStruct_m1_9 callback = {check, &environment};
    return forward(callback) != 41 || environment != 40;
}
""", encoding="ascii")
        self.assertEqual(link_c.main([*units, *provider, *self.build_args]), 0)
        subprocess.run([self.output], check=True, timeout=5)

        def wrong_result(metadata):
            metadata["export"]["signature"]["parameters"][0].update(source_path="other/point.ku")

        self.change_plan(provider, wrong_result)
        self.assert_preflight_error([*units, *provider], "signature mismatch")
        with self.assertRaisesRegex(link_c.LinkError, "cannot be a returned value"):
            link_c.Signature.read({"parameters": [], "result": callback(1), "returns_result": False}, "bad")

    def test_typed_int_array_callback_checks_nominal_argument_and_failure_record(self):
        def callback(module):
            return dict(view(f"KuBootstrapIntArrayCallback_KuStruct_m{module}_9"),
                        kind="IntArrayResultCallback", passing="value")

        code = """#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>
#include <stdlib.h>
typedef struct { size_t len; int64_t *data; size_t capacity; } KuArrayInt;
typedef struct { bool ok; KuArrayInt value; } KuBootstrapResult_KuArrayInt;
typedef struct {
    int append_status;
    bool out_of_memory, overflow, index_failed;
    const char *array_error;
    int64_t index;
    size_t length;
} KuLocalAppendFailure;
typedef struct { int64_t value; } TOKEN;
typedef struct {
    KuBootstrapResult_KuArrayInt (*entry)(void *, KuLocalAppendFailure *, KuArrayInt, int64_t, int64_t, TOKEN);
    void *environment;
    KuLocalAppendFailure *failure;
} KuBootstrapIntArrayCallback_TOKEN;
"""
        entry = declaration("app.ku", 58, "forward_array", [callback(1)])
        requested = declaration(self.left_path, 17, "apply_array", [callback(1)])
        supplied = declaration(self.left_path, 17, "invoke_array", [callback(4)])
        units = self.unit("array-callback-entry", plan(entry, [requested]), (code + """
extern int64_t apply_array(KuBootstrapIntArrayCallback_TOKEN);
int64_t forward_array(KuBootstrapIntArrayCallback_TOKEN f) { return apply_array(f); }
""").replace("TOKEN", "KuStruct_m1_9"))
        provider_code = (code + """
int64_t invoke_array(KuBootstrapIntArrayCallback_TOKEN f) {
    int64_t *data = malloc(sizeof(*data));
    if (!data) return -2;
    data[0] = 12;
    KuBootstrapResult_KuArrayInt result = f.entry(f.environment, f.failure,
        (KuArrayInt){1, data, 1}, 3, 10, (TOKEN){16});
    if (f.failure->index_failed) return -1;
    if (!result.ok) return -3;
    int64_t total = result.value.data[0];
    free(result.value.data);
    return total;
}
""").replace("TOKEN", "KuStruct_m4_9")
        provider = self.unit("array-callback-provider", plan(supplied), provider_code)
        self.caller.write_text("""
static KuBootstrapResult_KuArrayInt update(void *environment, KuLocalAppendFailure *failure, KuArrayInt input,
                                          int64_t a, int64_t b, KuStruct_m1_9 token) {
    int64_t *calls = environment;
    ++*calls;
    if (*calls > 1) {
        free(input.data);
        failure->index_failed = true;
        failure->index = a;
        return (KuBootstrapResult_KuArrayInt){0};
    }
    input.data[0] += a + b + token.value;
    return (KuBootstrapResult_KuArrayInt){true, input};
}
int main(void) {
    int64_t calls = 0;
    KuLocalAppendFailure failure = {0};
    KuBootstrapIntArrayCallback_KuStruct_m1_9 callback = {update, &calls, &failure};
    if (forward_array(callback) != 41 || calls != 1 || failure.index_failed) return 1;
    return forward_array(callback) != -1 || calls != 2 || !failure.index_failed || failure.index != 3;
}
""", encoding="ascii")
        self.assertEqual(link_c.main([*units, *provider, *self.build_args]), 0)
        subprocess.run([self.output], check=True, timeout=5)

        wrong_plan = copy.deepcopy(supplied)
        wrong_plan["signature"]["parameters"][0].update(source_path="other/point.ku")
        wrong = self.unit("array-callback-other-type", plan(wrong_plan), provider_code)
        self.assert_preflight_error([*units, *wrong], "signature mismatch")
        wrong = self.unit("array-callback-wrong-failure", plan(supplied),
                          provider_code.replace("int64_t index;", "uint64_t index;"))
        with contextlib.redirect_stderr(io.StringIO()) as error:
            self.assertEqual(link_c.main([*units, *wrong, *self.build_args]), 1)
        self.assertIn("C compiler exited", error.getvalue())
        with self.assertRaisesRegex(link_c.LinkError, "cannot be a returned value"):
            link_c.Signature.read({"parameters": [], "result": callback(1), "returns_result": False}, "bad")

    def test_parameter_order_and_result_status_are_checked(self):
        wrong = copy.deepcopy(self.left)
        wrong["signature"]["parameters"].reverse()
        args = self.unit("misordered", plan(wrong), self.provider_code)
        self.assert_preflight_error([*self.entry_args, *args, *self.right_args], "signature mismatch")
        wrong = copy.deepcopy(self.left)
        wrong["signature"]["returns_result"] = True
        args = self.unit("wrong-result", plan(wrong), self.provider_code)
        self.assert_preflight_error([*self.entry_args, *args, *self.right_args], "signature mismatch")

    def test_view_and_value_are_checked_independently_of_the_type_identity(self):
        def change(metadata):
            parameter = metadata["export"]["signature"]["parameters"][0]
            parameter.update(c_type="Provider", passing="value")
        self.change_plan(self.left_args, change)
        self.assert_preflight_error([*self.entry_args, *self.left_args, *self.right_args], "signature mismatch")

    def test_builtin_carrier_mismatch_is_rejected_before_compiling(self):
        self.change_plan(self.left_args, lambda p: p["export"]["signature"]["parameters"][1].update(c_type="int32_t"))
        self.assert_preflight_error([*self.entry_args, *self.left_args, *self.right_args], "signature mismatch")

    def test_builtin_string_array_requires_its_view_carrier(self):
        array = {"kind": "BuiltinArray", "source_name": "str", "source_path": None,
                 "node": 0, "passing": "view", "c_type": "const KuGuardStringArray*"}
        identity = link_c.TypeRef.read(array, "array").semantic()
        self.assertNotEqual(identity, link_c.TypeRef.read(builtin("str", "KuProjectionString"), "str").semantic())
        for change in ({"source_name": "int"}, {"source_path": "types.ku"},
                       {"passing": "value", "c_type": "KuGuardStringArray"},
                       {"c_type": "const OtherArray*"}):
            with self.assertRaisesRegex(link_c.LinkError, r"checked View \[str\] carrier"):
                link_c.TypeRef.read(dict(array, **change), "array")

    def test_checked_string_slice_uses_the_native_builtin(self):
        self.compiler()
        input_type = dict(builtin("str", "const KuProjectionString*"), passing="view")
        parameters = [input_type, builtin(), builtin()]
        result_type = builtin("str", "KuBootstrapResult_KuProjectionString")
        dependency = declaration("std/string.ku", 2, "ku_slice", parameters)
        dependency["signature"].update(result=result_type, returns_result=True)
        entry = declaration("app.ku", 58, "select", parameters)
        entry["signature"].update(result=result_type, returns_result=True)
        units = self.unit("string-slice", plan(entry, [dependency]), """
#include <stdbool.h>
#include <stdint.h>
#include <stddef.h>
#include <stdlib.h>
#include <string.h>
typedef struct { uint8_t *ptr; size_t len; size_t capacity; uint8_t storage; } KuProjectionString;
typedef struct { KuProjectionString domain, code, message; } KuBootstrapError;
typedef struct { bool ok; KuProjectionString value; KuBootstrapError error; } KuBootstrapResult_KuProjectionString;
extern KuBootstrapResult_KuProjectionString ku_slice(const KuProjectionString *, int64_t, int64_t);
KuBootstrapResult_KuProjectionString select(const KuProjectionString *input, int64_t start, int64_t end) {
    return ku_slice(input, start, end);
}
""")
        self.caller.write_text("""
#include <assert.h>
static bool text_equal(KuProjectionString value, const char *expected) {
    return value.len == strlen(expected) && !memcmp(value.ptr, expected, value.len);
}
static void reject(const KuProjectionString *input, int64_t start, int64_t end, const char *message) {
    KuBootstrapResult_KuProjectionString result = select(input, start, end);
    assert(!result.ok && !result.value.ptr && !result.value.len);
    assert(text_equal(result.error.domain, "string") && text_equal(result.error.code, "slice_error"));
    assert(text_equal(result.error.message, message));
    if (result.error.message.storage == 1) free(result.error.message.ptr);
}
int main(void) {
    const uint8_t bytes[] = {'A', 0xe4,0xb8,0xad, 0xf0,0x9f,0x99,0x82, 0, 0xc3,0xa9};
    uint8_t *data = malloc(sizeof bytes);
    assert(data);
    memcpy(data, bytes, sizeof bytes);
    const KuProjectionString input = {data, sizeof bytes, sizeof bytes, 1};
    KuBootstrapResult_KuProjectionString result = select(&input, 1, 4);
    assert(result.ok && result.value.len == 8 && result.value.capacity == 8 && result.value.storage == 1);
    assert(result.value.ptr != data + 1 && !memcmp(input.ptr, bytes, sizeof bytes));
    KuBootstrapResult_KuProjectionString empty = select(&input, 5, 5);
    assert(empty.ok && !empty.value.ptr && !empty.value.len && !empty.value.capacity && !empty.value.storage);
    reject(&input, 2, -1, "string.slice indexes must be >= 0");
    reject(&input, 7, 6, "string.slice start must be <= end");
    reject(&input, 1, INT64_MAX, "string.slice end 9223372036854775807 out of bounds for length 5");
    free(data);
    assert(!memcmp(result.value.ptr, bytes + 1, 8));
    free(result.value.ptr);

    uint8_t raw_bytes[] = {0x80, 'x'};
    const KuProjectionString raw = {raw_bytes, sizeof raw_bytes, 0, 0};
    result = select(&raw, 0, 1);
    assert(result.ok && result.value.len == 1 && result.value.ptr[0] == raw_bytes[0]);
    free(result.value.ptr);
    return 0;
}
""", encoding="ascii")
        feature = ["--builtin", "string.slice"]
        self.assert_preflight_error(units, "missing provider for std/string.ku#2")
        self.assertEqual(link_c.main([*units, *feature, *self.build_args]), 0)
        subprocess.run([self.output], check=True, timeout=5)
        self.change_plan(units, lambda p: p["dependencies"][0]["signature"]["parameters"][0]
                         .update(c_type="KuProjectionString", passing="value"))
        self.assert_preflight_error([*units, *feature], "signature mismatch for std/string.ku#2")

    def test_builtin_bytes_preserves_raw_data_and_has_a_distinct_plan_type(self):
        view_type = "const KuProjectionString*"
        text = dict(builtin("str", view_type), passing="view")
        data = dict(builtin("bytes", view_type), passing="view")
        take = declaration("std/bytes.ku", 3, "take_bytes", [text])
        take["signature"].update(result=builtin("bytes", "KuBootstrapResult_KuProjectionString"),
                                 returns_result=True)
        at = declaration("std/bytes.ku", 4, "byte_at", [data, builtin()])
        at["signature"].update(result=builtin("int", "KuBootstrapResult_int64_t"), returns_result=True)
        entry = declaration("app.ku", 58, "verify_bytes", [])
        units = self.unit("raw-bytes", plan(entry, [take, at]), """
#include <stdbool.h>
#include <stdint.h>
#include <stddef.h>
#include <stdlib.h>
#include <string.h>
#include <assert.h>
typedef struct { uint8_t *ptr; size_t len, capacity; uint8_t storage; } KuProjectionString;
typedef struct { KuProjectionString domain, code, message; } KuBootstrapError;
typedef struct { bool ok; KuProjectionString value; KuBootstrapError error; } KuBootstrapResult_KuProjectionString;
typedef struct { bool ok; int64_t value; KuBootstrapError error; } KuBootstrapResult_int64_t;
extern KuBootstrapResult_KuProjectionString take_bytes(const KuProjectionString *);
extern KuBootstrapResult_int64_t byte_at(const KuProjectionString *, int64_t);
static void reject(const KuProjectionString *data, int64_t index) {
    KuBootstrapResult_int64_t result = byte_at(data, index);
    assert(!result.ok && result.error.domain.len == 5 && !memcmp(result.error.domain.ptr, "bytes", 5));
    assert(result.error.code.len == 19 && !memcmp(result.error.code.ptr, "index_out_of_bounds", 19));
}
int64_t verify_bytes(void) {
    const uint8_t source[] = {'K', 0, 0xe4, 0xb8, 0xad, 0xf0, 0x9f, 0x99, 0x82};
    uint8_t *input = malloc(sizeof source);
    assert(input);
    memcpy(input, source, sizeof source);
    const KuProjectionString text = {input, sizeof source, sizeof source, 1};
    KuBootstrapResult_KuProjectionString result = take_bytes(&text);
    assert(result.ok && result.value.ptr != input && result.value.len == sizeof source);
    assert(result.value.capacity == sizeof source && result.value.storage == 1);
    free(input);
    for (size_t index = 0; index < sizeof source; index++) {
        KuBootstrapResult_int64_t value = byte_at(&result.value, (int64_t)index);
        assert(value.ok && value.value == source[index]);
    }
    reject(&result.value, -1);
    reject(&result.value, sizeof source);
    free(result.value.ptr);
    return 0;
}
""")
        self.caller.write_text("int main(void) { return verify_bytes() != 0; }\n", encoding="ascii")
        features = ["--builtin", "bytes.from_str", "--builtin", "bytes.get"]
        self.assert_preflight_error(units, "missing provider for std/bytes.ku#3")
        self.assertEqual(link_c.main([*units, *features, *self.build_args]), 0)
        subprocess.run([self.output], check=True, timeout=5)
        self.change_plan(units, lambda p: p["dependencies"][1]["signature"]["parameters"][0]
                         .update(source_name="str"))
        self.assert_preflight_error([*units, *features], "signature mismatch for std/bytes.ku#4")

    def test_checked_owned_string_array_result_across_two_c_units(self):
        result = dict(builtin("str", "KuBootstrapResultStringArray"), kind="BuiltinArray")
        provider = declaration("parts.ku", 8, "parts", [builtin()])
        consumer = declaration("consumer.ku", 3, "take", [builtin()])
        provider["signature"].update(result=result, returns_result=True)
        consumer["signature"].update(result=result, returns_result=True)
        abi = """#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>
#include <stdlib.h>
#include <string.h>
typedef struct { uint8_t* ptr; size_t len, capacity; uint8_t storage; } KuProjectionString;
typedef struct { size_t len; KuProjectionString* data; size_t capacity; } KuGuardStringArray;
typedef struct { KuProjectionString domain, code, message; } KuBootstrapError;
typedef struct { bool ok; KuGuardStringArray value; KuBootstrapError error; } KuBootstrapResultStringArray;
"""
        units = self.unit("array-provider", plan(provider), abi + """
KuBootstrapResultStringArray parts(int64_t choice) {
    KuBootstrapResultStringArray result = {0};
    if (choice < 0) {
        result.error.code = (KuProjectionString){(uint8_t*)"missing", 7, 0, 0};
        return result;
    }
    result.value.data = calloc(1, sizeof *result.value.data);
    if (!result.value.data) abort();
    result.value.data[0] = (KuProjectionString){malloc(3), 3, 3, 1};
    if (!result.value.data[0].ptr) abort();
    memcpy(result.value.data[0].ptr, "one", 3);
    result.value.len = result.value.capacity = 1;
    result.ok = true;
    return result;
}
""", receipt=True)
        units += self.unit("array-consumer", plan(consumer, [provider]), abi + """
extern KuBootstrapResultStringArray parts(int64_t);
KuBootstrapResultStringArray take(int64_t choice) { return parts(choice); }
""")
        self.caller.write_text("""
int main(void) {
    KuBootstrapResultStringArray value = take(1);
    if (!value.ok || value.value.len != 1 || value.value.capacity != 1 ||
        value.value.data[0].len != 3 || memcmp(value.value.data[0].ptr, "one", 3)) return 1;
    free(value.value.data[0].ptr);
    free(value.value.data);
    value = take(-1);
    return value.ok || value.value.data || value.error.code.len != 7 ||
           memcmp(value.error.code.ptr, "missing", 7);
}
""", encoding="ascii")
        args = ["--entry", "consumer.ku", "3", "--caller", str(self.caller), "-o", str(self.output)]
        self.assertEqual(link_c.main([*units, *args]), 0)
        self.assertEqual(subprocess.run([self.output], timeout=15).returncode, 0)
        with self.assertRaisesRegex(link_c.LinkError, r"checked owned \[str\]! result carrier"):
            link_c.Signature.read(dict(provider["signature"], returns_result=False), "plain result")
        with self.assertRaisesRegex(link_c.LinkError, r"checked owned \[str\]! result carrier"):
            link_c.Signature.read(dict(provider["signature"], parameters=[result]), "owned parameter")

    def test_missing_entry_and_ambiguous_local_symbol_are_rejected(self):
        with patch.object(link_c.subprocess, "run") as cc, contextlib.redirect_stderr(io.StringIO()) as err:
            status = link_c.main([*self.left_args, *self.right_args, *self.build_args])
        self.assertEqual(status, 1)
        self.assertIn("missing entry unit", err.getvalue())
        cc.assert_not_called()
        self.change_plan(self.entry_args, lambda p: p["dependencies"][1].update(symbol="ku_left"))
        self.assert_preflight_error([*self.entry_args, *self.left_args, *self.right_args], "conflicting C symbol")

    def test_receipt_hash_and_normalized_path_are_verified_before_compiling(self):
        src = Path(self.right_args[2])
        src.write_bytes(src.read_bytes() + b"\n")
        self.assert_preflight_error([*self.entry_args, *self.left_args, *self.right_args], "receipt C SHA256")
        self.change_plan(self.left_args, lambda p: p["export"].update(source_path="tools/../左.ku"))
        self.assert_preflight_error([*self.entry_args, *self.left_args, *self.right_args], "normalized project-relative")

    def test_aliases_are_stable_with_reordered_units(self):
        all_units = [self.entry_args, self.left_args, self.right_args]
        lines = []
        with patch.object(link_c.subprocess, "run") as cc:
            for units in (all_units, list(reversed(all_units))):
                with contextlib.redirect_stdout(io.StringIO()) as output:
                    self.assertEqual(link_c.main([*(part for unit in units for part in unit), "--check-only"]), 0)
                lines.append(output.getvalue())
            cc.assert_not_called()
        self.assertEqual(lines[0], lines[1])
        names = json.loads(lines[0])["app.ku#58"]
        self.assertNotEqual(names["ku_left"], names["ku_right"])
        self.assertEqual(names["ku_left"], link_c.canonical_symbol((self.left_path, 17)))

    def compiler(self):
        command = os.environ.get("CC", "cc")
        if not shutil.which(command):
            self.skipTest("no C compiler")
        return command

    def test_independent_units_can_share_local_c_names_and_use_different_carrier_aliases(self):
        cc = self.compiler()
        units = [*self.right_args, *self.entry_args, *self.left_args]
        sources = [self.caller, *(Path(args[2]) for args in (self.entry_args, self.left_args, self.right_args))]
        original = [path.read_bytes() for path in sources]
        with patch.object(link_c.subprocess, "run", wraps=subprocess.run) as calls:
            self.assertEqual(link_c.main([*units, *self.build_args, "--cc", cc]), 0)
        self.assertEqual(calls.call_count, 4)
        self.assertEqual([path.read_bytes() for path in sources], original)
        self.assertFalse(list(self.root.glob(".ku-bootstrap-link-*")))
        subprocess.run([self.output], check=True, timeout=10)

    def test_receipt_is_checked_against_the_c_declaration_at_compile_time(self):
        cc = self.compiler()
        self.output.write_bytes(b"preexisting")
        # The digest and semantic graph both validate, but this translation
        # unit's real return ABI disagrees with its explicit plan.
        wrong = self.provider_code.replace("int64_t ku_read", "int32_t ku_read").replace("FACTOR", "+")
        args = self.unit("bad-c", plan(self.left), wrong, receipt=True)
        with contextlib.redirect_stderr(io.StringIO()):
            self.assertEqual(link_c.main([*self.entry_args, *args, *self.right_args, *self.build_args, "--cc", cc]), 1)
        self.assertEqual(self.output.read_bytes(), b"preexisting")
        self.assertFalse(list(self.root.glob(".ku-bootstrap-link-*")))


@unittest.skipUnless(os.environ.get("KU_LINK_CHAIN_UNITS") and os.environ.get("KU_LINK_CHAIN_REFERENCE"),
                     "requires emitted parser-chain C and its original Ku reference")
class EmittedChainTests(unittest.TestCase):
    def test_original_parser_chain_links_and_runs_from_plans(self):
        units = Path(os.environ["KU_LINK_CHAIN_UNITS"])
        reference = Path(os.environ["KU_LINK_CHAIN_REFERENCE"])
        atom = json.loads((units / "ReadPrimaryAtom.plan.json").read_text(encoding="utf-8"))["export"]
        sig = atom["signature"]
        caller = ATOM_CALLER.replace("__TOKEN__", sig["parameters"][0]["c_type"]).replace(
            "__RESULT__", sig["result"]["c_type"]).replace("__ATOM__", atom["symbol"])
        expected = json.loads(reference.read_text(encoding="utf-8"))["generated_units"]["plain"]["stdout"]
        inputs = []
        for name in ("ReadPrimaryAtom", "RejectAt", "DiagnosticCanonical", "MakeDiagnostic",
                     "SpanFromToken", "SpanCanonical", "EscapeCanonical"):
            inputs.extend(["--unit", str(units / f"{name}.plan.json"), str(units / f"{name}.c.txt")])
        with tempfile.TemporaryDirectory(prefix="ku-link-emitted-chain-") as scratch:
            directory = Path(scratch)
            driver = directory / "caller.c"
            driver.write_text(caller, encoding="ascii")
            app = directory / ("atom.exe" if os.name == "nt" else "atom")
            flags = []
            if os.environ.get("KU_LINK_CHAIN_SANITIZE") == "1":
                flags = ["--cflag=-fsanitize=address,undefined", "--cflag=-fno-sanitize-recover=all",
                         "--ldflag=-fsanitize=address,undefined"]
            with patch.object(link_c.subprocess, "run", wraps=subprocess.run) as commands:
                self.assertEqual(link_c.main([*inputs, "--entry", atom["source_path"], str(atom["node"]),
                                              "--caller", str(driver), "-o", str(app), *flags]), 0)
            self.assertEqual(commands.call_count, 8)
            result = subprocess.run([app], stdout=subprocess.PIPE, stderr=subprocess.PIPE, check=True, timeout=20)
            self.assertEqual(result.stdout.decode("utf-8").replace("\r\n", "\n"), expected)
            self.assertFalse(result.stderr)
            self.assertFalse(list(directory.glob(".ku-bootstrap-link-*")))


if __name__ == "__main__":
    unittest.main()
