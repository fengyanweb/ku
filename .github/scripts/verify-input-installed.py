#!/usr/bin/env python3
"""Build and exercise an isolated CLI/Runner SDK for the input feature."""

import argparse
import errno
import hashlib
import json
import os
from pathlib import Path
from queue import Queue
import shutil
import subprocess
import sys
from threading import Thread
import time

PROMPT = '请输入名字：'
GREETING = '你好，小明！'
TEMPLATE = r"""
fn main(): null! {
    name, age = input(`我的名字是${}，今年${:int}岁。`)?
    println(name)
    println(age)
    return ok(null)
}
"""
PLAIN = r"""
fn main(): null! {
    try {
        name = input("请输入名字：")?
        println("[" + name + "]")
    } catch (problem) {
        println(problem.domain + "." + problem.code)
    }
    return ok(null)
}
"""


def invoke(command, cwd, env, data=None, *, timeout=90):
    result = subprocess.run(command, cwd=cwd, env=env, input=data,
                            capture_output=True, timeout=timeout)
    if result.returncode:
        raise RuntimeError((command, result.returncode, result.stdout, result.stderr))
    return result


def versioned_environment(repo, target):
    env = os.environ.copy()
    for key in ("KU_CC", "KU_RUNNER_RUSTC", "KU_RUNNER_RLIB", "RUSTUP_TOOLCHAIN",
                "RUST_PATH", "RUSTFLAGS", "CARGO_ENCODED_RUSTFLAGS", "CARGO_TARGET_DIR"):
        env.pop(key, None)
    env.update(CARGO_INCREMENTAL="0", PYTHONUTF8="1", TERM="xterm-256color")
    if target.endswith("-windows-gnu"):
        candidates = (
            Path(r"C:\mingw64\bin\gcc.exe"),
            Path(r"C:\msys64\mingw64\bin\gcc.exe"),
            Path(r"C:\tools\mingw64\bin\gcc.exe"),
            Path(shutil.which("gcc") or "missing-gcc"),
        )
        for candidate in candidates:
            if not candidate.is_file():
                continue
            probe = invoke([str(candidate), "-dumpmachine"], repo, env, timeout=10)
            if probe.stdout.strip() == b"x86_64-w64-mingw32":
                compiler = candidate.resolve()
                break
        else:
            raise RuntimeError("matching preinstalled x64 MinGW GCC unavailable")
        for name in ("g++.exe", "ar.exe", "windres.exe"):
            if not compiler.with_name(name).is_file():
                raise RuntimeError(f"incomplete MinGW installation: missing {name}")
        env["PATH"] = str(compiler.parent) + os.pathsep + env["PATH"]
        env["KU_CC"] = str(compiler)
        env["CC"] = str(compiler)
        env["CARGO_TARGET_X86_64_PC_WINDOWS_GNU_LINKER"] = str(compiler)
    else:
        env["KU_CC"] = "clang"
    print(invoke([env["KU_CC"], "--version"], repo, env).stdout.decode(), flush=True)
    return env


