#!/usr/bin/env python3
"""Link bootstrap C units by their declared Ku identities."""

from __future__ import annotations

import argparse
import hashlib
import json
import os
from pathlib import Path
import re
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

    def read(value, where, checked_result=False):
        ref = record(value, "kind source_name source_path node c_type passing", where)
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
        if ref["kind"] == "Builtin":
            if ref["source_path"] is not None or type(ref["node"]) is not int or ref["node"] != 0:
                raise LinkError(f"{where} builtin must have a null path and node 0")
            identity = ("builtin", name)
        elif ref["kind"] == "BuiltinArray":
            view_carrier = {"str": "KuGuardStringArray", "int": "KuArrayInt"}.get(name)
            view = (passing == "view" and view_carrier is not None and
                    c_type[len("const "):-1].rstrip() == view_carrier)
            owned = (name == "str" and checked_result and passing == "value" and
                     c_type == "KuBootstrapResultStringArray")
            if (ref["source_path"] is not None or type(ref["node"]) is not int or ref["node"] != 0
                    or not (view or owned)):
                raise LinkError(f"{where} builtin array requires the checked View [str] carrier, "
                                "checked View [int] carrier or checked owned [str]! result carrier")
            identity = ("builtin_array", name)
        elif ref["kind"] in ("Struct", "StructArray"):
            identity = ("struct" if ref["kind"] == "Struct" else "struct_array",
                        module_path(ref["source_path"], f"{where}.source_path"),
                        node_id(ref["node"], f"{where}.node"))
        else:
            raise LinkError(f"{where}.kind must be Builtin, BuiltinArray, Struct or StructArray")
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

    def read(value, where):
        decl = record(value, "source_path node source_name symbol signature", where)
        key = (module_path(decl["source_path"], f"{where}.source_path"),
               node_id(decl["node"], f"{where}.node"))
        text(decl["source_name"], f"{where}.source_name")
        symbol = text(decl["symbol"], f"{where}.symbol")
        if not IDENTIFIER.fullmatch(symbol):
            raise LinkError(f"{where}.symbol requires a C identifier")
        return Declaration(key, symbol, Signature.read(decl["signature"], f"{where}.signature"))


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


def wrapper(unit, include_caller):
    lines = ['#include "emitted.c"']
    for i, decl in enumerate((unit.export, *unit.dependencies)):
        sig = decl.signature
        expected = f"ku_bootstrap_signature_{i}"
        lines.append(f"typedef {sig.result.c_type} (*{expected})({sig.c_parameters()});")
        lines.append(f'_Static_assert(_Generic(&{decl.symbol}, {expected}: 1, default: 0), '
                     f'"link-plan C declaration {i} has a different signature");')
    if include_caller:
        lines.append('#include "caller.c"')
    return "\n".join(lines) + "\n"


def run_cc(args, timeout):
    try:
        subprocess.run([str(arg) for arg in args], check=True, timeout=timeout)
    except subprocess.CalledProcessError as error:
        raise LinkError(f"C compiler exited with status {error.returncode}") from error
    except subprocess.TimeoutExpired as error:
        raise LinkError(f"C compiler timed out after {timeout:g}s") from error


def link(units, aliases, output, entry, caller, cc, cflags, ldflags, timeout, builtin_headers):
    msvc = Path(cc).stem.lower() in {"cl", "clang-cl"}
    flags = (["/nologo", "/std:c11", "/O2", "/WX"] if msvc else
             ["-std=c11", "-O2", "-pedantic-errors", "-Wall", "-Wextra", "-Werror"])
    with tempfile.TemporaryDirectory(prefix=".ku-bootstrap-link-", dir=output.parent) as tmp:
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
            macros = [f"{define}{local}={canonical}" for local, canonical in sorted(aliases[unit.export.key].items())
                      if local != canonical]
            compile_args = ["/c", source, f"/Fo{obj}"] if msvc else ["-c", source, "-o", obj]
            run_cc([cc, *flags, *cflags, *macros, *compile_args], timeout)
            objects.append(obj)
        binary = root / ("program.exe" if os.name == "nt" or msvc else "program")
        link_args = ["/nologo", *objects, f"/Fe{binary}"] if msvc else [*objects, "-o", binary]
        run_cc([cc, *link_args, *ldflags], timeout)
        os.replace(binary, output)


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--unit", nargs=2, required=True, action="append", metavar=("PLAN", "C"))
    parser.add_argument("--builtin", action="append", choices=("fs.read",), default=[])
    parser.add_argument("--entry", nargs=2, metavar=("MODULE", "NODE"))
    parser.add_argument("--caller", type=Path)
    parser.add_argument("-o", "--output", type=Path)
    parser.add_argument("--check-only", action="store_true")
    parser.add_argument("--cc", default=os.environ.get("CC", "cc"))
    parser.add_argument("--cflag", action="append", default=[])
    parser.add_argument("--ldflag", action="append", default=[])
    parser.add_argument("--timeout", type=float, default=120)
    args = parser.parse_args(argv)
    try:
        if bool(args.entry) != bool(args.caller):
            raise LinkError("--entry and --caller must be supplied together")
        if not args.check_only and not args.output:
            raise LinkError("--output is required when linking")
        if not 0 < args.timeout < float("inf"):
            raise LinkError("--timeout must be a finite positive number")
        units = [Unit.read(Path(plan).resolve(), Path(source).resolve()) for plan, source in args.unit]
        builtin_headers = {}
        if args.builtin:
            if len(args.builtin) > 1:
                raise LinkError("duplicate provider for std/fs.ku#1")
            provider, builtin_headers = fs_read_provider()
            units.append(provider)
        aliases = checked_aliases(units)
        entry = None
        caller = None
        if args.entry:
            entry = (module_path(args.entry[0], "entry module"), node_id(int(args.entry[1]), "entry node"))
            if entry not in aliases:
                raise LinkError(f"missing entry unit for {label(entry)}")
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
