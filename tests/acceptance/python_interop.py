"""Run real Python package consumers with an installed Ku, outside source."""
import argparse
import hashlib
import json
import os
import re
import shutil
import subprocess
import sys
import tempfile
import urllib.request
from pathlib import Path


def run(program, cwd, env, *args, timeout=100):
    completed = subprocess.run(
        [str(program), *map(str, args)],
        cwd=cwd,
        env=env,
        capture_output=True,
        text=True,
        encoding="utf-8",
        timeout=timeout,
    )
    if completed.returncode:
        raise AssertionError(
            f"{program} {args} -> {completed.returncode}\n{completed.stdout}\n{completed.stderr}"
        )
    return completed.stdout


def write(path, text):
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(text, encoding="utf-8")


def add_adapter(project, name, operation, code, ku_parameter, ku_return, with_argument=False):
    """An author supplies the boundary where library object types are ambiguous."""
    root = project / "vendor" / f"ku-python-{name}" / "src"
    descriptor_path = root / "ku.python.json"
    descriptor = json.loads(descriptor_path.read_text(encoding="utf-8"))
    assert descriptor["artifacts"] and descriptor["distribution"] == name
    print(name, descriptor["version"], [
        (wheel["filename"], wheel["sha256"]) for wheel in descriptor["artifacts"]
    ])
    descriptor["local"] = True
    descriptor["exports"] = [{
        "name": operation, "callable": operation,
        "parameters": [["arg", "str"]] if with_argument else [],
        "returns": ku_return,
    }]
    write(descriptor_path, json.dumps(descriptor, sort_keys=True))
    write(root / "bridge.py", code)
    for existing in root.glob("*.ku"):
        existing.unlink()
    argument = ", arg" if with_argument else ""
    write(root / "text.ku",
          f'fn {operation}({ku_parameter}): {ku_return}! {{\n'
          f'    return __ku_interop_{ku_return}("self", "{operation}"{argument})\n'
          f'}}\n')


def cache_artifacts(files):
    records = [*files.rglob(".ku-bridge-identity"), *files.rglob("*.whl")]
    return {
        path.relative_to(files).as_posix(): (path.stat().st_mtime_ns, path.stat().st_size)
        for path in records
    }


