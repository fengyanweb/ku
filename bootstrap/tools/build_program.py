#!/usr/bin/env python3
"""Build a manifest-listed Ku application with the standalone bootstrap compiler."""

from __future__ import annotations

import argparse
from collections import deque
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile

import link_c


def bounded_file(path, limit):
    if not path.is_file():
        raise link_c.LinkError(f"expected a regular file: {path}")
    with path.open("rb") as source:
        data = source.read(limit + 1)
    if len(data) > limit:
        raise link_c.LinkError(f"{path} exceeds {limit} bytes")
    return data


def manifest_sources(manifest):
    paths = []
    for line in bounded_file(manifest, 32768).decode("utf-8").split("\n"):
        path = line.removesuffix("\r")
        if path:
            paths.append(link_c.module_path(path, "manifest entry"))
    if not 1 <= len(paths) <= 64 or len(set(paths)) != len(paths):
        raise link_c.LinkError("manifest must contain 1 to 64 distinct module paths")
    return paths


def compiler_output(compiler, operation, manifest, name, root):
    result = subprocess.run([compiler, operation, manifest, name], cwd=root,
                            capture_output=True, timeout=60)
    if result.returncode:
        diagnostic = result.stderr.decode("utf-8", errors="replace").rstrip()
        raise link_c.LinkError(diagnostic or f"{operation} failed with exit {result.returncode}")
    return result.stdout


def emit_unit(compiler, root, paths, module, name, ordinal):
    variant = root / "modules.txt"
    rotated = [module, *(path for path in paths if path != module)]
    variant.write_bytes(("\n".join(rotated) + "\n").encode("utf-8"))
    source = root / f"unit-{ordinal}.c"
    plan = source.with_suffix(".json")
    plan.write_bytes(compiler_output(compiler, "emit-unit-plan", variant.name, name, root))
    source.write_bytes(compiler_output(compiler, "emit-unit", variant.name, name, root))
    return link_c.Unit.read(plan, source)


def collect_units(compiler, root, paths):
    entry = emit_unit(compiler, root, paths, paths[0], "main", 0)
    if entry.export.key[0] != paths[0] or entry.export.source_name != "main":
        raise link_c.LinkError("expected main in the first manifest module")
    caller = link_c.program_caller(entry.export)
    known = {entry.export.key: (entry, "main", False)}
    pending = deque(entry.dependencies)
    builtins = {}
    for name, provider in link_c.BUILTIN_PROVIDERS.items():
        unit, headers = provider()
        method = name.rsplit(".", 1)[1]
        symbol = "ku_linked_dep_" + name.replace(".", "_")
        builtins[(unit.export.key, symbol)] = (unit, headers, method)
    headers = {}
    while pending:
        required = pending.popleft()
        key = required.key
        module = key[0]
        builtin = builtins.get((key, required.symbol))
        if key in known:
            provider, name, provided_builtin = known[key]
            if provided_builtin != (builtin is not None):
                raise link_c.LinkError(f"project and builtin provider share {link_c.label(key)}")
        else:
            if len(known) >= 768:
                raise link_c.LinkError("application exceeds the 768-provider build limit")
            if builtin:
                provider, sources, name = builtin
                headers.update(sources)
            elif module in paths:
                provider = emit_unit(compiler, root, paths, module, required.source_name, len(known))
                name = provider.export.source_name
            else:
                raise link_c.LinkError(f"missing project module: {module}")
            known[key] = (provider, name, builtin is not None)
            pending.extend(provider.dependencies)
        if provider.export.key != key or name != required.source_name:
            raise link_c.LinkError(f"provider identity mismatch for {link_c.label(key)}")
        if provider.export.signature.semantic() != required.signature.semantic():
            raise link_c.LinkError(f"provider signature mismatch for {link_c.label(key)}")
    return [unit for unit, _, _ in known.values()], entry.export.key, caller, headers


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("manifest", type=Path)
    parser.add_argument("--compiler", default="ku-bootstrap")
    parser.add_argument("--cc", default=os.environ.get("CC", "cc"))
    parser.add_argument("-o", "--output", type=Path, required=True)
    args = parser.parse_args(argv)
    try:
        compiler = shutil.which(args.compiler)
        if not compiler:
            raise link_c.LinkError(f"bootstrap compiler executable not found: {args.compiler}")
        compiler = Path(compiler).absolute()
        manifest, output = args.manifest.absolute(), args.output.resolve()
        paths = manifest_sources(manifest)
        sources = [manifest.parent / path for path in paths]
        for path in (compiler, manifest, *sources):
            if path.resolve() == output or (output.exists() and path.exists() and output.samefile(path)):
                raise link_c.LinkError(f"output refers to an input file: {path}")

        with tempfile.TemporaryDirectory(prefix="ku-bootstrap-app-") as scratch:
            staged = Path(scratch)
            for name, source in zip(paths, sources):
                destination = staged / name
                destination.parent.mkdir(parents=True, exist_ok=True)
                with destination.open("xb") as file:
                    file.write(bounded_file(source, 1000000))
            units, entry, caller, headers = collect_units(compiler, staged, paths)
            aliases = link_c.checked_aliases(units)
            output.parent.mkdir(parents=True, exist_ok=True)
            link_c.link(units, aliases, output, entry, caller, args.cc, [], [], 120, headers)
    except (OSError, ValueError, subprocess.TimeoutExpired) as error:
        print(f"bootstrap build: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
