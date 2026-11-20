#!/usr/bin/env python3
"""Build the private Ku bootstrap compiler and its typed C argument host."""

import argparse
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
        run([ku, "build", "--native", "--offline", "driver.ku"], work)
        program = work / "ku-bootstrap"
        run([cc, "-std=c11", "-O2", "-pthread", "host.c", "-o", program], work)
        os.replace(program, output)


def terminate(signum, _frame):
    raise SystemExit(128 + signum)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("-o", "--output", required=True, type=Path)
    parser.add_argument("--ku", default="ku", help="stage-0 Ku executable")
    parser.add_argument("--cc", default="cc", help="C11 compiler executable")
    args = parser.parse_args()
    if os.name != "posix":
        parser.error("this bootstrap host requires POSIX UTF-8 argv; Windows needs a UTF-16 adapter")
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
