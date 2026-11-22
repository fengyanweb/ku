"""Run real Python package consumers with an installed Ku, outside source."""
import argparse
import json
import os
import re
import shutil
import subprocess
import sys
import tempfile
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


def bridges(files):
    return {
        path.relative_to(files).as_posix(): (path.stat().st_mtime_ns, path.stat().st_size)
        for path in files.rglob(".ku-bridge-identity")
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
        match = re.search(r'^import \{ [^\n]+ \} from "@subtitles/text"$', output, re.M)
        assert match, output
        imports = [match.group()]
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
            run(ku, author, env, "package", "install", "pypi:pysubs2==1.8.0", "--module", "pysubs2")
            unconfigured = run(ku, author, env, "i", "pypi:pillow==11.3.0")
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
        run(ku, consumer, env, "i", "--locked")
        assert (consumer / "ku.lock").read_bytes() == before_lock
        cache = consumer / ".ku" / "cache"
        cold = bridges(cache)
        assert cold, "no compiled interop artifacts"
        run(ku, consumer, env, "i", "--offline")
        run(ku, consumer, env, "i")
        assert bridges(cache) == cold, "warm restoration rebuilt a prepared bridge"
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


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--ku", required=True, type=Path)
    parser.add_argument("--python", default=Path(sys.executable), type=Path)
    parser.add_argument("--wheels", action="store_true", help="also restore public PyPI wheels")
    opts = parser.parse_args()
    acceptance(opts.ku.resolve(strict=True), opts.wheels, opts.python.resolve(strict=True))
