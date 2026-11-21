#!/usr/bin/env python3
"""Link bootstrap C units by their declared Ku identities."""

from __future__ import annotations

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
from dataclasses import dataclass


PLAN = "ku-bootstrap-link-plan-v1"
RECEIPT = "ku-bootstrap-link-receipt-v1"
IDENTIFIER = re.compile(r"[A-Za-z_][A-Za-z_0-9]*\Z")
VIEW_TYPE = re.compile(r"const [A-Za-z_][A-Za-z_0-9]* ?\*\Z")
SHA256 = re.compile(r"[a-f0-9]{64}\Z")
BOOTSTRAP_STRING_TYPES = {"KuProjectionString", "KuBootstrapString", "KuScalarString"}
CALLBACK_TYPES = {
    "IntPairResultCallback": ("int_pair_result_callback", "KuBootstrapIntPairCallback_"),
    "IntArrayResultCallback": ("int_array_result_callback", "KuBootstrapIntArrayCallback_"),
    "IntResultAppendCallback": ("int_result_append_callback", "KuBootstrapIntAppendCallback_"),
    "IntResultArenaCallback": ("int_result_arena_callback", "KuBootstrapIntArenaCallback_"),
    "IntRecordReaderCallback": ("int_record_reader_callback", "KuBootstrapIntReaderCallback_"),
    "NullableRecordViewReader": ("nullable_record_view_reader", "KuBootstrapNullableRecordViewReader_"),
}
CALLBACK_IDENTITIES = {identity for identity, _ in CALLBACK_TYPES.values()}
CALLBACK_PREFIXES = {identity: prefix for identity, prefix in CALLBACK_TYPES.values()}
NOMINAL_CARRIER = re.compile(r"(?:KuBootstrapResult_)?(?:KuArray_)?(KuStruct_m[1-9][0-9]*_([1-9][0-9]*))\Z")
UNIT_ENUM_CARRIER = re.compile(r"KuEnum_m[1-9][0-9]*_([1-9][0-9]*)\Z")
CHECKED_INT_CALLBACK_PARAMETERS = {
    "IntResultAppendCallback": ("KuProjectionString", "KuProjectionString", "int64_t", 0, "KuArrayInt", 1),
    "IntResultArenaCallback": (0, 1, "bool"),
}
CHECKED_INT_CALLBACK_ABI = {CALLBACK_TYPES[kind][0]: parameters
                           for kind, parameters in CHECKED_INT_CALLBACK_PARAMETERS.items()}


class LinkError(ValueError):
    pass


def record(value, fields, where):
    if not isinstance(value, dict) or set(value) != set(fields.split()):
        raise LinkError(f"{where} requires exactly these fields: {fields}")
    return value


def text(value, where):
    if not isinstance(value, str) or not value:
        raise LinkError(f"{where} requires a nonempty string")
    return value


def module_path(value, where):
    value = text(value, where)
    if (not value.endswith(".ku") or len(value.encode("utf-8")) > 512
            or any(c in value for c in "\\:\n\r\t\0")
            or any(part in {"", ".", ".."} for part in value.split("/"))):
        raise LinkError(f"{where} requires a normalized project-relative .ku path")
    return value


def node_id(value, where):
    if type(value) is not int or value <= 0:
        raise LinkError(f"{where} requires a positive declaration node")
    return value


def array(value, where):
    if not isinstance(value, list):
        raise LinkError(f"{where} requires an array")
    return value


def no_duplicate_fields(pairs):
    result = {}
    for key, value in pairs:
        if key in result:
            raise LinkError(f"duplicate JSON field {key!r}")
        result[key] = value
    return result