def assemble(repo, commit, target, work, env):
    head = invoke(["git", "rev-parse", "HEAD"], repo, env).stdout.decode().strip()
    assert head == commit, (head, commit)
    manifests = Path(__file__).with_name("input-ci-runtime-bundle.py")
    channel = invoke([sys.executable, str(manifests), "toolchain", "--repo", str(repo)],
                     work, env).stdout.decode().strip()
    toolchain = channel + "-" + target
    invoke(["rustup", "toolchain", "install", toolchain, "--profile", "minimal",
            "--no-self-update"], work, env, timeout=240)
    rustc = invoke(["rustc", "+" + toolchain, "-vV"], work, env).stdout
    assert f"host: {target}".encode() in rustc, rustc
    print(rustc.decode(), flush=True)

    cargo = work / "cargo"
    command = ["cargo", "+" + toolchain, "build", "--locked", "--release", "--lib",
               "--bin", "ku", "--target", target, "--target-dir", str(cargo),
               "--message-format=json-render-diagnostics"]
    print(f"Building {commit} for {target} with Cargo --locked --release", flush=True)
    build = invoke(command, repo, env, timeout=900)
    profile = cargo / target / "release"
    suffix = ".exe" if os.name == "nt" else ""
    executable = "ku" + suffix
    found = json.loads(invoke([str(profile / executable), "doctor", "--json"],
                              work, env).stdout)
    assert found["source_commit"] == commit, found
    for key in ("ku_version", "build_source_sha256", "build_identity_sha256"):
        print(f"{key} {found[key]}", flush=True)

    sdk = work / "installed"
    deps = sdk / "deps"
    deps.mkdir(parents=True)
    shutil.copy2(profile / executable, sdk / executable)
    shutil.copy2(profile / "libku.rlib", sdk / "libku.rlib")
    for path in (profile / "deps").glob("*.rlib"):
        if not path.name.startswith("libku-"):
            shutil.copy2(path, deps / path.name)
    for line in build.stdout.splitlines():
        record = json.loads(line)
        if record["reason"] != "compiler-artifact" or "proc-macro" not in record["target"]["kind"]:
            continue
        for entry in record["filenames"]:
            path = Path(entry)
            if path.suffix in (".so", ".dll", ".dylib"):
                assert path.resolve().is_relative_to(cargo), path
                shutil.copy2(path, deps / path.name)

    identity = work / "doctor.json"
    identity.write_text(json.dumps(found), encoding="utf-8")
    compiler = work / "rustc.txt"
    compiler.write_bytes(rustc.replace(b"\r\n", b"\n"))
    args = ["--bundle", str(sdk), "--version", found["ku_version"],
            "--commit", commit, "--target", target]
    invoke([sys.executable, str(manifests), "create", *args, "--repo", str(repo),
            "--rustc-version-file", str(compiler), "--build-identity-file", str(identity)], work, env)
    print(invoke([sys.executable, str(manifests), "verify", *args], work, env).stdout.decode(), flush=True)
    installed = sdk / executable
    print(f"Installed CLI sha256 {hashlib.file_digest(installed.open('rb'), 'sha256').hexdigest()}",
          flush=True)
    return installed, suffix


def build_modes(ku, source, cwd, env, suffix):
    native = cwd / (source.stem + "-native" + suffix)
    runner = cwd / (source.stem + "-runner" + suffix)
    for kind, expected, path, flags in [
        ("default native", b"native c ok:", native, []),
        ("explicit Runner", b"build ok:", runner, ["--backend", "runner"]),
    ]:
        result = invoke([str(ku), "build", "--release", *flags, source.name, "-o", path.name], cwd, env)
        assert expected in result.stdout and path.is_file(), (kind, result)
    return [("ku run", [str(ku), "run", source.name]), ("default native C", [str(native)]),
            ("explicit Runner", [str(runner)])]


def output(command, cwd, env, data):
    result = invoke(command, cwd, env, data, timeout=12)
    assert not result.stderr, (command, result.stderr)
    return result.stdout.replace(b"\r\n", b"\n").decode("utf-8")


def read_prompt_before_input(command, cwd, env):
    child = subprocess.Popen(command, cwd=cwd, env=env, stdin=subprocess.PIPE,
                             stdout=subprocess.PIPE, stderr=subprocess.PIPE)
    prompt = PROMPT.encode()
    result = Queue(maxsize=1)
    reader = Thread(target=lambda: result.put(child.stdout.read(len(prompt))), daemon=True)
    try:
        reader.start()
        assert result.get(timeout=8) == prompt and child.poll() is None
        remaining, errors = child.communicate("小明🙂\r\n".encode(), timeout=8)
        assert child.returncode == 0 and not errors and remaining.rstrip(b"\r\n").decode() == "[小明🙂]"
    finally:
        if child.poll() is None:
            child.kill()
            child.wait()


