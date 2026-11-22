"""Build Ku and install its same-build CLI and Runner SDK into a new directory."""

import argparse
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile


ARTIFACT_SUFFIXES = {".rlib", ".dll", ".dylib", ".so", ".a", ".lib"}


def execute(args, repository, capture=False, timeout=1200):
    result = subprocess.run(
        [str(arg) for arg in args],
        cwd=repository,
        check=True,
        stdout=subprocess.PIPE if capture else None,
        timeout=timeout,
    )
    return result.stdout if capture else None


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--prefix", required=True, type=Path)
    parser.add_argument("--rust-host", help="use an explicit installed Rust host triple")
    parser.add_argument("--jobs", type=int)
    arguments = parser.parse_args()
    if arguments.jobs is not None and not 1 <= arguments.jobs <= 64:
        parser.error("--jobs must be between 1 and 64")
    repo = Path(__file__).resolve().parent.parent
    prefix = arguments.prefix.absolute()
    if prefix.exists() or prefix.is_symlink():
        parser.error("choose a new --prefix directory; an existing installation is never replaced")
    prefix.parent.mkdir(parents=True, exist_ok=True)
    manifest_tool = repo / ".github" / "scripts" / "runtime-bundle.py"
    version = execute([sys.executable, "-B", manifest_tool, "toolchain", "--repo", repo],
                      repo, capture=True).decode().strip()
    toolchain = version if arguments.rust_host is None else version + "-" + arguments.rust_host
    rustc = execute(["rustc", "+" + toolchain, "-vV"], repo, capture=True)
    fields = dict(line.split(": ", 1) for line in rustc.decode().splitlines() if ": " in line)
    target = fields["host"]
    if fields["release"] != version:
        raise RuntimeError("source installer did not resolve the repository's exact Rust toolchain")
    if arguments.rust_host is not None and target != arguments.rust_host:
        raise RuntimeError("source installer resolved the wrong Rust host")
    commit = execute(["git", "rev-parse", "HEAD"], repo, capture=True).decode().strip()
    exe = "ku.exe" if os.name == "nt" else "ku"
    with tempfile.TemporaryDirectory(prefix=".ki-", dir=prefix.parent) as temporary:
        work = Path(temporary)
        target_dir = work / "target"
        command = ["cargo", "+" + toolchain, "build", "--locked", "--release",
                   "--lib", "--bin", "ku", "--target-dir", target_dir]
        if arguments.jobs is not None:
            command.extend(["--jobs", str(arguments.jobs)])
        execute(command, repo)
        profile = target_dir / "release"
        sdk = work / "sdk"
        dependencies = sdk / "deps"
        dependencies.mkdir(parents=True)
        shutil.copy2(profile / exe, sdk / exe)
        shutil.copy2(profile / "libku.rlib", sdk / "libku.rlib")
        for dependency in sorted((profile / "deps").iterdir()):
            if (dependency.is_file() and not dependency.name.startswith("libku-")
                    and dependency.suffix in ARTIFACT_SUFFIXES):
                shutil.copy2(dependency, dependencies / dependency.name)
        for name in ("LICENSE", "NOTICE"):
            shutil.copy2(repo / name, sdk / name)
        (work / "compiler.txt").write_bytes(rustc)
        observed = execute([profile / exe, "doctor", "--json"], work, capture=True, timeout=30)
        (work / "identity.json").write_bytes(observed)
        ku_version = json.loads(observed)["ku_version"]
        execute(
            [sys.executable, "-B", manifest_tool, "create", "--bundle", sdk,
             "--version", ku_version, "--commit", commit, "--target", target, "--repo", repo,
             "--rustc-version-file", work / "compiler.txt", "--build-identity-file", work / "identity.json"],
            repo,
        )
        status = json.loads(execute([sdk / exe, "doctor", "--backend", "runner", "--json"],
                                    work, capture=True, timeout=30))
        if (not status["ready"] or status["installation_mode"] != "release-bundle"
                or status["runtime_source_commit"] != commit):
            raise RuntimeError("the private SDK did not resolve to its same-build Runner")
        if prefix.exists() or prefix.is_symlink():
            raise FileExistsError(f"another installation created {prefix}")
        sdk.rename(prefix)
    print(f"Ku {ku_version} {commit} ({target}) installed at {prefix / exe}")


if __name__ == "__main__":
    main()
