#!/usr/bin/env python3
"""Exercise the bootstrap argv host and the independently emitted C."""

import argparse
import hashlib
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys
import tempfile

from build import executable, link_c, windows_process_owner


def execute(args, cwd, status=0):
    if os.name == "nt":
        command = [os.fspath(argument) for argument in args]
        result = windows_process_owner()(command, cwd, Path(command[0]).name, check=False)
    else:
        result = subprocess.run(args, cwd=cwd, capture_output=True, timeout=400)
    if result.returncode != status:
        raise AssertionError(
            f"{args}: expected exit {status}, got {result.returncode}\n"
            f"{result.stdout.decode(errors='replace')}"
            f"{result.stderr.decode(errors='replace')}"
        )
    return result


def exercise_project(compiler, root, caller):
    project = root / "项目 清单🙂"
    src, lib = project / "src", project / "lib"
    src.mkdir(parents=True)
    lib.mkdir()
    manifest = project / "modules.txt"
    library = lib / "数字🙂.ku"
    library.write_text(
        "struct Counter { value: int }\n"
        "fn Increment(value: int): int { return value + 1 }\n",
        encoding="utf-8",
    )
    main = src / "main.ku"
    valid = (
        'import { Counter as Record, Increment as Step } from "../lib/数字🙂.ku"\n'
        'struct Packet { item: Record }\n'
        'fn Answer(): int { return Step(41) }\n'
    )
    main.write_text(valid, encoding="utf-8")
    listed = "src/main.ku\r\nlib/数字🙂.ku\r\n".encode()
    manifest.write_bytes(listed)
    # A matching filename in the caller's directory must never substitute
    # for a manifest-relative project module.
    (caller / "src").mkdir()
    (caller / "src/main.ku").write_text("fn broken( ", encoding="utf-8")
    relative = Path("..") / project.name / manifest.name
    assert execute([compiler, "check-project", relative], caller).stdout == b""
    assert execute([compiler, "check-project", manifest], caller).stdout == b""
    if os.name == "nt":
        assert execute([compiler, "check-project", relative.as_posix()], caller).stdout == b""
    assert b"usage:" in execute(
        [compiler, "check-project", relative, "extra"], caller, 2
    ).stderr

    def project_error(code, logical=None):
        result = execute([compiler, "check-project", relative], caller, 1)
        assert not result.stdout and code in result.stderr, result.stderr
        if logical is not None:
            assert logical + b": " in result.stderr, result.stderr
            assert re.search(rb"[0-9]+:[0-9]+@[0-9]+\.\.[0-9]+:[0-9]+@[0-9]+", result.stderr), result.stderr

    for original, replacement, code in [
        ("Increment as Step", "Unknown as Step", b"missing_export"),
        ("Increment as Step", "Increment as Record", b"duplicate_import_alias"),
        ("item: Record", "item: Counter", b"undefined_type"),
        ("Step(41)", "Step()", b"call_arity"),
        ("Step(41)", 'Step("not an integer")', b"call_argument_type"),
    ]:
        main.write_text(valid.replace(original, replacement, 1), encoding="utf-8")
        project_error(code, b"src/main.ku")
    main.write_text("fn broken( ", encoding="utf-8")
    project_error(b"bootstrap.parser.stage3|unexpected_token", b"src/main.ku")
    main.write_text(valid, encoding="utf-8")

    manifest.write_bytes(b"src/main.ku\n")
    project_error(b"unresolved_import", b"src/main.ku")
    (caller / "only-in-caller.ku").write_text("fn Present(): int { return 1 }\n")
    manifest.write_bytes(b"only-in-caller.ku\n")
    project_error(b"only-in-caller.ku: ")
    (root / "escape.ku").write_text("fn Present(): int { return 1 }\n")
    manifest.write_bytes(b"../escape.ku\n")
    project_error(b"project_path")
    manifest.write_text("".join(f"unit{index}.ku\n" for index in range(65)))
    project_error(b"accepts at most 64 modules")
    manifest.write_bytes(b"x" * 32769)
    project_error(b"exceeds 32768 UTF-8 bytes")
    manifest.write_bytes(listed)
    assert execute([compiler, "check-project", relative], caller).stdout == b""