@dataclass(frozen=True)
class TypeRef:
    identity: tuple
    passing: str
    c_type: str
    callback_nominals: tuple[str, ...] = ()

    def read(value, where, checked_result=False):
        fields = "kind source_name source_path node c_type passing"
        candidate_kind = value.get("kind") if isinstance(value, dict) else None
        if isinstance(candidate_kind, str) and candidate_kind in CHECKED_INT_CALLBACK_PARAMETERS:
            fields += " nominals"
        ref = record(value, fields, where)
        kind_name = text(ref["kind"], f"{where}.kind")
        name = text(ref["source_name"], f"{where}.source_name")
        passing = ref["passing"]
        c_type = text(ref["c_type"], f"{where}.c_type")
        if passing == "value":
            valid = IDENTIFIER.fullmatch(c_type)
        elif passing == "view":
            valid = VIEW_TYPE.fullmatch(c_type)
        else:
            raise LinkError(f"{where}.passing must be value or view")
        if not valid:
            raise LinkError(f"{where}.c_type is not a supported {passing} C carrier")
        if kind_name == "Builtin":
            if ref["source_path"] is not None or type(ref["node"]) is not int or ref["node"] != 0:
                raise LinkError(f"{where} builtin must have a null path and node 0")
            if name == "bool" and checked_result and c_type != "KuBootstrapResult_bool":
                raise LinkError(f"{where} checked bool requires the KuBootstrapResult_bool carrier")
            if name == "null":
                carrier = "KuBootstrapResultNull" if checked_result else "uint8_t"
                if passing != "value" or c_type != carrier:
                    raise LinkError(f"{where} null requires the by-value {carrier} carrier")
            identity = ("builtin", name)
        elif kind_name == "NullableInt":
            if (name != "int" or ref["source_path"] is not None or type(ref["node"]) is not int or ref["node"] != 0
                    or passing != "value" or checked_result or c_type != "KuBootstrapOptionalInt"):
                raise LinkError(f"{where} requires the plain by-value nullable int carrier")
            identity = ("nullable_int",)
        elif kind_name == "BuiltinArray":
            view_carrier = {"str": "KuGuardStringArray", "int": "KuArrayInt"}.get(name)
            view = (passing == "view" and view_carrier is not None and
                    c_type[len("const "):-1].rstrip() == view_carrier)
            string_array = "KuBootstrapResultStringArray" if checked_result else "KuGuardStringArray"
            owned = name == "str" and passing == "value" and c_type == string_array
            int_array = "KuBootstrapResult_KuArrayInt" if checked_result else "KuArrayInt"
            owned_int = name == "int" and passing == "value" and c_type == int_array
            if (ref["source_path"] is not None or type(ref["node"]) is not int or ref["node"] != 0
                    or not (view or owned or owned_int)):
                raise LinkError(f"{where} builtin array requires the checked View [str] carrier, "
                                "the checked owned [str]! result carrier, or a matching View, owned, "
                                "or checked owned [int] carrier")
            identity = ("builtin_array", name)
        elif kind_name in ("Struct", "StructArray"):
            identity = ("struct" if kind_name == "Struct" else "struct_array",
                        module_path(ref["source_path"], f"{where}.source_path"),
                        node_id(ref["node"], f"{where}.node"))
        elif kind_name == "UnitEnum":
            path = module_path(ref["source_path"], f"{where}.source_path")
            node = node_id(ref["node"], f"{where}.node")
            match = UNIT_ENUM_CARRIER.fullmatch(c_type)
            if passing != "value" or checked_result or not match or int(match[1]) != node:
                raise LinkError(f"{where} requires the plain by-value unit enum carrier for {label((path, node))}")
            identity = ("unit_enum", path, node)
        elif kind_name in CALLBACK_TYPES:
            path = module_path(ref["source_path"], f"{where}.source_path")
            node = node_id(ref["node"], f"{where}.node")
            kind, prefix = CALLBACK_TYPES[kind_name]
            if kind_name in CHECKED_INT_CALLBACK_PARAMETERS:
                if (passing != "value" or checked_result or
                        not re.fullmatch(rf"{prefix}m[1-9][0-9]*_{node}", c_type)):
                    raise LinkError(f"{where} requires the typed carrier for {kind_name}")
                nominals = array(ref["nominals"], f"{where}.nominals")
                if len(nominals) != 2:
                    raise LinkError(f"{where} requires two ordered nominal callback arguments")
                keys = []
                carriers = []
                for index, value in enumerate(nominals):
                    field = f"{where}.nominals[{index}]"
                    nominal = record(value, "source_path node c_type", field)
                    nominal_path = module_path(nominal["source_path"], f"{field}.source_path")
                    nominal_node = node_id(nominal["node"], f"{field}.node")
                    nominal_c = text(nominal["c_type"], f"{field}.c_type")
                    if not re.fullmatch(rf"KuStruct_m[1-9][0-9]*_{nominal_node}", nominal_c):
                        raise LinkError(f"{field} requires its nominal C carrier")
                    keys.append((nominal_path, nominal_node))
                    carriers.append(nominal_c)
                return TypeRef((kind, *keys), passing, c_type, tuple(carriers))
            if (passing != "value" or checked_result or
                    not re.fullmatch(rf"{prefix}KuStruct_m[1-9][0-9]*_{node}", c_type)):
                raise LinkError(f"{where} requires the typed carrier for {kind_name}")
            identity = (kind, path, node)
        else:
            raise LinkError(f"{where}.kind is not a supported link-plan type")
        return TypeRef(identity, passing, c_type)

    def semantic(self):
        carrier = None
        if self.identity[0] == "builtin":
            carrier = self.c_type
            if self.passing == "view":
                carrier = carrier[len("const "):-1].rstrip()
            # The scalar and projected emitters spell the same string
            # carrier differently. Other builtin ABI spellings must agree.
            if self.identity[1] == "str" and carrier in BOOTSTRAP_STRING_TYPES:
                carrier = "bootstrap string"
            if self.identity[1] == "bool" and carrier == "_Bool":
                carrier = "bool"
        return self.identity, self.passing, carrier


