#!/usr/bin/env python3
"""Build the private Ku bootstrap compiler and its typed C argument host."""

import argparse
from contextlib import contextmanager
from functools import cache
from importlib.util import module_from_spec, spec_from_file_location
import os
from pathlib import Path
import shutil
import signal
import subprocess
import sys
import tempfile


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


def link_c(cc, source, name, cwd, *, bootstrap_host=False):
    command = [cc, "-std=c11", "-O2"]
    libraries = []
    if os.name == "nt":
        if bootstrap_host:
            target = run_windows([cc, "-dumpmachine"], cwd, capture_stdout=True)
            target = target.decode("ascii").strip().lower()
            if target.startswith("x86_64-") and "-windows-msvc" in target:
                libraries = ["-Xlinker", "/ENTRY:wmainCRTStartup", "-Xlinker",
                             "/SUBSYSTEM:CONSOLE", "-Xlinker", "/DEFAULTLIB:kernel32.lib"]
            elif target.startswith("x86_64-") and target.endswith(
                ("-mingw32", "-mingw64", "-windows-gnu")
            ):
                command.extend(["-municode", "-static"])
                libraries = ["-lkernel32"]
            else:
                raise ValueError(f"expected an x64 Windows clang or MinGW compiler, got {target!r}")
    elif bootstrap_host:
        command.append("-pthread")
    run([*command, source, "-o", name, *libraries], cwd)


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


def build(output, ku, cc):
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
            run([ku, "build", "--native", "--offline", "driver.ku"], work)
            link_c(cc, "host.c", program.name, work, bootstrap_host=True)
        os.replace(program, output)


def terminate(signum, _frame):
    raise SystemExit(128 + signum)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("-o", "--output", required=True, type=Path)
    parser.add_argument("--ku", default="ku", help="stage-0 Ku executable")
    parser.add_argument("--cc", default="cc", help="C11 compiler executable")
    args = parser.parse_args()
    if os.name not in ("posix", "nt"):
        parser.error("this bootstrap host requires POSIX or Windows")
    if os.name == "nt" and args.output.suffix.lower() != ".exe":
        parser.error("Windows bootstrap output must end in .exe")
    signal.signal(signal.SIGTERM, terminate)
    try:
        build(args.output.absolute(), executable(args.ku), executable(args.cc))
    except (OSError, ValueError, subprocess.SubprocessError) as error:
        print(f"bootstrap compiler build: {error}", file=sys.stderr)
        return 1
    except KeyboardInterrupt:
        return 130
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
