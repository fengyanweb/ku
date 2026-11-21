"""Rebuild an identified standalone Stage1 with the installed Stage0."""
import argparse
from hashlib import sha256
import io
import json
import os
from pathlib import Path, PurePosixPath
import shutil
import subprocess
import sys
import tempfile


def digest(path):
    return sha256(path.read_bytes()).hexdigest()


def stage_source_blobs(repo, source, destination):
    """Use the same raw Git source bytes as the later generation resolver."""
    records = subprocess.check_output(
        ["git", "ls-tree", "-r", "-z", source, "--", "bootstrap", ".github/scripts/verify-native-three-os.py"],
        cwd=repo, timeout=30,
    ).split(b"\0")
    entries = []
    for record in records[:-1]:
        metadata, encoded = record.split(b"\t", 1)
        mode, kind, object_id = metadata.split()
        relative = PurePosixPath(encoded.decode("utf-8"))
        if (kind != b"blob" or mode not in (b"100644", b"100755")
                or relative.is_absolute() or ".." in relative.parts or b"\\" in encoded):
            raise ValueError(f"unexpected bootstrap snapshot entry: {record!r}")
        entries.append((relative, encoded, object_id))
    if records[-1] or not entries:
        raise ValueError("missing committed bootstrap sources")

    blobs = subprocess.run(["git", "cat-file", "--batch"], cwd=repo,
                           input=b"".join(object_id + b"\n" for _, _, object_id in entries),
                           check=True, capture_output=True, timeout=60).stdout
    digest_input = sha256()
    with io.BytesIO(blobs) as data:
        for relative, encoded, object_id in entries:
            header = data.readline().split()
            if len(header) != 3 or header[:2] != [object_id, b"blob"]:
                raise ValueError(f"unexpected bootstrap blob for {relative}: {header!r}")
            length = int(header[2])
            content = data.read(length)
            if len(content) != length or data.read(1) != b"\n":
                raise ValueError(f"truncated bootstrap blob for {relative}")
            target = destination.joinpath(*relative.parts)
            target.parent.mkdir(parents=True, exist_ok=True)
            target.write_bytes(content)
            digest_input.update(encoded + b"\0" + header[2] + b"\0")
            digest_input.update(content)
        if data.read(1):
            raise ValueError("unexpected trailing bootstrap blob data")
    return digest_input.hexdigest()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("repo", type=Path)
    parser.add_argument("revision")
    parser.add_argument("ku", type=Path)
    parser.add_argument("output", type=Path)
    parser.add_argument("--cc", help="target C11 compiler executable (default: clang)")
    parser.add_argument("--gnu-stage1-clang", help="compile only the Windows GNU Stage1 host with this Clang")
    parser.add_argument("--temp-dir", type=Path, default="/private/tmp" if sys.platform == "darwin" else None)
    args = parser.parse_args()
    if args.gnu_stage1_clang and os.name != "nt":
        parser.error("--gnu-stage1-clang requires the x64 Windows GNU host")
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
    host_receipt = output.with_suffix(".gnu-host.json")
    if output.exists() or identity.exists() or (args.gnu_stage1_clang and host_receipt.exists()):
        parser.error("choose a fresh output directory")
    with tempfile.TemporaryDirectory(prefix="ku-long-stage1-sources-", dir=temp_dir) as temporary:
        location = Path(temporary)
        source_snapshot = stage_source_blobs(repo, source, location)
        builder = location / "bootstrap/tools/compiler_driver/build.py"
        environment = dict(os.environ, KU_CC=cc) if args.cc is not None else None
        # The builder owns the per-command deadlines and process-tree
        # cleanup. The outer verification job also bounds the entire chain.
        command = [sys.executable, "-B", builder, "--ku", ku, "--cc", cc, "-o", output]
        if args.gnu_stage1_clang:
            command.extend(["--gnu-stage1-clang", args.gnu_stage1_clang])
        subprocess.run(command, cwd=location, env=environment, check=True)
    result = {"commit": source, "seed": str(ku), "seed_sha256": digest(ku),
              "sha256": digest(output), "source_snapshot_sha256": source_snapshot,
              "c_compiler": cc, "c_target": c_target}
    if args.gnu_stage1_clang:
        result["gnu_host"] = json.loads(host_receipt.read_bytes())
        host_receipt.unlink()
    identity.write_text(json.dumps(result, indent=2) + "\n")
    print(output)


if __name__ == "__main__":
    main()