@dataclass(frozen=True)
class Signature:
    parameters: tuple[TypeRef, ...]
    result: TypeRef
    returns_result: bool

    def read(value, where):
        sig = record(value, "parameters result returns_result", where)
        params = array(sig["parameters"], f"{where}.parameters")
        if type(sig["returns_result"]) is not bool:
            raise LinkError(f"{where} requires a by-value result and boolean returns_result")
        result = TypeRef.read(sig["result"], f"{where}.result", checked_result=sig["returns_result"])
        if result.identity[0] in CALLBACK_IDENTITIES and result.identity[0] != "nullable_record_view_reader":
            raise LinkError(f"{where} callback carrier cannot be a returned value")
        if result.passing != "value":
            raise LinkError(f"{where} requires a by-value result and boolean returns_result")
        return Signature(tuple(TypeRef.read(item, f"{where}.parameters[{i}]")
                               for i, item in enumerate(params)), result, sig["returns_result"])

    def semantic(self):
        return (tuple(ref.semantic() for ref in self.parameters),
                self.result.semantic(), self.returns_result)

    def c_parameters(self):
        return ", ".join(ref.c_type for ref in self.parameters) or "void"


@dataclass(frozen=True)
class Declaration:
    key: tuple[str, int]
    symbol: str
    signature: Signature
    source_name: str | None = None

    def read(value, where):
        decl = record(value, "source_path node source_name symbol signature", where)
        key = (module_path(decl["source_path"], f"{where}.source_path"),
               node_id(decl["node"], f"{where}.node"))
        name = text(decl["source_name"], f"{where}.source_name")
        symbol = text(decl["symbol"], f"{where}.symbol")
        if not IDENTIFIER.fullmatch(symbol):
            raise LinkError(f"{where}.symbol requires a C identifier")
        return Declaration(key, symbol, Signature.read(decl["signature"], f"{where}.signature"), name)


@dataclass(frozen=True)
class Unit:
    plan: Path
    source: Path
    code: bytes
    export: Declaration
    dependencies: tuple[Declaration, ...]

    def read(plan, source):
        try:
            value = json.loads(plan.read_text(encoding="utf-8"), object_pairs_hook=no_duplicate_fields)
            mode = value.get("schema") if isinstance(value, dict) else None
            if mode not in (PLAN, RECEIPT):
                raise LinkError(f"expected {PLAN} or {RECEIPT}")
            fields = "schema mode export dependencies"
            if mode == RECEIPT:
                fields += " c_sha256"
            record(value, fields, "link plan")
            text(value["mode"], "mode")
            export = Declaration.read(value["export"], "export")
            deps = array(value["dependencies"], "dependencies")
            dependencies = tuple(Declaration.read(dep, f"dependencies[{i}]") for i, dep in enumerate(deps))
            code = source.read_bytes()
            if mode == RECEIPT:
                digest = text(value["c_sha256"], "c_sha256")
                if not SHA256.fullmatch(digest) or hashlib.sha256(code).hexdigest() != digest:
                    raise LinkError(f"receipt C SHA256 does not match {source}")
            return Unit(plan, source, code, export, dependencies)
        except (OSError, UnicodeError, ValueError) as error:
            raise LinkError(f"{plan}: {error}") from error