def acceptance(ku, wheels, python):
    env = os.environ.copy()
    env["KU_PYTHON"] = str(python)
    with tempfile.TemporaryDirectory(prefix="ku-python-accept-") as temp:
        sandbox = Path(temp)
        author = sandbox / "author"
        manifest = 'name = "consumer"\nversion = "0.1.0"\nroot = "src"\nmain = "main.ku"\n'
        write(author / "ku.mod", manifest)
        write(author / "src" / "main.ku", 'fn main() { println("hello") }\n')
        write(author / "tools" / "subtitles.py", """calls = 0

def Clean(text: str) -> str:
    global calls
    calls += 1
    return text.strip()

def Count() -> int:
    return calls

def Explode() -> str:
    raise ValueError("故意失败")

def Dynamic(value):
    return {"subtitle": value}
""")
        output = run(ku, author, env, "i", "./tools/subtitles.py", "--module", "text",
                     "--python", python)
        assert "Python file tools/subtitles.py" in output, output
        imports = re.findall(r'^import \{ [A-Za-z]+ \} from "@subtitles/text"$', output, re.M)
        assert {line.split()[2] for line in imports} == {"Clean", "Count", "Dynamic", "Explode"}, output
        main = """    text = "  你好😀  "
    println(Clean(text)?)
    if (text != "  你好😀  ") { fail("Python bridge mutated its borrowed input") }
    try {
        println(Explode()?)
    } catch (err) {
        println(err.code)
    }
    println(Clean(" next ")?)
    println(Count()?)
    println(Dynamic("[\\"雪\\"]")?)
"""
        expected = ['你好😀', 'call_error', 'next', '2', '{"subtitle": "雪"}']
        if wheels:
            configured = run(ku, author, env, "package", "install", "pypi:pysubs2==1.8.0",
                             "--module", "pysubs2.time")
            assert "PyPI pysubs2==1.8.0" in configured, configured
            copied = re.search(r'^import \{ MsToTimes \} from "@pysubs2/time"$', configured, re.M)
            assert copied, configured
            write(author / "src" / "main.ku", copied.group() + '\n\nfn main(): null! {\n'
                  '    println(MsToTimes("[1500]")?)\n    return ok(null)\n}\n')
            assert run(ku, author, env, "run", "src/main.ku") == "[0, 0, 1, 500]\n"
            unconfigured = run(ku, author, env, "i", "pypi:pillow==11.3.0")
            assert "PyPI pillow==11.3.0" in unconfigured, unconfigured
            assert "No callable Ku exports" in unconfigured, unconfigured
            add_adapter(author, "pysubs2", "Shift", """import pysubs2

def Shift(text: str) -> str:
    subs = pysubs2.SSAFile()
    subs.events.append(pysubs2.SSAEvent(start=1000, end=1750, text=text))
    subs.shift(ms=500)
    item = subs.events[0]
    return f"{item.start}:{item.end}:{item.text}"
""", "&arg: str", "str", with_argument=True)
            add_adapter(author, "pillow", "Pixel", """from PIL import Image

def Pixel() -> int:
    with Image.new("RGB", (2, 1), (17, 31, 47)) as image:
        assert image.size == (2, 1)
        return image.getpixel((1, 0))[1]
""", "", "int")
            imports += ['import { Shift } from "@pysubs2/text"', 'import { Pixel } from "@pillow/text"']
            main += '    println(Shift("字幕😀")?)\n    println(Pixel()?)\n'
            expected += ['1500:2250:字幕😀', '31']
        write(author / "src" / "main.ku",
              "\n".join(imports) + "\n\nfn main(): null! {\n" + main + "    return ok(null)\n}\n")
        run(ku, author, env, "i")
        expect = "\n".join(expected) + "\n"

        consumer = sandbox / "consumer"
        shutil.copytree(author, consumer, ignore=shutil.ignore_patterns(".ku"))
        before_lock = (consumer / "ku.lock").read_bytes()
        offline = subprocess.run(
            [str(ku), "i", "--offline"], cwd=consumer, env=env, capture_output=True,
            text=True, encoding="utf-8", timeout=30
        )
        assert offline.returncode and "offline" in offline.stderr.lower(), offline
        assert (consumer / "ku.lock").read_bytes() == before_lock
        assert not list((consumer / ".ku").rglob("*.whl"))
        run(ku, consumer, env, "i", "--locked")
        assert (consumer / "ku.lock").read_bytes() == before_lock
        cache = consumer / ".ku" / "cache"
        cold = cache_artifacts(cache)
        assert any(key.endswith(".ku-bridge-identity") for key in cold), cold
        run(ku, consumer, env, "i", "--offline")
        run(ku, consumer, env, "i")
        assert cache_artifacts(cache) == cold, "warm restoration rebuilt a bridge or downloaded a wheel"
        assert run(ku, consumer, env, "run", "--offline", "src/main.ku") == expect

        for backend, args in [("native", []), ("runner", ["--backend", "runner"])]:
            suffix = ".exe" if os.name == "nt" else ""
            appdir = consumer / backend
            appdir.mkdir()
            executable = appdir / f"consumer{suffix}"
            logs = run(ku, consumer, env, "build", *args, "--offline", "--release",
                       "src/main.ku", "-o", executable, timeout=180)
            assert ("native c ok:" if backend == "native" else "build ok:") in logs, logs
            moved = sandbox / f"shipped-{backend}"
            shutil.copytree(appdir, moved)
            output = run(moved / f"consumer{suffix}", moved, env)
            assert output == expect, (backend, output, expect)
        print(f"Python package interop passed on {sys.platform} ({sys.version.split()[0]})")


