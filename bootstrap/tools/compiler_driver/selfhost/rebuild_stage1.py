"""Rebuild an identified standalone Stage1 with the installed Stage0."""
import argparse
from hashlib import sha256
import io
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tarfile
import tempfile


def digest(path):
    return sha256(path.read_bytes()).hexdigest()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("repo", type=Path)
    parser.add_argument("revision")
    parser.add_argument("ku", type=Path)
    parser.add_argument("output", type=Path)
    parser.add_argument("--cc", help="target C11 compiler executable (default: clang)")
    parser.add_argument("--temp-dir", type=Path, default="/private/tmp" if sys.platform == "darwin" else None)
    args = parser.parse_args()
    repo = args.repo.resolve(strict=True)
    temp_dir = args.temp_dir.resolve(strict=True) if args.temp_dir is not None else None
    source = subprocess.check_output(
        ["git", "rev-parse", args.revision + "^{commit}"], cwd=repo, text=True
    ).strip()
    ku = args.ku.resolve(strict=True)
    output = args.output.resolve()
    if os.name == "nt" and output.suffix.lower() != ".exe":
        output = output.with_name(output.name + ".exe")
    cc = shutil.which(args.cc or "clang")
    if cc is None:
        parser.error(f"C compiler not found: {args.cc or 'clang'}")
    cc = str(Path(cc).absolute())
    c_target = subprocess.check_output([cc, "-dumpmachine"], text=True, timeout=20).strip()
    output.parent.mkdir(parents=True, exist_ok=True)
    identity = output.parent / "build-identity.json"
    if output.exists() or identity.exists():
        parser.error("choose a fresh output directory")
    archive = subprocess.check_output(["git", "archive", source, "bootstrap",
                                       ".github/scripts/verify-native-three-os.py"], cwd=repo)
    with tempfile.TemporaryDirectory(prefix="ku-long-stage1-sources-", dir=temp_dir) as temporary:
        location = Path(temporary)
        with tarfile.open(fileobj=io.BytesIO(archive)) as opened:
            for member in opened:
                relative = Path(member.name)
                if relative.is_absolute() or ".." in relative.parts or member.issym() or member.islnk():
                    raise ValueError(f"unexpected snapshot path: {relative}")
                if member.isfile():
                    destination = location / relative
                    destination.parent.mkdir(parents=True, exist_ok=True)
                    destination.write_bytes(opened.extractfile(member).read())
        builder = location / "bootstrap/tools/compiler_driver/build.py"
        environment = dict(os.environ, KU_CC=cc) if args.cc is not None else None
        # The builder owns the per-command deadlines and process-tree
        # cleanup. The outer verification job also bounds the entire chain.
        subprocess.run([sys.executable, "-B", builder, "--ku", ku, "--cc", cc, "-o", output],
                       cwd=location, env=environment, check=True)
    identity.write_text(json.dumps({"commit": source, "seed": str(ku), "seed_sha256": digest(ku),
                                    "sha256": digest(output), "archive_sha256": sha256(archive).hexdigest(),
                                    "c_compiler": cc, "c_target": c_target},
                                   indent=2) + "\n")
    print(output)


if __name__ == "__main__":
    main()
