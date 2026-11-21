#!/usr/bin/env python3
"""Build the private Ku bootstrap compiler and its typed C argument host."""

import argparse
from contextlib import contextmanager
from functools import cache
from hashlib import sha256
from importlib.util import module_from_spec, spec_from_file_location
import json
import os
from pathlib import Path
import re
import shutil
import signal
import struct
import subprocess
import sys
import tempfile


# Keep the standalone compiler's reservation equal to the Rust CLI worker.
WINDOWS_COMPILER_STACK_BYTES = 8 * 1024 * 1024
MINGW_GCC_TARGET = "x86_64-w64-mingw32"
MINGW_CLANG_TARGET = "x86_64-w64-windows-gnu"
GNU_HOST_PROBE = r"""
#include <_mingw.h>
#include <stdio.h>
#include <wchar.h>
#include <windows.h>

#if !defined(__clang__) || !defined(__MINGW64__) || !defined(_WIN64) || !defined(UNICODE) || defined(_MSC_VER)
#error expected Clang with the x64 MinGW Unicode host target
#endif
_Static_assert(sizeof(void *) == 8 && sizeof(wchar_t) == 2, "expected Windows x64 ABI");

int wmain(int argc, wchar_t **argv) {
    if (argc != 2 || wcscmp(argv[1], L"\u4e2d") != 0 ||
        GetModuleHandleW(L"kernel32.dll") == NULL)
        return 1;
    return puts("ku-gnu-stage1-host-ok") < 0;
}
"""


def windows_host_link_flags(target):
    target = target.lower()
    if target.startswith("x86_64-") and "-windows-msvc" in target:
        return ["-Xlinker", "/ENTRY:wmainCRTStartup", "-Xlinker", "/SUBSYSTEM:CONSOLE",
                "-Xlinker", "/DEFAULTLIB:kernel32.lib",
                "-Xlinker", f"/STACK:{WINDOWS_COMPILER_STACK_BYTES}"]
    if target.startswith("x86_64-") and target.endswith(("-mingw32", "-mingw64", "-windows-gnu")):
        return ["-municode", "-static", f"-Wl,--stack,{WINDOWS_COMPILER_STACK_BYTES}", "-lkernel32"]
    raise ValueError(f"expected an x64 Windows clang or MinGW compiler, got {target!r}")


def windows_stack_reserve(binary):
    with binary.open("rb") as stream:
        dos = stream.read(64)
        if len(dos) != 64 or dos[:2] != b"MZ":
            raise ValueError(f"invalid Windows executable header: {binary}")
        offset, = struct.unpack_from("<I", dos, 60)
        if not 64 <= offset <= 1024 * 1024:
            raise ValueError(f"invalid PE header offset: {binary}")
        stream.seek(offset)
        pe = stream.read(24 + 88)
    if (len(pe) != 24 + 88 or pe[:4] != b"PE\0\0" or
            struct.unpack_from("<H", pe, 4)[0] != 0x8664 or
            struct.unpack_from("<H", pe, 20)[0] < 88 or
            struct.unpack_from("<H", pe, 24)[0] != 0x20B):
        raise ValueError(f"invalid x64 PE32+ header: {binary}")
    reserve, commit = struct.unpack_from("<QQ", pe, 24 + 72)
    if reserve != WINDOWS_COMPILER_STACK_BYTES or not 0 < commit < reserve:
        raise ValueError(f"unexpected bootstrap stack reserve/commit {reserve}/{commit}: {binary}")
    return {"reserved_bytes": reserve, "initial_commit_bytes": commit}


def executable(name):
    result = shutil.which(name)
    if result is None:
        raise ValueError(f"executable not found: {name}")
    return Path(result).absolute()