def exercise(ku, cc):
    here = Path(__file__).resolve().parent
    with tempfile.TemporaryDirectory(prefix="ku-bootstrap-argv-test-") as temp:
        root = Path(temp)
        sources = root / "source copy"
        bootstrap = sources / "bootstrap"
        tools = bootstrap / "tools" / "compiler_driver"
        shutil.copytree(here, tools)
        for stage in ("stage1", "stage2", "stage3", "semantics"):
            shutil.copytree(here.parents[1] / stage, bootstrap / stage)
        if os.name == "nt":
            owner = Path(".github/scripts/verify-native-three-os.py")
            (sources / owner.parent).mkdir(parents=True)
            shutil.copyfile(here.parents[2] / owner, sources / owner)
        installed = root / "installed compiler"
        installed.mkdir()
        compiler = installed / ("ku-bootstrap.exe" if os.name == "nt" else "ku-bootstrap")
        command = [sys.executable, "-B", tools / "build.py", "--ku", ku, "--cc", cc,
                   "-o", compiler]
        execute(command, root)
        assert list(installed.iterdir()) == [compiler]

        # The checked C signature must reject a generated by-value parameter.
        source = tools / "driver.ku"
        original = source.read_text(encoding="utf-8")
        changed = original.replace("&symbol: str", "symbol: str", 1)
        assert changed != original
        source.write_text(changed, encoding="utf-8")
        before = hashlib.sha256(compiler.read_bytes()).digest()
        failure = execute(command, root, 1)
        assert b"bootstrap driver must provide BootstrapEntry" in failure.stderr
        assert hashlib.sha256(compiler.read_bytes()).digest() == before
        assert list(installed.iterdir()) == [compiler]
        shutil.rmtree(sources)

        caller = root / "调用目录 🙂"
        caller.mkdir()
        source = caller / "含空 格🙂.ku"
        source.write_text("fn AddOne(value: int): int { return value + 1 }\n", encoding="utf-8")
        assert execute([compiler, "check", source.name], caller).stdout == b""
        assert execute([compiler, "check", source], caller).stdout == b""
        invalid = caller / "malformed.ku"
        invalid.write_text("fn broken( ", encoding="utf-8")
        diagnostic = execute([compiler, "check", invalid.name], caller, 1)
        assert diagnostic.stdout == b""
        assert b"bootstrap.parser.stage3|unexpected_token" in diagnostic.stderr
        bad_call = caller / "bad call.ku"
        bad_call.write_text(
            'fn Takes(value: int): int { return value }\n'
            'fn Wrong(): int { return Takes("text") }\n',
            encoding="utf-8",
        )
        assert b"call_argument_type" in execute(
            [compiler, "check", bad_call.name], caller, 1
        ).stderr
        missing = execute([compiler, "check", "never-created.ku"], caller, 1)
        assert missing.stdout == b"" and missing.stderr
        assert b"usage:" in execute([compiler, "check", source.name, "extra"], caller, 2).stderr
        assert b"1 to 512 bytes of UTF-8" in execute(
            [compiler, "check", "x" * 513], caller, 2
        ).stderr
        malformed = "\ud800.ku" if os.name == "nt" else b"\xff.ku"
        assert b"1 to 512 bytes of UTF-8" in execute(
            [compiler, "check", malformed], caller, 2
        ).stderr
        assert b"1 to 512 bytes of UTF-8" in execute(
            [compiler, "emit", source.name, "界" * 171], caller, 2
        ).stderr
        valid_width = execute([compiler, "emit", source.name, "界" * 170 + "ab"], caller, 1)
        assert b"1 to 512 bytes of UTF-8" not in valid_width.stderr
        if os.name == "nt":
            verbatim = "\\\\?\\" + str(source)
            assert b"UTF-8 path limit" in execute([compiler, "check", verbatim], caller, 2).stderr
        no_function = execute([compiler, "emit", source.name, "Unknown"], caller, 1)
        assert no_function.stdout == b"" and no_function.stderr

        emitted = execute([compiler, "emit", source.name, "AddOne"], caller).stdout
        symbol = re.match(rb"/\* ([A-Za-z_][0-9A-Za-z_]*) \*/\n", emitted)
        assert symbol, emitted[:200]
        derived = caller / "derived.c"
        derived.write_bytes(
            emitted + b"\nint main(void) { return " + symbol[1] + b"(41) == 42 ? 0 : 1; }\n"
        )
        program = "derived.exe" if os.name == "nt" else "derived"
        link_c(cc, "derived.c", program, caller)
        execute([caller / program], caller)
        exercise_project(compiler, root, caller)
        print("bootstrap argv, module checks, signature and standalone C verified")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--ku", default="ku")
    parser.add_argument("--cc", default="cc")
    args = parser.parse_args()
    exercise(executable(args.ku), executable(args.cc))


if __name__ == "__main__":
    main()
