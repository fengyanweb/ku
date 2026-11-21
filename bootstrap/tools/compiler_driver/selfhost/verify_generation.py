"""Exercise the real linked CLI after all of its typed providers are available."""
import argparse
from dataclasses import replace
from hashlib import sha256
from importlib.util import module_from_spec, spec_from_file_location
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys
import tempfile


def dynamic_imports(binary):
    if sys.platform == "darwin":
        return subprocess.check_output(["otool", "-L", binary], cwd=binary.parent, text=True).splitlines()[1:]
    if sys.platform.startswith("linux"):
        result = subprocess.check_output(["readelf", "-d", binary], text=True)
        return re.findall(r"\(NEEDED\).*\[([^\]]+)\]", result)
    if os.name == "nt":
        objdump = shutil.which("llvm-objdump") or shutil.which("objdump")
        if objdump:
            result = subprocess.check_output([objdump, "-p", binary], text=True)
            imports = re.findall(r"^\s*DLL Name:\s*(\S+)\s*$", result, re.MULTILINE)
            if imports:
                return imports
        dumpbin = shutil.which("dumpbin")
        if dumpbin:
            result = subprocess.check_output([dumpbin, "/nologo", "/dependents", binary], text=True)
            imports = re.findall(r"^\s*(\S+\.dll)\s*$", result, re.MULTILINE | re.IGNORECASE)
            if imports:
                return imports
        raise ValueError("cannot read the compiler host's PE import table with llvm-objdump, objdump or dumpbin")
    raise ValueError(f"cannot inspect executable imports on {sys.platform}")


def frozen_builtin(repo, commit, path, checkout):
    relative = path.relative_to(repo).as_posix()
    content = subprocess.check_output(["git", "show", f"{commit}:{relative}"], cwd=repo)
    if checkout != content and checkout.replace(b"\r\n", b"\n") != content.replace(b"\r\n", b"\n"):
        raise ValueError(f"builtin differs from the frozen Git source: {relative}")
    return content


def sanitizer_profiles(mode, cc, root, extension, host_flags, execute):
    profiles = [("strict_o2", [])]
    if mode == "off":
        return profiles, "disabled by request"
    flags = ["-fsanitize=address,undefined"]
    if mode == "auto" and sys.platform != "darwin":
        source = root / "sanitizer-probe.c"
        binary = root / ("sanitizer-probe" + extension)
        source.write_text("#include <wchar.h>\nint wmain(int argc, wchar_t **argv) { "
                          "return argc < 1 || argv == 0; }\n" if os.name == "nt"
                          else "int main(void) { return 0; }\n", encoding="ascii")
        try:
            execute([cc, "-std=c11", "-O2", *flags, source, "-o", binary, *host_flags], root)
            execute([binary], root)
        except (OSError, AssertionError, subprocess.TimeoutExpired) as error:
            return profiles, f"compile or runtime probe failed: {error}"
    return [*profiles, ("asan_ubsan", flags)], "enabled"