def portable_acceptance(ku, python, notices):
    env = os.environ.copy()
    env["KU_PYTHON_REDISTRIBUTION_NOTICES"] = str(notices)
    with tempfile.TemporaryDirectory(prefix="ku-portable-accept-") as temp:
        sandbox = Path(temp)
        if python.parent.name != "bin":
            raise ValueError("Portable test requires a relocatable bin/python installation")
        disposable_python = sandbox / "build-python"
        shutil.copytree(python.parent.parent, disposable_python, symlinks=True)
        env["KU_PYTHON"] = str(disposable_python / "bin" / python.name)
        author = sandbox / "author"
        write(author / "ku.mod", 'name = "portable"\nversion = "0.1.0"\nroot = "src"\nmain = "main.ku"\n')
        write(author / "src" / "main.ku", 'fn main() { println("hello") }\n')
        run(ku, author, env, "i", "pypi:pillow==11.3.0")
        add_adapter(author, "pillow", "Pixel", """from PIL import Image

def Pixel() -> int:
    with Image.new("RGB", (2, 1), (17, 31, 47)) as image:
        return image.getpixel((1, 0))[1]
""", "", "int")
        write(author / "src" / "main.ku", """
import { Pixel } from "@pillow/text"

fn main(): null! {
    println(Pixel()?)
    return ok(null)
}
""")
        run(ku, author, env, "i")
        receivers = []
        for backend, args in [("native", []), ("runner", ["--backend", "runner"])]:
            stage = author / backend
            stage.mkdir()
            run(ku, author, env, "build", *args, "--offline", "--release", "src/main.ku",
                "-o", stage / "portable", timeout=220)
            receiver = sandbox / f"received-{backend}"
            shutil.copytree(stage, receiver)
            runtimes = [path for path in (receiver / "ku-runtime.d").rglob("python") if path.is_dir()]
            assert len(runtimes) == 1 and (runtimes[0] / "notices").is_dir()
            assert list((runtimes[0] / "lib").glob("python*/LICENSE.txt"))
            copied_notices = runtimes[0] / "notices"
            for notice in notices.glob("LICENSE.*.txt"):
                assert (copied_notices / notice.name).read_bytes() == notice.read_bytes()
            receivers.append(receiver)
        disposable_python.rename(sandbox / "unavailable-build-python")
        runtime_env = {"HOME": str(sandbox), "PATH": "/usr/bin:/bin"}
        for receiver in receivers:
            assert run(receiver / "portable", receiver, runtime_env) == "31\n"
        print(f"Bundled Python runtime passed relocation ({sys.platform})")


def download_exact(url, file, size, expected):
    digest = hashlib.sha256()
    copied = 0
    with urllib.request.urlopen(url, timeout=60) as source, file.open("xb") as out:
        for chunk in iter(lambda: source.read(1024 * 1024), b""):
            copied += len(chunk)
            if copied > size:
                raise AssertionError(f"Published archive exceeds its pinned size: {url}")
            digest.update(chunk)
            out.write(chunk)
    if copied != size or digest.hexdigest() != expected:
        raise AssertionError(f"Published archive differs from its pinned identity: {url}")


def official_macos_runtime(ku):
    if sys.platform != "darwin" or os.uname().machine != "arm64":
        raise ValueError("The pinned portable Python acceptance asset is macOS ARM64")
    prefix = ("https://github.com/astral-sh/python-build-standalone/releases/download/"
              "20261110/cpython-3.12.16%2B20261110-aarch64-apple-darwin")
    with tempfile.TemporaryDirectory(prefix="ku-cpython-assets-") as temp:
        temp = Path(temp)
        runtime = temp / "runtime.tar.gz"
        complete = temp / "complete.tar.zst"
        download_exact(prefix + "-install_only.tar.gz", runtime, 25_371_130,
                       "e956e60bc9465d577ef8062059aa05cd3a2665b4b8678a2ed24e432368330b8f")
        download_exact(prefix + "-pgo%2Blto-full.tar.zst", complete, 58_217_880,
                       "4da072af9dbbc2557d3aabf51db553a19694f6ac61b5b7169749e4acfa5a052d")
        subprocess.run(["bsdtar", "-xf", str(runtime), "-C", str(temp)], check=True, timeout=60)
        notices = temp / "notices"
        notices.mkdir()
        subprocess.run(["bsdtar", "-xf", str(complete), "-C", str(notices), "python/licenses"],
                       check=True, timeout=60)
        licenses = notices / "python" / "licenses"
        assert len(list(licenses.glob("LICENSE.*.txt"))) == 20
        portable_acceptance(ku, temp / "python" / "bin" / "python3.12", licenses)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--ku", required=True, type=Path)
    parser.add_argument("--python", default=Path(sys.executable), type=Path)
    parser.add_argument("--wheels", action="store_true", help="also restore public PyPI wheels")
    portable = parser.add_mutually_exclusive_group()
    portable.add_argument("--portable-notices", type=Path, help="test an explicitly redistributable CPython")
    portable.add_argument("--standalone-macos", action="store_true", help="test a pinned public macOS CPython distribution")
    opts = parser.parse_args()
    ku = opts.ku.resolve(strict=True)
    python = opts.python.resolve(strict=True)
    if opts.standalone_macos:
        official_macos_runtime(ku)
    elif opts.portable_notices:
        portable_acceptance(ku, python, opts.portable_notices.resolve(strict=True))
    else:
        acceptance(ku, opts.wheels, python)