def fs_read_provider():
    tools = Path(__file__).resolve().parent
    driver = tools / "compiler_driver"
    backend = tools.parents[1] / "src" / "backend"
    source = (driver / "fs_read.c").resolve()
    headers = (driver / "fs_read.h", backend / "c_fs_common.c",
               backend / "c_fs_current_directory.c", backend / "c_fs_read.c")
    # This declaration must match FsReadLinkPlan and the checked fs_read.h ABI.
    signature = Signature((TypeRef(("builtin", "str"), "view", "const KuString*"),),
                          TypeRef(("builtin", "str"), "value", "KuResult_str"), True)
    export = Declaration(("std/fs.ku", 1), "KuBootstrapFsRead", signature)
    unit = Unit(source, source, source.read_bytes(), export, ())
    return unit, {path.resolve(): path.read_bytes() for path in headers}


def string_slice_provider():
    tools = Path(__file__).resolve().parent
    source = tools / "compiler_driver" / "string_slice.c"
    implementation = tools.parents[1] / "src" / "backend" / "c_string_slice.c"
    index = TypeRef(("builtin", "int"), "value", "int64_t")
    signature = Signature((TypeRef(("builtin", "str"), "view", "const KuProjectionString*"), index, index),
                          TypeRef(("builtin", "str"), "value", "KuBootstrapResult_KuProjectionString"), True)
    export = Declaration(("std/string.ku", 2), "KuBootstrapStringSlice", signature)
    return Unit(source, source, source.read_bytes(), export, ()), {implementation: implementation.read_bytes()}


def string_trim_provider():
    tools = Path(__file__).resolve().parent
    driver = tools / "compiler_driver"
    backend = tools.parents[1] / "src" / "backend"
    source = driver / "string_trim.c"
    headers = (driver / "string_trim_whitespace.h",
               backend / "c_string_unicode_base.c", backend / "c_string_unicode_trim.c")
    input_type = TypeRef(("builtin", "str"), "view", "const KuProjectionString*")
    result_type = TypeRef(("builtin", "str"), "value", "KuProjectionString")
    export = Declaration(("std/string.ku", 5), "KuBootstrapStringTrim", Signature((input_type,), result_type, False))
    return Unit(source, source, source.read_bytes(), export, ()), {path: path.read_bytes() for path in headers}


def bytes_provider(method, number, symbol, parameters, result):
    tools = Path(__file__).resolve().parent
    driver = tools / "compiler_driver"
    backend = tools.parents[1] / "src" / "backend"
    source = driver / f"bytes_{method}.c"
    headers = (driver / "bytes_abi.h", backend / "c_bytes_storage.c", backend / "c_bytes_index.c")
    export = Declaration(("std/bytes.ku", number), symbol, Signature(parameters, result, True))
    return Unit(source, source, source.read_bytes(), export, ()), {path: path.read_bytes() for path in headers}


def bytes_from_str_provider():
    source = TypeRef(("builtin", "str"), "view", "const KuProjectionString*")
    result = TypeRef(("builtin", "bytes"), "value", "KuBootstrapResult_KuProjectionString")
    return bytes_provider("from_str", 3, "KuBootstrapBytesFromString", (source,), result)


def bytes_get_provider():
    source = TypeRef(("builtin", "bytes"), "view", "const KuProjectionString*")
    index = TypeRef(("builtin", "int"), "value", "int64_t")
    result = TypeRef(("builtin", "int"), "value", "KuBootstrapResult_int64_t")
    return bytes_provider("get", 4, "KuBootstrapBytesGet", (source, index), result)


def label(key):
    return f"{key[0]}#{key[1]}"


def canonical_symbol(key):
    digest = hashlib.sha256(f"{key[0]}\0{key[1]}".encode("utf-8")).hexdigest()
    return f"ku_bootstrap_link_{digest}"