def main():
    arguments = argparse.ArgumentParser(description=__doc__)
    arguments.add_argument("repo", type=Path)
    arguments.add_argument("revision")
    arguments.add_argument("--host-check", action="store_true")
    arguments.add_argument("--exact-ledger", type=Path, required=True)
    arguments.add_argument("--output-dir", type=Path)
    arguments.add_argument("--cc", default="clang", help="target C11 compiler executable")
    arguments.add_argument("--temp-dir", type=Path, default="/private/tmp" if sys.platform == "darwin" else None)
    arguments.add_argument("--sanitizers", choices=("auto", "required", "off"), default="auto",
                           help="retain the macOS gate; probe the selected compiler on Linux and Windows in auto mode")
    args = arguments.parse_args()
    if args.host_check == bool(args.output_dir) or (args.output_dir and args.output_dir.exists()):
        arguments.error("choose either --host-check or a fresh --output-dir")
    repo = args.repo.resolve(strict=True)
    temp_dir = args.temp_dir.resolve(strict=True) if args.temp_dir is not None else None
    revision = args.revision
    host_check = args.host_check
    cc = shutil.which(args.cc)
    if cc is None:
        arguments.error(f"C compiler not found: {args.cc}")
    cc = str(Path(cc).absolute())
    target = subprocess.check_output([cc, "-dumpmachine"], text=True, timeout=20).strip()
    extension = ".exe" if os.name == "nt" else ""
    driver = repo / "bootstrap/tools/compiler_driver"
    sys.path.insert(0, str(driver))
    host_flags, reproducible_cflags = [], []
    if os.name == "nt":
        from build import windows_host_link_flags, windows_stack_reserve

        host_flags = windows_host_link_flags(target)
        if "-windows-msvc" in target:
            reproducible_cflags.append("-mno-incremental-linker-compatible")
            host_flags.extend(["-Xlinker", "/Brepro"])
    sys.path.insert(0, str(repo / "bootstrap/tools"))
    import link_c

    ledger_path = args.exact_ledger.resolve(strict=True)
    ledger_bytes = ledger_path.read_bytes()
    ledger = json.loads(ledger_bytes)
    if ledger["c_target"] != target:
        raise SystemExit("the exact ledger unit compiler and whole linker have different target identities")
    source_commit = subprocess.check_output(["git", "rev-parse", "--verify", revision + "^{commit}"],
                                            cwd=repo, text=True).strip()
    if source_commit != ledger["source_commit"]:
        raise SystemExit("link-input ledger does not describe the requested frozen source commit")
    if ledger.get("mode") != "exact-source" or ledger.get("producer_source_commit") != source_commit:
        raise SystemExit("whole compiler verification requires a producer built from the same frozen source")
    if not host_check and ledger["unresolved"]:
        raise SystemExit(f"whole compiler has {len(ledger['unresolved'])} unresolved units; see {ledger_path}")
    rows = ledger["providers"][:1] if host_check else ledger["providers"]
    if not rows:
        raise SystemExit("the frozen ledger did not produce a genuine BootstrapEntry unit")


    def ledger_member(name):
        path = Path(name)
        if path.parts != (name,) or path.is_absolute() or name == "..":
            raise ValueError(f"not a local ledger member: {name}")
        return ledger_path.with_name(name)


    plans = []
    for item in rows:
        path = ledger_member(item["plan"])
        document = json.loads(path.read_bytes())
        if path.name.startswith("test-") or document.get("mode") == "test-provider":
            raise SystemExit(f"cannot verify a whole compiler with a test provider: {item['plan']}; "
                             "regenerate the typed link-input ledger")
        plans.append(document)
    if not host_check:
        if not any(dep["source_path"] == "semantics/linked_unit.ku" and dep["source_name"] == "EmitLinkedUnit"
                   for dep in plans[0]["dependencies"]):
            raise SystemExit("frozen BootstrapEntry plan predates emit-unit and emit-unit-plan; "
                             "resolve the current compiler-driver receipt")
    providers = {
        "bytes.from_str": link_c.bytes_from_str_provider,
        "bytes.get": link_c.bytes_get_provider,
        "fs.read": link_c.fs_read_provider,
        "string.slice": link_c.string_slice_provider,
    }
    units = []
    for item in rows:
        unit = link_c.Unit.read(ledger_member(item["plan"]), ledger_member(item["c"]))
        assert item["key"] == list(unit.export.key) and item["c_sha256"] == sha256(unit.code).hexdigest()
        assert item["plan_sha256"] == sha256(unit.plan.read_bytes()).hexdigest()
        source = subprocess.check_output(
            ["git", "show", f"{source_commit}:bootstrap/{unit.export.key[0]}"], cwd=repo)
        assert item["source_sha256"] == sha256(source).hexdigest()
        units.append(unit)
    builtin_headers = {}
    for name in (() if host_check else ledger["builtins"]):
        unit, headers = providers[name]()
        units.append(replace(unit, code=frozen_builtin(repo, source_commit, unit.source, unit.code)))
        builtin_headers.update({path: frozen_builtin(repo, source_commit, path, checkout)
                                for path, checkout in headers.items()})
    entry = units[0].export
    assert list(entry.key) == ledger["entry"]
    assert entry.key[0] == "tools/compiler_driver/driver.ku" and plans[0]["export"]["source_name"] == "BootstrapEntry"
    assert [value.semantic() for value in entry.signature.parameters] == [(("builtin", "str"), "view", "bootstrap string")] * 3
    assert entry.signature.returns_result and entry.signature.result.identity == ("builtin", "null")

    spec = spec_from_file_location("ku_compiler_driver_test", driver / "test_driver.py")
    consumer = module_from_spec(spec)
    spec.loader.exec_module(consumer)

    # The product host's executable argument and absolute-path rules are used as
    # written. Provide the same string/result operations on the emitted ABI.
    def frozen_text(path):
        return subprocess.check_output(["git", "show", f"{source_commit}:{path}"], cwd=repo, encoding="utf-8")


    host = frozen_text("bootstrap/tools/compiler_driver/host.c")
    host = host[host.index("#ifdef _WIN32"):]
    start, end = host.index("static int finish("), host.index("static int usage(")
    host = host[:start] + r'''
static int finish(KuBootstrapResultNull result) {
    int status = result.ok ? 0 : 1;
    if (status) {
        if (result.error.message.len)
            fwrite(result.error.message.ptr, 1, result.error.message.len, stderr);
        fputc('\n', stderr);
    }
    ku_projection_string_drop(&result.error.domain);
    ku_projection_string_drop(&result.error.code);
    ku_projection_string_drop(&result.error.message);
    return status;
}
''' + host[end:]
    host = re.sub(r"\bKuResult_null\b", "KuBootstrapResultNull", host)
    host = re.sub(r"\bBootstrapEntry\b", entry.symbol, host)
    host = re.sub(r"\bKuString\b", "KuProjectionString", host)
    host = host.replace("    __ku_sync_reset();\n", "")
    host = """
static KuProjectionString ku_string_static(const uint8_t *input, size_t size) {
    return (KuProjectionString){(uint8_t*)input, size, 0, KU_PROJECTION_STRING_STATIC};
}
""" + host
    if os.name != "nt":
        fs = frozen_text("src/backend/c_fs_common.c")
        start = fs.index("static bool ku_fs_utf8_valid(")
        end = fs.index("\n}", start) + 2
        host = fs[start:end] + host

    with tempfile.TemporaryDirectory(prefix="ku-bootstrap-compiler-cli-", dir=temp_dir) as temp:
        root = Path(temp)
        caller = root / "caller.c"
        caller.write_text(host, encoding="utf-8")
        if host_check:
            (root / "emitted.c").write_bytes(units[0].code)
            driver_source = root / "host-with-generated-entry.c"
            driver_source.write_text(link_c.wrapper(units[0], True))
            command = [cc, "-std=c11", "-O2", "-pedantic-errors", "-Wall", "-Wextra", "-Werror",
                       "-I", driver, "-fsyntax-only", driver_source]
            if os.name == "nt":
                consumer.execute(command, root)
            else:
                subprocess.run(command, cwd=root, check=True, timeout=20)
            print("Strict C11 host and exact BootstrapEntry signature passed")
            return 0
        aliases = link_c.checked_aliases(units)
        binaries = {}
        profiles, sanitizer_status = sanitizer_profiles(args.sanitizers, cc, root, extension, host_flags, consumer.execute)
        for name, flags in profiles:
            binary = root / (name + extension)
            link_c.link(units, aliases, binary, entry.key, caller.read_bytes(), cc,
                        ["-Wall", "-Wextra", "-Werror", "-pedantic-errors", *reproducible_cflags, *flags],
                        [*flags, *host_flags], 60, builtin_headers)
            stack = windows_stack_reserve(binary) if os.name == "nt" else None
            current = root / f"{name}-fixture"
            current.mkdir()
            foreign = current / "不同 工作目录🙂"
            foreign.mkdir()
            file = foreign / "函数🙂.ku"
            file.write_text("fn AddOne(value: int): int { return value + 1 }\n")
            assert consumer.execute([binary, "check", file.name], foreign).stdout == b""
            rendered = consumer.execute([binary, "emit", file.name, "AddOne"], foreign)
            consumer.exercise_emitted(cc, rendered.stdout, foreign, "standalone-module", b"(41)", b"42")
            consumer.exercise_project(binary, cc, current, foreign)
            consumer.exercise_parameter_bindings(binary, cc, foreign)
            consumer.exercise_owned_recovery(binary, cc, foreign)
            imports = dynamic_imports(binary)
            assert not any("libku" in line.lower() for line in imports)
            binaries[name] = {"sha256": sha256(binary.read_bytes()).hexdigest(), "dynamic_imports": imports}
            if stack is not None:
                binaries[name]["stack"] = stack
        args.output_dir.mkdir()
        output_binary = args.output_dir / ("ku-bootstrap" + extension)
        shutil.copy2(root / ("strict_o2" + extension), output_binary)
        assert sha256(output_binary.read_bytes()).hexdigest() == binaries["strict_o2"]["sha256"]
        (args.output_dir / "build-identity.json").write_text(json.dumps(
            {"commit": source_commit, "sha256": binaries["strict_o2"]["sha256"],
             "producer_sha256": ledger["producer_sha256"], "ledger_sha256": sha256(ledger_bytes).hexdigest(),
             "c_compiler": cc, "c_target": target},
            indent=2) + "\n", encoding="utf-8")

    receipt = {"source_commit": ledger["source_commit"], "input_units": len(units), "builtins": ledger["builtins"],
               "oracle": "existing compiler driver project acceptance", "profiles": binaries,
               "c_compiler": cc, "c_target": target, "sanitizers": sanitizer_status,
               "ledger_sha256": sha256(ledger_bytes).hexdigest()}
    output = args.output_dir / "verification.json"
    output.write_text(json.dumps(receipt, indent=2) + "\n", encoding="utf-8")
    print(output)
    return 0


if __name__ == "__main__":
    sys.exit(main())