def posix_console(command, cwd, env, *, is_template):
    import pty
    import re
    import select
    import termios

    master, slave = pty.openpty()
    original = termios.tcgetattr(slave)
    child = subprocess.Popen(command, cwd=cwd, env=env, stdin=slave, stdout=slave, stderr=slave)
    collected = b""

    def expect(token):
        nonlocal collected
        deadline = time.monotonic() + 8
        while token.encode() not in collected:
            time_left = deadline - time.monotonic()
            if time_left <= 0 or not select.select([master], [], [], time_left)[0]:
                raise TimeoutError((token, collected))
            try:
                chunk = os.read(master, 4096)
            except OSError as exc:
                if exc.errno != errno.EIO:
                    raise
                chunk = b""
            if not chunk:
                raise RuntimeError((token, collected))
            collected += chunk

    try:
        if is_template:
            expect("我的名字是")
            assert "，今年".encode() not in collected
            os.write(master, "小明\r".encode())
            expect("，今年")
            assert "岁。".encode() not in collected
            os.write(master, b"0018\r")
            expect("岁。\r\n小明\r\n18\r\n")
            visible = re.sub(rb"\x1b\[[?0-9;]*[A-Za-z]", b"", collected)
            assert visible.startswith("我的名字是小明，今年0018岁。\r\n".encode()), visible
        else:
            expect(PROMPT)
            assert collected == PROMPT.encode() and child.poll() is None
            os.write(master, "小明\r".encode())
            expect(GREETING)
            assert collected.startswith((PROMPT + "小明\r\n" + GREETING).encode()), collected
        assert child.wait(timeout=5) == 0
        assert termios.tcgetattr(slave) == original
    finally:
        if child.poll() is None:
            child.kill()
            child.wait()
        os.close(master)
        os.close(slave)


def verify_installed(repo, commit, target, work, env, ku, suffix):
    cwd = work / "outside-source"
    cwd.mkdir()
    doctor = json.loads(invoke([str(ku), "doctor", "--backend", "runner", "--json"], cwd, env).stdout)
    assert doctor["ready"] and doctor["installation_mode"] == "release-bundle", doctor
    assert Path(doctor["installation_root"]).resolve() == ku.parent, doctor
    assert Path(doctor["runtime_rlib"]).resolve() == ku.with_name("libku.rlib"), doctor
    assert doctor["runtime_source_commit"] == commit, doctor
    installed = json.loads(ku.with_name("ku-runtime.json").read_text(encoding="utf-8"))
    assert doctor["bundle_id"] == installed["bundle_id"] and installed["host"] == target, doctor
    print(f"Confirmed installed CLI {ku} matches runtime {doctor['bundle_id']} at {commit}", flush=True)

    example = cwd / "terminal_input.ku"
    shutil.copy2(repo / "examples" / example.name, example)
    plain = cwd / "plain.ku"
    template = cwd / "template.ku"
    plain.write_text(PLAIN, encoding="utf-8")
    template.write_text(TEMPLATE, encoding="utf-8")
    examples = build_modes(ku, example, cwd, env, suffix)
    plains = build_modes(ku, plain, cwd, env, suffix)
    templates = build_modes(ku, template, cwd, env, suffix)
    cases = [
        (b"\r\n", "[]\n"), (b"\n", "[]\n"), (b"", "input.eof\n"),
        (" 小明🙂 \r\n".encode(), "[ 小明🙂 ]\n"), ("无换行".encode(), "[无换行]\n"),
        (b"x\r", "[x\r]\n"),
    ]
    for (mode, example_command), (_, plain_command), (_, template_command) in zip(examples, plains, templates, strict=True):
        assert output(example_command, cwd, env, "小明\r\n".encode()) == PROMPT + GREETING + "\n"
        for data, expected in cases:
            actual = output(plain_command, cwd, env, data)
            assert actual == PROMPT + expected, (mode, data, actual, expected)
        assert output(template_command, cwd, env, "小明🙂\r\n0018".encode()) == "小明🙂\n18\n"
        read_prompt_before_input(plain_command, cwd, env)
        print(f"{mode}: installed pipe Unicode/CRLF/blank/EOF and prompt before read passed", flush=True)

    if os.name == "nt":
        from verify_input_windows_console import windows_console
        windows_console(cwd, env, examples, templates)
    else:
        for (mode, example_command), (_, template_command) in zip(examples, templates, strict=True):
            posix_console(example_command, cwd, env, is_template=False)
            posix_console(template_command, cwd, env, is_template=True)
            print(f"{mode}: real POSIX PTY prompt and template, termios restored; simulated keys", flush=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--repo", required=True, type=Path)
    parser.add_argument("--commit", required=True)
    parser.add_argument("--target", required=True)
    parser.add_argument("--work-root", required=True, type=Path)
    args = parser.parse_args()
    repo, work = args.repo.resolve(), args.work_root.resolve()
    work.mkdir()
    env = versioned_environment(repo, args.target)
    ku, suffix = assemble(repo, args.commit, args.target, work, env)
    verify_installed(repo, args.commit, args.target, work, env, ku, suffix)


if __name__ == "__main__":
    main()
