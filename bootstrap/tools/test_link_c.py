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