def checked_aliases(units):
    providers = {}
    given_sources = set()
    canonical = {}
    for unit in units:
        key = unit.export.key
        if key in providers:
            raise LinkError(f"duplicate provider for {label(key)}")
        if unit.source in given_sources:
            raise LinkError(f"C unit supplied more than once: {unit.source}")
        given_sources.add(unit.source)
        providers[key] = unit.export
        symbol = canonical_symbol(key)
        if symbol in canonical:
            raise LinkError(f"canonical symbol collision for {label(key)}")
        canonical[symbol] = key
    result = {}
    for unit in units:
        aliases = {}
        for request in (unit.export, *unit.dependencies):
            key = request.key
            if key not in providers:
                raise LinkError(f"{unit.plan}: missing provider for {label(key)}")
            actual = providers[key].signature.semantic()
            expected = request.signature.semantic()
            if actual != expected:
                raise LinkError(f"{unit.plan}: signature mismatch for {label(key)}: "
                                f"expected {expected!r}, provider declares {actual!r}")
            symbol = canonical_symbol(key)
            local = request.symbol
            if (local in aliases and aliases[local] != symbol) or (local in canonical and local != symbol):
                raise LinkError(f"{unit.plan}: conflicting C symbol {local}")
            aliases[local] = symbol
        result[unit.export.key] = aliases
    return result


def carrier_aliases(unit):
    """Give the same Ku nominal type the same C identity in each linked unit."""
    aliases = {}
    used = set(re.findall(rb"[A-Za-z_][A-Za-z_0-9]*", unit.code))

    def bind(local, stable):
        if local.encode("ascii") not in used:
            return
        if local in aliases and aliases[local] != stable:
            raise LinkError(f"{unit.plan}: conflicting C carrier {local}")
        aliases[local] = stable

    def nominal(local, path, node):
        canonical = "KuStruct_" + canonical_symbol((path, node))
        for prefix in ("", "KuArray_", "KuBootstrapResult_", "KuBootstrapResult_KuArray_"):
            bind(prefix + local, prefix + canonical)

    for declaration in (unit.export, *unit.dependencies):
        for ref in (*declaration.signature.parameters, declaration.signature.result):
            kind, *identity = ref.identity
            if kind in ("struct", "struct_array"):
                carrier = ref.c_type if ref.passing == "value" else ref.c_type[6:-1].rstrip()
                match = NOMINAL_CARRIER.fullmatch(carrier)
                if match:
                    path, node = identity
                    if int(match[2]) != node:
                        raise LinkError(f"{unit.plan}: C carrier for {label((path, node))} has the wrong declaration node")
                    nominal(match[1], path, node)
            elif kind == "unit_enum":
                path, node = identity
                bind(ref.c_type, "KuEnum_" + canonical_symbol((path, node)))
            elif kind in CHECKED_INT_CALLBACK_ABI:
                for local, (path, node) in zip(ref.callback_nominals, identity):
                    nominal(local, path, node)
            elif kind in CALLBACK_PREFIXES:
                path, node = identity
                prefix = CALLBACK_PREFIXES[kind]
                nominal(ref.c_type.removeprefix(prefix), path, node)
    return aliases


def check_carrier_layout(carrier, expected, fields, diagnostic="link-plan C callback has a different carrier"):
    members = " ".join(f"{c_type} {name};" for name, c_type in fields)
    checks = [f"sizeof({carrier}) == sizeof({expected})", f"_Alignof({carrier}) == _Alignof({expected})"]
    for name, c_type in fields:
        checks.append(f"_Generic((({carrier}){{0}}).{name}, {c_type}: 1, default: 0)")
        checks.append(f"offsetof({carrier}, {name}) == offsetof({expected}, {name})")
    return [f"typedef struct {{ {members} }} {expected};",
            f'_Static_assert({" && ".join(checks)}, "{diagnostic}");']


