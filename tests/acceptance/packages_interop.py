"""Exercise package consumers using a real Ku CLI, outside its source tree."""

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


PUBLIC_KU_PACKAGE = (
    "https://github.com/fengyanweb/ku.git"
    "#780e3955473eaaf1d7b101ed68c946c7173866c9:examples/package"
)


def run(argv, cwd, env=None, success=True):
    command = [str(arg) for arg in argv]
    result = subprocess.run(command, cwd=cwd, env=env, capture_output=True, timeout=240)
    stdout = result.stdout.decode("utf-8", errors="replace").replace("\r\n", "\n")
    stderr = result.stderr.decode("utf-8", errors="replace").replace("\r\n", "\n")
    if success != (result.returncode == 0):
        raise AssertionError(f"{command}: exit {result.returncode}\n{stdout}\n{stderr}")
    return stdout, stderr


def windows_dll_imports(image):
    objdump = shutil.which("objdump.exe") if os.name == "nt" else None
    if not objdump:
        return []
    try:
        info = subprocess.run([objdump, "-p", str(image)], capture_output=True, timeout=15)
    except (OSError, subprocess.TimeoutExpired):
        return []
    return [line.strip() for line in info.stdout.decode(errors="replace").splitlines()
            if "DLL Name:" in line]


def write(path, text):
    path.parent.mkdir(parents=True, exist_ok=True)
    with path.open("w", encoding="utf-8", newline="\n") as output:
        output.write(text)


def init(ku, workspace, source=""):
    workspace.mkdir()
    run([ku, "init"], workspace)
    if source:
        write(workspace / "src" / "main.ku", source)


def import_line(stdout, symbol, path):
    for line in stdout.splitlines():
        match = re.fullmatch(r'import \{ ([\w, ]+) \} from "([^"]+)"', line)
        if match and match[2] == path and symbol in match[1].split(", "):
            return line
    raise AssertionError(f"install did not print a usable import for {symbol} from {path}:\n{stdout}")


def assert_three_entries(ku, project, deploy, expected, bridge):
    suffix = ".exe" if os.name == "nt" else ""
    deploy.mkdir()
    native = deploy / ("native" + suffix)
    runner = deploy / ("runner" + suffix)
    check_output, _ = run([ku, "check", "--locked"], project)
    if check_output:
        print(check_output.strip())
    actual, _ = run([ku, "run", "--locked"], project)
    assert actual == expected, (actual, expected)
    native_output, _ = run([ku, "build", "--release", "--locked", "-o", native], project)
    assert "native c ok:" in native_output, native_output
    runner_output, _ = run(
        [ku, "build", "--backend", "runner", "--release", "--locked", "-o", runner], project
    )
    assert "build ok:" in runner_output and "native c ok:" not in runner_output, runner_output
    runtime = deploy / "ku-runtime.d"
    assert runtime.is_dir() == bridge
    # No leading PATH entry for ku or developer compiler. The layout copied to
    # delivery contains only executables and declared runtime files.
    clean = deploy.with_name(deploy.name + "-moved")
    shutil.copytree(deploy, clean, ignore=shutil.ignore_patterns("*.lock"))
    shell_env = os.environ.copy()
    shell_env["PATH"] = os.defpath
    for key in ("KU_RUNNER_RUSTC", "KU_PYTHON", "PYTHONHOME", "PYTHONPATH"):
        shell_env.pop(key, None)
    # Remove the author and consumer projects before either delivery is run.
    gone = project.with_name(project.name + "-removed")
    project.rename(gone)
    for entry in (native, runner):
        moved = clean / entry.name
        try:
            actual, _ = run([moved], clean, shell_env)
        except AssertionError as error:
            for binary in [moved, *clean.glob("ku-runtime.d/interop/*/*.dll")]:
                imports = windows_dll_imports(binary)
                if imports:
                    error.add_note(f"{binary.name}\n" + "\n".join(imports))
            raise
        assert actual == expected, (entry, actual, expected)
    print(f"{project.name}: interpreter, native Release and explicit Runner passed; copied delivery ran")


