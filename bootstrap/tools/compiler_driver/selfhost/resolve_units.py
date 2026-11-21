"""Emit the exact typed compiler closure with a same-source bootstrap executable."""
import argparse
from collections import deque
from hashlib import sha256
import json
from pathlib import Path
import posixpath
import re
import shutil
import subprocess
import sys
import tempfile

def builtin_signatures(link_c):
    result = {}
    for name, provider in (("fs.read", link_c.fs_read_provider), ("string.slice", link_c.string_slice_provider),
                           ("bytes.from_str", link_c.bytes_from_str_provider), ("bytes.get", link_c.bytes_get_provider)):
        unit, _ = provider()
        result[unit.export.key] = (name, unit.export.signature.semantic())
    return result


def generate_exact(repo, revision, producer, limit, output, cc, temp_dir):
    sys.path.insert(0, str(repo / "bootstrap/tools"))
    import link_c

    source_commit = subprocess.check_output(
        ["git", "rev-parse", "--verify", revision + "^{commit}"], cwd=repo, text=True).strip()
    identity = json.loads((producer.parent / "build-identity.json").read_bytes())
    producer_sha = sha256(producer.read_bytes()).hexdigest()
    if identity["commit"] != source_commit or identity["sha256"] != producer_sha:
        raise ValueError("the bootstrap producer does not match its same-source build identity")
    target = subprocess.check_output([cc, "-dumpmachine"], text=True, timeout=20).strip()
    if identity.get("c_target", target) != target:
        raise ValueError("the supplied bootstrap producer and unit compiler have different target identities")
    entry_path = "tools/compiler_driver/driver.ku"
    entry_name = "BootstrapEntry"
    output.mkdir()
    files = {}
    builtins = builtin_signatures(link_c)
    pending = deque([(None, None)])
    seen = {}
    providers, unresolved, used_builtins = [], [], set()
    entry_key = None
    attempts = 0
    strict = ["-std=c11", "-O2", "-pedantic-errors", "-Wall", "-Wextra", "-Werror"]

    def run(command, cwd, phase):
        result = subprocess.run(command, cwd=cwd, capture_output=True, timeout=30)
        if result.returncode or result.stderr:
            diagnostic = result.stderr.decode("utf-8", errors="replace").strip()
            raise ValueError(f"{phase}, exit {result.returncode}: {diagnostic}")
        return result.stdout

    def read_source(path):
        if path not in files:
            files[path] = run(["git", "show", f"{source_commit}:bootstrap/{path}"], repo, "read frozen source")
        return files[path]

    def stage_closure(path, scratch):
        ordered, seen_paths, waiting = [], set(), deque([path])
        while waiting:
            item = waiting.popleft()
            if item in seen_paths:
                continue
            seen_paths.add(item)
            ordered.append(item)
            if len(ordered) > 64:
                raise ValueError("the frozen module closure exceeds the 64-module manifest limit")
            source = read_source(item)
            destination = scratch / item
            destination.parent.mkdir(parents=True, exist_ok=True)
            destination.write_bytes(source)
            for relative in re.findall(rb'(?:from|import)\s+"(\.\.?/[^"\n]+\.ku)"', source):
                target = posixpath.normpath(posixpath.join(posixpath.dirname(item), relative.decode()))
                waiting.append(link_c.module_path(target, "frozen import"))
        index = scratch / "modules.txt"
        index.write_bytes(("\n".join(ordered) + "\n").encode("utf-8"))
        sources = {item: sha256(files[item]).hexdigest() for item in ordered}
        return index, sources

    with tempfile.TemporaryDirectory(prefix="ku-bootstrap-exact-providers-", dir=temp_dir) as temporary:
        root = Path(temporary)
        include = root / "include"
        include.mkdir()
        (include / "fs_read.h").write_bytes(read_source("tools/compiler_driver/fs_read.h"))
        while pending:
            caller, request = pending.popleft()
            demand = link_c.Declaration.read(request, "requested provider") if request else None
            key = demand.key if demand else None
            path = key[0] if demand else entry_path
            name = request["source_name"] if demand else entry_name
            signature = demand.signature.semantic() if demand else None
            report = {"key": key, "source_path": path, "source_name": name, "required_by": caller}
            if key in seen:
                if seen[key] != (name, signature):
                    unresolved.append({**report, "status": "conflicting_request", "signature": signature})
                continue
            if demand:
                seen[key] = (name, signature)
            if key in builtins:
                builtin, expected = builtins[key]
                if signature == expected:
                    used_builtins.add(builtin)
                else:
                    unresolved.append({**report, "status": "builtin_abi_mismatch", "signature": signature})
                continue
            if attempts == limit:
                unresolved.append({**report, "status": "limit_reached"})
                continue
            attempts += 1
            phase = "stage"
            try:
                work = root / str(attempts)
                work.mkdir()
                manifest_file, manifest_sources = stage_closure(path, work)
                phase = "emit-unit-plan"
                plan_bytes = run([producer, "emit-unit-plan", manifest_file, name], work, phase)
                document = json.loads(plan_bytes, object_pairs_hook=link_c.no_duplicate_fields)
                actual = link_c.Declaration.read(document["export"], name)
                if (document["schema"] != link_c.PLAN or actual.key[0] != path or
                        document["export"]["source_name"] != name):
                    raise ValueError(f"the plan did not identify the source function {path}:{name}")
                if demand and (actual.key != key or actual.signature.semantic() != signature):
                    raise ValueError(f"requested {key} {signature!r}; got {actual.key} {actual.signature.semantic()!r}")
                if not demand:
                    entry_key = actual.key
                    seen[entry_key] = (name, actual.signature.semantic())
                    if ([param.semantic() for param in actual.signature.parameters] !=
                            [(("builtin", "str"), "view", "bootstrap string")] * 3 or
                            not actual.signature.returns_result or actual.signature.result.identity != ("builtin", "null")):
                        raise ValueError("BootstrapEntry does not have the checked driver ABI")
                phase = "emit-unit"
                code = run([producer, "emit-unit", manifest_file, name], work, phase)
                candidate = work / "provider.c"
                candidate.write_bytes(code)
                receipt = work / "provider.plan.json"
                receipt.write_bytes(plan_bytes)
                link_c.Unit.read(receipt, candidate)
                phase = "strict-c11-o2"
                obj = work / "provider.o"
                run([cc, *strict, "-I", include, "-c", candidate, "-o", obj], work, phase)
                saved_code = output / f"{attempts:03d}.c"
                saved_plan = output / f"{attempts:03d}.plan.json"
                saved_code.write_bytes(code)
                saved_plan.write_bytes(plan_bytes)
                providers.append({"key": actual.key, "source_name": name, "source_sha256": manifest_sources[path],
                                  "manifest_sources": manifest_sources, "c": saved_code.name,
                                  "plan": saved_plan.name, "c_sha256": sha256(code).hexdigest(),
                                  "plan_sha256": sha256(plan_bytes).hexdigest(),
                                  "object_sha256": sha256(obj.read_bytes()).hexdigest()})
                pending.extend((actual.key, item) for item in document["dependencies"])
            except (OSError, ValueError, KeyError, subprocess.TimeoutExpired) as error:
                unresolved.append({**report, "status": "rejected", "phase": phase, "diagnostic": str(error)})

    ledger = {"mode": "exact-source", "source_commit": source_commit, "entry": entry_key,
              "producer_source_commit": identity["commit"], "producer_sha256": producer_sha,
              "limit": limit, "attempted": attempts, "providers": providers, "unresolved": unresolved,
              "builtins": sorted(used_builtins), "c_flags": strict, "linked": False,
              "c_compiler": cc, "c_target": target,
              "scope": "Same frozen input source and producer used for each emitted unit."}
    result = output / "ledger.json"
    result.write_text(json.dumps(ledger, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")
    print(f"{len(providers)} providers compiled; {len(unresolved)} unresolved; {result}")
    diagnostics = [issue for issue in unresolved if "diagnostic" in issue]
    for issue in (diagnostics or unresolved)[:3]:
        reason = issue.get("phase", issue["status"])
        diagnostic = issue.get("diagnostic", issue["status"])
        print(f"{issue['source_path']}:{issue['source_name']} [{reason}]: {diagnostic[:550]}", file=sys.stderr)
    return int(bool(unresolved))


def main():
    cli = argparse.ArgumentParser(description=__doc__)
    cli.add_argument("repo", type=Path)
    cli.add_argument("revision")
    cli.add_argument("--producer", required=True, type=Path)
    cli.add_argument("--limit", required=True, type=int)
    cli.add_argument("--output-dir", required=True, type=Path, help="fresh output directory")
    cli.add_argument("--cc", default="/usr/bin/clang" if sys.platform == "darwin" else "clang", help="target C11 compiler")
    cli.add_argument("--temp-dir", type=Path)
    args = cli.parse_args()
    if args.limit < 1:
        cli.error("--limit must be positive")
    cc = shutil.which(args.cc)
    if cc is None:
        cli.error(f"C compiler not found: {args.cc}")
    temp_dir = args.temp_dir.resolve(strict=True) if args.temp_dir is not None else None
    return generate_exact(args.repo.resolve(strict=True), args.revision, args.producer.resolve(strict=True),
                          args.limit, args.output_dir.resolve(), str(Path(cc).absolute()), temp_dir)


if __name__ == "__main__":
    sys.exit(main())