def run(args, cwd):
    if os.name == "nt":
        run_windows(args, cwd)
        return
    with subprocess.Popen(args, cwd=cwd, start_new_session=True) as child:
        try:
            status = child.wait(timeout=180)
        except BaseException:
            try:
                os.killpg(child.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
            child.wait()
            raise
        if status:
            raise subprocess.CalledProcessError(status, args)


@cache
def windows_process_owner():
    owner_path = (Path(__file__).resolve().parents[3] / ".github" / "scripts"
                  / "verify-native-three-os.py")
    if not owner_path.is_file():
        raise ValueError(f"cannot find the repository Windows process owner: {owner_path}")
    spec = spec_from_file_location("ku_bootstrap_process_owner", owner_path)
    owner = module_from_spec(spec)
    spec.loader.exec_module(owner)
    return owner.run_bounded


def run_windows(args, cwd, *, capture_stdout=False):
    command = [os.fspath(argument) for argument in args]
    result = windows_process_owner()(command, cwd, Path(command[0]).name, check=False)
    if not capture_stdout:
        sys.stdout.buffer.write(result.stdout)
        sys.stdout.buffer.flush()
    sys.stderr.buffer.write(result.stderr)
    sys.stderr.buffer.flush()
    if result.returncode:
        raise subprocess.CalledProcessError(result.returncode, command)
    return result.stdout


def file_sha256(path):
    digest = sha256()
    with Path(path).open("rb") as stream:
        while chunk := stream.read(1024 * 1024):
            digest.update(chunk)
    return digest.hexdigest()


def gnu_stage1_toolchain(clang, gcc, cwd):
    if os.name != "nt":
        raise ValueError("--gnu-stage1-clang requires the x64 Windows GNU host")
    gcc = executable(gcc).resolve(strict=True)
    gcc_target = run_windows([gcc, "-dumpmachine"], cwd, capture_stdout=True).decode("ascii").strip()
    gcc_version = run_windows([gcc, "--version"], cwd, capture_stdout=True)
    if gcc_target != MINGW_GCC_TARGET or not gcc_version.lower().startswith((b"gcc.exe ", b"gcc ")):
        raise ValueError(f"--gnu-stage1-clang requires x64 MinGW GCC, got {gcc_target!r}")
    sysroot = gcc.parent.parent
    startup_path = run_windows([gcc, "-print-file-name=crt2u.o"], cwd, capture_stdout=True)
    startup = Path(startup_path.decode("utf-8").strip())
    if not startup.is_absolute():
        raise ValueError("MinGW GCC did not find its Unicode startup object in the selected toolchain")
    startup = startup.resolve(strict=True)
    if not startup.is_relative_to(sysroot):
        raise ValueError("MinGW GCC picked a Unicode startup object outside the selected toolchain")
    clang = executable(clang).resolve(strict=True)
    clang_target_flags = [f"--target={MINGW_CLANG_TARGET}", f"--sysroot={sysroot}"]
    clang_target = run_windows([clang, *clang_target_flags, "-dumpmachine"], cwd,
                               capture_stdout=True).decode("ascii").strip()
    clang_version = run_windows([clang, "--version"], cwd, capture_stdout=True)
    if clang_target != MINGW_CLANG_TARGET or not clang_version.startswith(b"clang version "):
        raise ValueError(f"expected LLVM Clang targeting x64 MinGW, got {clang_target!r}: {clang_version[:120]!r}")
    return {
        "clang": {"path": str(clang), "target": clang_target, "sha256": file_sha256(clang),
                  "version_sha256": sha256(clang_version).hexdigest()},
        "gcc": {"path": str(gcc), "target": gcc_target, "sha256": file_sha256(gcc),
                "version_sha256": sha256(gcc_version).hexdigest()},
        "sysroot": str(sysroot),
        "unicode_startup": {"path": str(startup), "sha256": file_sha256(startup)},
        "compile_prefix": [str(clang), *clang_target_flags, "-std=c11", "-O2", "-municode", "-static"],
        "link_flags": windows_host_link_flags(gcc_target),
    }


def gnu_stage1_commands(toolchain, source, name):
    obj = str(Path(source).with_suffix(".o"))
    return ([*toolchain["compile_prefix"], "-c", source, "-o", obj],
            [toolchain["gcc"]["path"], obj, "-o", name, *toolchain["link_flags"]])


def require_static_gnu_imports(imports, label):
    names = {name.lower() for name in imports}
    crt = {name for name in names if name in ("ucrtbase.dll", "msvcrt.dll")
           or name.startswith("api-ms-win-crt-") and name.endswith(".dll")}
    if ("kernel32.dll" not in names or not crt or ("msvcrt.dll" in crt and len(crt) != 1) or
            names - (crt | {"kernel32.dll", "kernelbase.dll"})):
        raise ValueError(f"unexpected imports in the static MinGW {label}: {sorted(names)}")


def probe_gnu_stage1(toolchain, cwd):
    print("checking the Clang/MinGW GCC bootstrap host", flush=True)
    source, name = "gnu-host-probe.c", "gnu-host-probe.exe"
    (cwd / source).write_text(GNU_HOST_PROBE, encoding="ascii")
    compile_command, link_command = gnu_stage1_commands(toolchain, source, name)
    run(compile_command, cwd)
    run(link_command, cwd)
    binary = cwd / name
    stack = windows_stack_reserve(binary)
    objdump = Path(toolchain["gcc"]["path"]).with_name("objdump.exe").resolve(strict=True)
    details = run_windows([objdump, "-p", name], cwd, capture_stdout=True).decode("utf-8", errors="replace")
    imports = re.findall(r"^\s*DLL Name:\s*(\S+)\s*$", details, re.IGNORECASE | re.MULTILINE)
    require_static_gnu_imports(imports, "host probe")
    stdout = run_windows([binary, "\u4e2d"], cwd, capture_stdout=True)
    if stdout.splitlines() != [b"ku-gnu-stage1-host-ok"]:
        raise ValueError(f"unexpected MinGW Unicode host probe output: {stdout!r}")
    print("Clang/MinGW GCC bootstrap host probe passed", flush=True)
    return {"status": "passed", "sha256": file_sha256(binary), "dynamic_imports": imports, "stack": stack}


def link_c(cc, source, name, cwd, *, bootstrap_host=False, gnu_stage1=None):
    command = [cc, "-std=c11", "-O2"]
    libraries = []
    if os.name == "nt":
        if bootstrap_host:
            target = run_windows([cc, "-dumpmachine"], cwd, capture_stdout=True)
            target = target.decode("ascii").strip().lower()
            libraries = windows_host_link_flags(target)
            if gnu_stage1 and target != gnu_stage1["gcc"]["target"]:
                raise ValueError(f"the MinGW linker target changed during Stage1: {target!r}")
            if "-municode" in libraries:
                # Keep the existing C and linker options while separating
                # compilation from linking in the bounded stage logs.
                obj = Path(source).with_suffix(".o")
                if gnu_stage1:
                    compile_command, link_command = gnu_stage1_commands(gnu_stage1, source, name)
                else:
                    compile_command = [*command, "-municode", "-static", "-c", source, "-o", obj]
                    link_command = [cc, obj, "-o", name, *libraries]
                print("compiling the MinGW bootstrap host", flush=True)
                run(compile_command, cwd)
                print("linking the MinGW bootstrap host", flush=True)
                run(link_command, cwd)
                windows_stack_reserve(cwd / name)
                return
    elif bootstrap_host:
        command.append("-pthread")
    run([*command, source, "-o", name, *libraries], cwd)
    if os.name == "nt" and bootstrap_host:
        windows_stack_reserve(cwd / name)


@contextmanager
def compiler_scratch(root):
    if os.name != "nt":
        yield
        return
    # Child compilers can leave intermediates after the Job is terminated.
    scratch = root / "tmp"
    scratch.mkdir()
    previous = {key: os.environ.get(key) for key in ("TEMP", "TMP")}
    try:
        for key in previous:
            os.environ[key] = str(scratch)
        yield
    finally:
        for key, value in previous.items():
            if value is None:
                os.environ.pop(key, None)
            else:
                os.environ[key] = value


def build(output, ku, cc, gnu_stage1_clang=None):
    sources = Path(__file__).resolve().parent
    bootstrap = sources.parents[1]
    with tempfile.TemporaryDirectory(prefix=".ku-bootstrap-build-", dir=output.parent) as temp:
        root = Path(temp)
        for stage in ("stage1", "stage2", "stage3", "semantics"):
            destination = root / stage
            destination.mkdir()
            for source in (bootstrap / stage).glob("*.ku"):
                shutil.copyfile(source, destination / source.name)

        work = root / "tools" / "compiler_driver"
        work.mkdir(parents=True)
        for name in ("driver.ku", "host.c"):
            shutil.copyfile(sources / name, work / name)
        program = work / ("ku-bootstrap.exe" if os.name == "nt" else "ku-bootstrap")
        with compiler_scratch(root):
            toolchain = None
            if gnu_stage1_clang:
                toolchain = gnu_stage1_toolchain(gnu_stage1_clang, cc, work)
                with tempfile.TemporaryDirectory(prefix="gnu-host-probe-", dir=root) as location:
                    probe = probe_gnu_stage1(toolchain, Path(location))
            run([ku, "build", "--native", "--offline", "driver.ku"], work)
            link_c(cc, "host.c", program.name, work, bootstrap_host=True, gnu_stage1=toolchain)
        if toolchain:
            compile_command, link_command = gnu_stage1_commands(toolchain, "host.c", program.name)
            identity = {"toolchain": toolchain, "probe": probe, "compile_argv": compile_command,
                        "link_argv": link_command, "driver_c_sha256": file_sha256(work / "driver.c"),
                        "host_object_sha256": file_sha256(work / "host.o")}
            with output.with_suffix(".gnu-host.json").open("x", encoding="utf-8") as receipt:
                receipt.write(json.dumps(identity, indent=2) + "\n")
        os.replace(program, output)


def terminate(signum, _frame):
    raise SystemExit(128 + signum)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("-o", "--output", required=True, type=Path)
    parser.add_argument("--ku", default="ku", help="stage-0 Ku executable")
    parser.add_argument("--cc", default="cc", help="C11 compiler executable")
    parser.add_argument("--gnu-stage1-clang", help="compile only the Windows GNU Stage1 host with this Clang")
    args = parser.parse_args()
    if os.name not in ("posix", "nt"):
        parser.error("this bootstrap host requires POSIX or Windows")
    if os.name == "nt" and args.output.suffix.lower() != ".exe":
        parser.error("Windows bootstrap output must end in .exe")
    if args.gnu_stage1_clang and os.name != "nt":
        parser.error("--gnu-stage1-clang requires the x64 Windows GNU host")
    signal.signal(signal.SIGTERM, terminate)
    try:
        build(args.output.absolute(), executable(args.ku), executable(args.cc), args.gnu_stage1_clang)
    except (OSError, ValueError, subprocess.SubprocessError) as error:
        print(f"bootstrap compiler build: {error}", file=sys.stderr)
        return 1
    except KeyboardInterrupt:
        return 130
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