def git_package(ku, root):
    project = root / "public-git"
    init(ku, project)
    output, _ = run([ku, "i", PUBLIC_KU_PACKAGE], project)
    print(output.strip())
    imported = import_line(output, "Value", "@demo_pkg/util")
    write(project / "src" / "main.ku", f"{imported}\nfn main() {{ println(Value()) }}\n")
    lock = (project / "ku.lock").read_bytes()
    run([ku, "package", "install", "--locked", "--prod"], project)
    run([ku, "i", "--offline"], project)
    assert (project / "ku.lock").read_bytes() == lock
    assert_three_entries(ku, project, root / "public-git-delivery", "7\n", False)


def native_package(ku, root):
    project = root / "native-provider"
    init(ku, project)
    vendor = project / "vendor" / "unseen"
    write(vendor / "lib.h", "#include <stdint.h>\nint64_t fresh_twice(int64_t value);\n")
    write(vendor / "lib.c", '#include "lib.h"\nint64_t fresh_twice(int64_t value) { return value * 2; }\n')
    notice = "Fixture notices accompany the generated provider.\n"
    write(vendor / "NOTICE", notice)
    output, _ = run([ku, "package", "install", "./vendor/unseen"], project)
    imported = import_line(output, "FreshTwice", "@unseen/native")
    write(project / "src" / "main.ku", f"{imported}\nfn main(): null! {{\n"
          "    println(FreshTwice(21)?)\n    return ok(null)\n}\n")
    lock = (project / "ku.lock").read_bytes()
    run([ku, "i", "--offline"], project)
    assert (project / "ku.lock").read_bytes() == lock
    deploy = root / "native-provider-delivery"
    assert_three_entries(ku, project, deploy, "42\n", True)
    moved = deploy.with_name(deploy.name + "-moved")
    notices = list(moved.glob("ku-runtime.d/interop/*/provider-notices/NOTICE"))
    assert len(notices) == 1 and notices[0].read_text(encoding="utf-8") == notice


def local_python_package(ku, root, python):
    project = root / "local-python"
    init(ku, project)
    write(project / "tools" / "greet.py", 'def greet(name: str) -> str:\n    return "你好 " + name\n')
    output, _ = run(
        [ku, "i", "./tools/greet.py", "--module", "text", "--python", python], project
    )
    imported = import_line(output, "Greet", "@greet/text")
    write(project / "src" / "main.ku", f"{imported}\nfn main(): null! {{\n"
          '    println(Greet("Ku 😀")?)\n    return ok(null)\n}\n')
    assert_three_entries(ku, project, root / "python-delivery", "你好 Ku 😀\n", True)

def local_runner_preserves_resource_base(ku, root):
    project = root / "local-runner"
    init(ku, project, '''import { Value } from "./helper"
import "std.fs"

fn main(): null! {
    println(Value())
    println(fs.read("note.txt")?)
    return ok(null)
}
''')
    write(project / "src" / "helper.ku", "fn Value(): int { return 42 }\n")
    write(project / "src" / "note.txt", "source data")
    output = root / "local-runner-delivery"
    output.mkdir()
    executable = output / ("local.exe" if os.name == "nt" else "local")
    run([ku, "build", "--backend", "runner", "--release", "-o", executable], project)
    actual, _ = run([executable], output)
    assert actual == "42\nsource data\n", actual


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--ku", required=True, type=Path)
    parser.add_argument("--python", type=Path, default=Path(sys.executable))
    parser.add_argument("--public-git", action="store_true")
    parser.add_argument("--local-python", action="store_true")
    arguments = parser.parse_args()
    ku = arguments.ku.resolve(strict=True)
    with tempfile.TemporaryDirectory(prefix="ku-package-acceptance-") as work:
        root = Path(work)
        identity, _ = run([ku, "doctor", "--backend", "runner", "--json"], root)
        doctor = json.loads(identity)
        print(json.dumps({
            "cli_sha256": hashlib.sha256(ku.read_bytes()).hexdigest(),
            "target": doctor["runtime_target"],
            "installation_mode": doctor["installation_mode"],
            "bundle_id": doctor["bundle_id"],
            "source": doctor["runtime_source_commit"],
        }))
        local_runner_preserves_resource_base(ku, root)
        native_package(ku, root)
        if arguments.public_git:
            git_package(ku, root)
        if arguments.local_python:
            local_python_package(ku, root, arguments.python.resolve(strict=True))


if __name__ == "__main__":
    main()