def wrapper(unit, include_caller):
    lines = ['#include "emitted.c"']
    declarations = (unit.export, *unit.dependencies)
    nullable_int = any(ref.identity[0] == "nullable_int" for decl in declarations
                       for ref in (*decl.signature.parameters, decl.signature.result))
    callbacks = sorted({(ref.identity[0], ref.c_type, ref.callback_nominals) for decl in declarations
                        for ref in (*decl.signature.parameters, decl.signature.result)
                        if ref.identity[0] in CALLBACK_IDENTITIES})
    unit_enums = sorted({ref.c_type for decl in declarations
                         for ref in (*decl.signature.parameters, decl.signature.result)
                         if ref.identity[0] == "unit_enum"})
    nullable_int = nullable_int or any(kind == "nullable_record_view_reader" for kind, _, _ in callbacks)
    if callbacks or nullable_int or unit_enums:
        lines.append('#include <stddef.h>')
    if unit_enums:
        lines.append("typedef struct { int32_t tag; union { unsigned char empty; } payload; } ku_bootstrap_enum_layout;")
    for carrier in unit_enums:
        lines.append(f'_Static_assert(sizeof({carrier}) == sizeof(ku_bootstrap_enum_layout) && '
                     f'_Alignof({carrier}) == _Alignof(ku_bootstrap_enum_layout) && '
                     f'offsetof({carrier}, tag) == offsetof(ku_bootstrap_enum_layout, tag) && '
                     f'offsetof({carrier}, payload) == offsetof(ku_bootstrap_enum_layout, payload) && '
                     f'_Generic((({carrier}){{0}}).tag, int32_t: 1, default: 0) && '
                     f'_Generic((({carrier}){{0}}).payload.empty, unsigned char: 1, default: 0) && '
                     f'sizeof((({carrier}){{0}}).payload) == sizeof(unsigned char), '
                     f'"link-plan C unit enum has a different carrier");')
    if nullable_int:
        lines.extend(check_carrier_layout("KuBootstrapOptionalInt", "ku_bootstrap_optional_int_layout",
                                         (("is_null", "bool"), ("value", "int64_t")),
                                         "link-plan C nullable int has a different carrier"))
    if any(kind not in {"int_pair_result_callback", "nullable_record_view_reader"} for kind, _, _ in callbacks):
        lines.extend(check_carrier_layout("KuLocalAppendFailure", "ku_bootstrap_callback_failure_layout", (
            ("append_status", "int"), ("out_of_memory", "bool"), ("overflow", "bool"), ("index_failed", "bool"),
            ("array_error", "const char *"), ("index", "int64_t"), ("length", "size_t"),
        )))
    for i, (kind, carrier, nominals) in enumerate(callbacks):
        entry = f"ku_bootstrap_callback_entry_{i}"
        expected = f"ku_bootstrap_callback_layout_{i}"
        fields = [("entry", entry), ("environment", "void *")]
        if kind == "nullable_record_view_reader":
            nominal = carrier.removeprefix(CALLBACK_TYPES["NullableRecordViewReader"][1])
            lines.append(f"typedef KuBootstrapOptionalInt (*{entry})(const KuProjectionString*, const {nominal}*);")
            fields = [("entry", entry), ("capture", "KuProjectionString")]
        elif kind == "int_array_result_callback":
            nominal = carrier.removeprefix(CALLBACK_TYPES["IntArrayResultCallback"][1])
            lines.append(f"typedef KuBootstrapResult_KuArrayInt (*{entry})(void*, KuLocalAppendFailure*, "
                         f"KuArrayInt, int64_t, int64_t, {nominal});")
            fields.append(("failure", "KuLocalAppendFailure *"))
        elif kind in CHECKED_INT_CALLBACK_ABI:
            params = [nominals[field] if isinstance(field, int) else field for field in CHECKED_INT_CALLBACK_ABI[kind]]
            lines.append(f"typedef KuBootstrapResult_int64_t (*{entry})(void*, KuLocalAppendFailure*, "
                         f"{', '.join(params)});")
            fields.append(("failure", "KuLocalAppendFailure *"))
        elif kind == "int_record_reader_callback":
            result = carrier.removeprefix(CALLBACK_TYPES["IntRecordReaderCallback"][1])
            lines.append(f"typedef {result} (*{entry})(void*, KuLocalAppendFailure*, int64_t);")
            fields.append(("failure", "KuLocalAppendFailure *"))
        else:
            result = carrier.removeprefix(CALLBACK_TYPES["IntPairResultCallback"][1])
            lines.append(f"typedef KuBootstrapResult_{result} (*{entry})(void*, int64_t, int64_t);")
        lines.extend(check_carrier_layout(carrier, expected, fields))
    for i, decl in enumerate(declarations):
        sig = decl.signature
        expected = f"ku_bootstrap_signature_{i}"
        lines.append(f"typedef {sig.result.c_type} (*{expected})({sig.c_parameters()});")
        lines.append(f'_Static_assert(_Generic(&{decl.symbol}, {expected}: 1, default: 0), '
                     f'"link-plan C declaration {i} has a different signature");')
    if include_caller:
        lines.append('#include "caller.c"')
    return "\n".join(lines) + "\n"


def program_caller(entry):
    """Provide a C process entry for a plain or checked Ku application."""
    plain = TypeRef(("builtin", "null"), "value", "uint8_t")
    if entry.signature == Signature((), plain, False):
        return f'''int main(void) {{
    (void){entry.symbol}();
    return 0;
}}
'''.encode("ascii")
    checked = TypeRef(("builtin", "null"), "value", "KuBootstrapResultNull")
    if entry.signature != Signature((), checked, True):
        raise LinkError("--program requires a zero-argument linked null or null! entry")
    return f'''#include <stdio.h>
#include <stdlib.h>

/* The entry unit may use only Copy values and omit its string helpers. */
static void ku_bootstrap_program_drop_error_part(KuProjectionString part) {{
    if (part.storage == KU_PROJECTION_STRING_OWNED && part.ptr) free(part.ptr);
}}

int main(void) {{
    KuBootstrapResultNull result = {entry.symbol}();
    if (result.ok) return 0;
    if (result.error.message.len)
        fwrite(result.error.message.ptr, 1, result.error.message.len, stderr);
    fputc('\\n', stderr);
    ku_bootstrap_program_drop_error_part(result.error.domain);
    ku_bootstrap_program_drop_error_part(result.error.code);
    ku_bootstrap_program_drop_error_part(result.error.message);
    return 1;
}}
'''.encode("ascii")


def run_cc(args, timeout):
    try:
        subprocess.run([str(arg) for arg in args], check=True, timeout=timeout)
    except subprocess.CalledProcessError as error:
        raise LinkError(f"C compiler exited with status {error.returncode}") from error
    except subprocess.TimeoutExpired as error:
        raise LinkError(f"C compiler timed out after {timeout:g}s") from error


def windows_command_fits(args):
    # CreateProcessW counts UTF-16 code units, including the trailing NUL.
    command = subprocess.list2cmdline([str(arg) for arg in args])
    return len(command.encode("utf-16-le", errors="surrogatepass")) // 2 + 1 <= 32767


def link_command(cc, objects, binary, ldflags, *, msvc):
    prefix = [cc, "/nologo"] if msvc else [cc]
    target = [f"/Fe{binary}"] if msvc else ["-o", binary]
    command = [*prefix, *objects, *target, *ldflags]
    if sys.platform != "win32" or windows_command_fits(command):
        return command

    # Leave flags on the command line so their order and relative paths
    # keep their original meaning. The response file only holds objects.
    response = binary.with_name("objects.rsp")
    command = [*prefix, f"@{response}", *target, *ldflags]
    if not windows_command_fits(command):
        raise LinkError("C linker arguments exceed the Windows command-line limit even with an object response file")
    if msvc:
        lines = [subprocess.list2cmdline([str(obj)]) for obj in objects]
        data = b"\xff\xfe" + ("\n".join(lines) + "\n").encode("utf-16-le")
    else:
        lines = ['"' + str(obj).replace('\\', '\\\\').replace('"', '\\"') + '"' for obj in objects]
        # MinGW GCC reads its system ANSI code page. Clang reads UTF-8.
        encoding = "mbcs" if os.name == "nt" and "clang" not in Path(cc).stem.lower() else "utf-8"
        try:
            data = ("\n".join(lines) + "\n").encode(encoding)
        except UnicodeEncodeError as error:
            raise LinkError("GNU C linker response file cannot encode a temporary object path") from error
    response.write_bytes(data)
    return command


def link(units, aliases, output, entry, caller, cc, cflags, ldflags, timeout, builtin_headers):
    msvc = Path(cc).stem.lower() in {"cl", "clang-cl"}
    windows_gnu = sys.platform == "win32" and not msvc
    flags = (["/nologo", "/std:c11", "/O2", "/WX"] if msvc else
             ["-std=c11", "-O2", "-pedantic-errors", "-Wall", "-Wextra", "-Werror"])
    # MinGW's assembler can lose Unicode in absolute object paths. Use
    # compiler-owned scratch independent of the project's output directory.
    scratch_parent = None if windows_gnu else output.parent
    with tempfile.TemporaryDirectory(prefix=".ku-bootstrap-link-", dir=scratch_parent) as tmp:
        root = Path(tmp)
        if builtin_headers:
            include = root / "builtin"
            include.mkdir()
            for path, code in builtin_headers.items():
                (include / path.name).write_bytes(code)
            cflags = [f"{'/I' if msvc else '-I'}{include}", *cflags]
        objects = []
        for i, unit in enumerate(sorted(units, key=lambda item: item.export.key)):
            work = root / str(i)
            work.mkdir()
            (work / "emitted.c").write_bytes(unit.code)
            include_caller = unit.export.key == entry
            if include_caller:
                (work / "caller.c").write_bytes(caller)
            source = work / "unit.c"
            source.write_text(wrapper(unit, include_caller), encoding="ascii")
            obj = work / ("unit.obj" if msvc else "unit.o")
            define = "/D" if msvc else "-D"
            renames = dict(aliases[unit.export.key])
            for local, canonical in carrier_aliases(unit).items():
                if local in renames and renames[local] != canonical:
                    raise LinkError(f"{unit.plan}: conflicting C identifier {local}")
                renames[local] = canonical
            macros = [f"{define}{local}={canonical}" for local, canonical in sorted(renames.items())
                      if local != canonical]
            compile_args = ["/c", source, f"/Fo{obj}"] if msvc else ["-c", source, "-o", obj]
            run_cc([cc, *flags, *cflags, *macros, *compile_args], timeout)
            objects.append(obj)
        binary = root / ("program.exe" if os.name == "nt" or msvc else "program")
        run_cc(link_command(cc, objects, binary, ldflags, msvc=msvc), timeout)
        if windows_gnu:
            # Scratch can be on another volume. Stage the finished binary
            # beside the output so replacement keeps its atomic semantics.
            with tempfile.TemporaryDirectory(prefix=".ku-bootstrap-publish-", dir=output.parent) as publish:
                staged = Path(publish) / "program.exe"
                shutil.copy2(binary, staged)
                os.replace(staged, output)
        else:
            os.replace(binary, output)


BUILTIN_PROVIDERS = {
    "fs.read": fs_read_provider, "string.slice": string_slice_provider, "string.trim": string_trim_provider,
    "bytes.from_str": bytes_from_str_provider, "bytes.get": bytes_get_provider,
}


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--unit", nargs=2, required=True, action="append", metavar=("PLAN", "C"))
    parser.add_argument("--builtin", action="append", choices=BUILTIN_PROVIDERS, default=[])
    parser.add_argument("--entry", nargs=2, metavar=("MODULE", "NODE"))
    host = parser.add_mutually_exclusive_group()
    host.add_argument("--caller", type=Path)
    host.add_argument("--program", action="store_true", help="add a process entry for a linked null or null! function")
    parser.add_argument("-o", "--output", type=Path)
    parser.add_argument("--check-only", action="store_true")
    parser.add_argument("--cc", default=os.environ.get("CC", "cc"))
    parser.add_argument("--cflag", action="append", default=[])
    parser.add_argument("--ldflag", action="append", default=[])
    parser.add_argument("--timeout", type=float, default=120)
    args = parser.parse_args(argv)
    try:
        if bool(args.entry) != bool(args.caller or args.program):
            raise LinkError("--entry requires either --caller or --program, and vice versa")
        if not args.check_only and not args.output:
            raise LinkError("--output is required when linking")
        if not 0 < args.timeout < float("inf"):
            raise LinkError("--timeout must be a finite positive number")
        units = [Unit.read(Path(plan).resolve(), Path(source).resolve()) for plan, source in args.unit]
        builtin_headers = {}
        for name in args.builtin:
            provider, headers = BUILTIN_PROVIDERS[name]()
            units.append(provider)
            builtin_headers.update(headers)
        aliases = checked_aliases(units)
        entry = None
        caller = None
        if args.entry:
            entry = (module_path(args.entry[0], "entry module"), node_id(int(args.entry[1]), "entry node"))
            if entry not in aliases:
                raise LinkError(f"missing entry unit for {label(entry)}")
            if args.program:
                selected = next(unit.export for unit in units if unit.export.key == entry)
                caller = program_caller(selected)
            else:
                caller = args.caller.read_bytes()
        if args.check_only:
            print(json.dumps({label(key): table for key, table in sorted(aliases.items())}, sort_keys=True))
            return 0
        output = args.output.resolve()
        inputs = {path for unit in units for path in (unit.plan, unit.source)}
        inputs.update(builtin_headers)
        if args.caller:
            inputs.add(args.caller.resolve())
        if output in inputs:
            raise LinkError("output refers to an input file")
        output.parent.mkdir(parents=True, exist_ok=True)
        link(units, aliases, output, entry, caller, args.cc, args.cflag, args.ldflag,
             args.timeout, builtin_headers)
    except (OSError, ValueError) as error:
        print(f"bootstrap link: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
