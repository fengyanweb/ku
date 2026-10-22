#!/usr/bin/env python3
"""Build the frozen four-stack benchmark on its disposable Linux ARM64 guest.

Run as the benchmark user, never as root. Outputs/caches/dependencies belong to
ROOT/build, not the source tree. This prepares measured programs, not a Release.
"""
from __future__ import annotations

import argparse
import fcntl
import hashlib
import json
import os
from pathlib import Path
import platform
import shutil
import subprocess
import tempfile
import time

CANDIDATE = "8210e5200c466d9041807d04a68c1844ba9cbd6f"


def digest(path):
    h = hashlib.sha256()
    with Path(path).open("rb") as stream:
        for block in iter(lambda: stream.read(1048576), b""):
            h.update(block)
    return h.hexdigest()


def write_json(path, value):
    path = Path(path)
    path.parent.mkdir(parents=True, exist_ok=True)
    temporary = path.with_name(path.name + ".writing")
    with temporary.open("w", encoding="utf-8") as stream:
        json.dump(value, stream, ensure_ascii=False, indent=2)
        stream.write("\n")
    os.replace(temporary, path)


def identities(root):
    result = {}
    for base in (root / "src", root / "crates", root / "tests/benchmarks/cross_stack"):
        for path in sorted(base.rglob("*")):
            relative = path.relative_to(root)
            if (path.is_file() and not path.is_symlink() and
                    not {"target", "node_modules", "__pycache__", ".git"}.intersection(relative.parts)):
                result[relative.as_posix()] = digest(path)
    for name in ("Cargo.toml", "Cargo.lock"):
        result[name] = digest(root / name)
    return result


class Builder:
    def __init__(self, root, source, uid=501, gid=1000, peer_port=19191, candidate=CANDIDATE):
        if platform.system() != "Linux" or platform.machine() not in ("aarch64", "arm64"):
            raise RuntimeError("build helper requires the actual ARM64 Linux benchmark guest")
        if os.geteuid() != uid or os.getegid() != gid:
            raise RuntimeError(f"run as the benchmark user uid={uid}, gid={gid}; never root")
        if len(candidate) != 40 or any(c not in "0123456789abcdef" for c in candidate):
            raise ValueError("candidate must be a full lowercase Git SHA")
        self.root, self.source = Path(root).resolve(), Path(source).resolve()
        if self.source != self.root / "src":
            raise ValueError("source must be the isolated ROOT/src checkout")
        self.build, self.peer_port = self.root / "build", peer_port
        self.build.mkdir(parents=True, exist_ok=True)
        self.bin = self.build / "bin"
        self.bin.mkdir(exist_ok=True)
        self.logs = self.build / "logs" / str(time.time_ns())
        self.logs.mkdir(parents=True)
        self.benchmark = self.source / "tests/benchmarks/cross_stack"
        self.go = self.root / "tools/go/bin/go"
        self.node = self.root / "tools/node-v24.18.0-linux-arm64/bin/node"
        self.npm = self.node.with_name("npm")
        self.cargo = self.root / "tools/cargo/bin/cargo"
        self.rustc = self.cargo.with_name("rustc")
        self.environment = os.environ.copy()
        # These overrides would change code generation or execution semantics.
        for name in ("RUSTFLAGS", "CARGO_ENCODED_RUSTFLAGS", "RUSTC_WRAPPER",
                     "RUSTC_WORKSPACE_WRAPPER", "NODE_OPTIONS", "GOFLAGS", "GODEBUG",
                     "GOGC", "GOMEMLIMIT", "KU_NATIVE_TLS_PACK"):
            self.environment.pop(name, None)
        self.environment.update(CARGO_HOME=str(self.root / "tools/cargo"),
                                RUSTUP_HOME=str(self.root / "tools/rustup"),
                                CARGO_BUILD_JOBS="2", CARGO_INCREMENTAL="0",
                                GOTOOLCHAIN="local", GOMAXPROCS="2",
                                GOCACHE=str(self.build / "go-cache"),
                                GOMODCACHE=str(self.root / "tools/gomodcache"),
                                CC="clang-18", KU_CC="clang-18")
        self.environment["PATH"] = os.pathsep.join((str(self.cargo.parent), str(self.go.parent),
                                                     str(self.node.parent), self.environment.get("PATH", "")))
        self.commands, self.versions = [], {}
        self.built = set()
        self.initial_hashes = identities(self.source)
        self.candidate, self.uid, self.gid = candidate, uid, gid
        self.manifest_path = self.root / "manifest.json"
        self.manifest = json.loads(self.manifest_path.read_text()) if self.manifest_path.exists() else {}
        previous = self.manifest.get("candidate_base")
        if previous not in (None, candidate):
            raise RuntimeError("existing manifest belongs to another candidate; use a new benchmark root")
        self.manifest.update(schema="ku-cross-stack-manifest-v1", candidate_base=candidate,
                             source_root=str(self.source), uid=uid, gid=gid,
                             protocol=str(self.benchmark / "protocol.json"), peer_port=peer_port,
                             driver_cpuset="4-6", service_cpu_set="0..cores-1", database_cpu_set="7",
                             cgroup_root="/sys/fs/cgroup/ku-cross-stack",
                             package_version="0.0.18", release_status="unpublished runtime candidate; no tag or release created",
                             service_env=dict(KU_EVENT_LOOPS="{cores}", KU_BLOCKING_THREADS="17",
                                              KU_THREAD_BUDGET="64", KU_TASK_MAX_BYTES="67108864"))
        self.manifest.setdefault("stacks", {})

    def command(self, label, command, cwd=None, timeout=900, environment=None, accepted_exit_codes=(0,)):
        env = dict(self.environment)
        env.update(environment or {})
        log = self.logs / (label + ".log")
        began = time.monotonic()
        row = dict(label=label, command=list(map(str, command)), cwd=str(cwd or self.source),
                   log=str(log), timeout_seconds=timeout)
        self.commands.append(row)
        with log.open("xb") as stream:
            process = subprocess.Popen(row["command"], cwd=cwd or self.source, env=env,
                                       stdin=subprocess.DEVNULL, stdout=stream, stderr=subprocess.STDOUT,
                                       start_new_session=True)
            try:
                deadline = time.monotonic() + timeout
                while process.poll() is None:
                    if time.monotonic() >= deadline or log.stat().st_size > 16777216:
                        import signal
                        os.killpg(process.pid, signal.SIGKILL)
                        process.wait(timeout=5)
                        raise RuntimeError(f"bounded build step exceeded time/output: {label}; {log}")
                    time.sleep(.1)
                row.update(exit_code=process.returncode, seconds=time.monotonic()-began,
                           log_sha256=digest(log))
                if process.returncode not in accepted_exit_codes:
                    raise RuntimeError(f"build step {label} exit {process.returncode}; inspect {log}")
            finally:
                if process.poll() is None:
                    import signal
                    os.killpg(process.pid, signal.SIGKILL)
                    process.wait(timeout=5)
        return log.read_text(encoding="utf-8", errors="replace").strip()

    def version(self, label, command, expected=None):
        value = self.versions.get(label)
        if value is None:
            value = self.command("version-" + label, command, timeout=30)
        if expected is not None and expected not in value:
            raise RuntimeError(f"unexpected {label} version: {value}")
        self.versions[label] = value
        return value

    def ku(self):
        self.version("rustc", [self.rustc, "+1.89.0", "-vV"], "rustc 1.89.0")
        self.version("cargo", [self.cargo, "+1.89.0", "--version"], "cargo 1.89.0")
        self.version("cc", ["clang-18", "--version"], "18.")
        target = self.build / "ku-cargo"
        self.command("ku-cli", [self.cargo, "+1.89.0", "build", "--locked", "--release", "--bin", "ku",
                                "--jobs", "2", "--target-dir", target])
        ku = target / "release/ku"
        self.version("ku", [ku, "--version"], "ku 0.0.18")
        libpq = Path("/usr/lib/aarch64-linux-gnu/libpq.so").resolve(strict=True)
        with libpq.open("rb") as stream:
            magic = stream.read(4)
        if not libpq.is_file() or magic != b"\x7fELF":
            raise RuntimeError("installed ARM64 shared libpq is missing")
        private = self.build / "libpq"
        private.mkdir(exist_ok=True)
        shutil.copy2(libpq, private / "libpq.so")
        work = self.build / "ku-native"
        work.mkdir(exist_ok=True)
        source = self.benchmark / "ku/server.ku"
        text = source.read_text(encoding="utf-8")
        if text.count("__PEER_PORT__") != 1:
            raise RuntimeError("expected exactly one fixed peer substitution in Ku source")
        expanded = work / "server.ku"
        expanded.write_text(text.replace("__PEER_PORT__", str(self.peer_port)), encoding="utf-8")
        batch = work / "batch.ku"
        shutil.copyfile(self.benchmark / "ku/batch.ku", batch)
        outputs = {}
        for name, path in (("server", expanded), ("batch", batch)):
            destination = self.bin / ("ku-" + name)
            emission = self.command("ku-native-" + name, [ku, "build", "--backend", "c", "--profile", "release",
                                                       "--emit-c", "-o", destination, path], cwd=work,
                                    environment={"KU_PG_LIB": str(private)}, accepted_exit_codes=(0, 1))
            # The current CLI emits portable C but intentionally refuses to
            # validate/link an ARM64 Linux release. Keep that official support
            # boundary and compile the emitted file explicitly for this
            # benchmark only. No compiler/runtime source is changed.
            refusal = "native C linking is not validated for this host architecture"
            if self.commands[-1]["exit_code"] != 1 or refusal not in emission:
                raise RuntimeError("expected the exact unsupported Linux ARM64 linking diagnostic; inspect emission log")
            lines = emission.splitlines()
            if len(lines) != 3 or not lines[0].startswith("native c ok: ") or lines[1] != refusal:
                raise RuntimeError("unexpected Ku C emission output; refusing to reuse generated source")
            generated_source = Path(lines[0].removeprefix("native c ok: ")).resolve(strict=True)
            if work.resolve() not in generated_source.parents or generated_source.is_symlink():
                raise RuntimeError("generated C escaped the private build directory")
            compiler = ["clang-18", generated_source, "-std=c11", "-O2", "-pthread"]
            if name == "server":
                compiler.append(private / "libpq.so")
            compiler.extend(["-lm", "-o", destination])
            self.command("ku-manual-c-" + name, compiler, cwd=work, timeout=180)
            elf_header = self.command("ku-elf-header-" + name, ["readelf", "-h", destination], timeout=30)
            if "AArch64" not in elf_header or "ELF64" not in elf_header:
                raise RuntimeError("native benchmark output is not actual ELF64 AArch64")
            dynamic = self.command("ku-elf-dynamic-" + name, ["readelf", "-d", destination], timeout=30)
            outputs[name] = dict(source=str(path), source_sha256=digest(path),
                                 generated_c=str(generated_source), generated_c_sha256=digest(generated_source),
                                 binary=str(destination), binary_sha256=digest(destination),
                                 elf_header=elf_header, dynamic_dependencies=dynamic,
                                 linking="explicit benchmark-only ARM64 Linux C compilation; not an official Ku release target")
        generated = {str(p.relative_to(work)): digest(p) for p in work.rglob("*.c")}
        if not generated:
            raise RuntimeError("Ku native build did not retain generated C identity")
        self.manifest["stacks"]["ku"] = dict(command=[str(self.bin / "ku-server")],
                                               batch_command=[str(self.bin / "ku-batch")], cwd="{workdir}",
                                               runtime_layout=dict(binary_dir="bin", fs_base_dir="ku-native"),
                                               env={}, build=dict(cli=str(ku), cli_sha256=digest(ku),
                                                                  native=outputs, generated_c=generated,
                                                                  libpq_source=str(libpq), libpq_sha256=digest(libpq),
                                                                  libpq_private_sha256=digest(private / "libpq.so"),
                                                                  cc="clang-18", native_flags="C11 -O2 -pthread -lm; server links installed libpq.so",
                                                                  official_release_target=False))
        self.built.add("ku")

    def go_build(self, client=False):
        self.version("go", [self.go, "version"], "go1.27.2")
        name = "client" if client else "go"
        directory = self.benchmark / name
        binary = self.bin / ("cross-stack-client" if client else "go-server")
        self.command("go-" + name, [self.go, "build", "-trimpath", "-mod=readonly", "-o", binary, "."], cwd=directory)
        if client:
            self.command("client-contracts", [self.go, "test", "./..."], cwd=directory, timeout=180)
            self.manifest["client"] = dict(command=[str(binary)], cwd=str(self.root), env=dict(GOMAXPROCS="3"),
                                           sha256=digest(binary), source_sha256=digest(directory / "main.go"))
            self.built.add("client")
        else:
            self.manifest["stacks"]["go"] = dict(command=[str(binary)], batch_command=[str(binary), "--batch"],
                                                   cwd="{workdir}", env=dict(GOMAXPROCS="{cores}"),
                                                   build=dict(binary_sha256=digest(binary), flags="-trimpath -mod=readonly; normal GC"))
            self.built.add("go")

    def rust(self):
        self.version("rustc", [self.rustc, "+1.89.0", "-vV"], "rustc 1.89.0")
        directory = self.benchmark / "rust"
        target = self.build / "rust-cargo"
        self.command("rust-server", [self.cargo, "+1.89.0", "build", "--locked", "--release", "--jobs", "2",
                                     "--target-dir", target], cwd=directory)
        binary = target / "release/ku-cross-stack-rust"
        self.manifest["stacks"]["rust"] = dict(command=[str(binary)], batch_command=[str(binary), "--batch", "{batch_input}"],
                                                 cwd="{workdir}", env={}, build=dict(binary_sha256=digest(binary),
                                                                                  flags="cargo +1.89.0 build --locked --release -j2"))
        self.built.add("rust")

    def node_build(self):
        self.version("node", [self.node, "--version"], "v24.18.0")
        self.version("npm", [self.npm, "--version"])
        directory = self.build / "node-deps"
        directory.mkdir(exist_ok=True)
        for name in ("package.json", "package-lock.json"):
            shutil.copyfile(self.benchmark / "node" / name, directory / name)
        self.command("node-npm-ci", [self.npm, "ci", "--ignore-scripts", "--no-audit", "--no-fund"], cwd=directory, timeout=300)
        for name in ("server.cjs", "compute.cjs"):
            self.command("node-check-" + name, [self.node, "--check", self.benchmark / "node" / name], timeout=30)
        self.manifest["stacks"]["node"] = dict(command=[str(self.node), str(self.benchmark / "node/server.cjs")],
                                                 batch_command=[str(self.node), str(self.benchmark / "node/server.cjs"), "--batch", "{batch_input}"],
                                                 cwd="{workdir}", env=dict(NODE_PATH=str(directory / "node_modules")),
                                                 build=dict(node_binary_sha256=digest(self.node),
                                                            npm_lock_sha256=digest(directory / "package-lock.json"),
                                                            dependency_directory=str(directory / "node_modules"),
                                                            flags="normal node:http/cluster; normal GC; no NODE_OPTIONS"))
        self.built.add("node")

    def finish(self, failed=None):
        current = identities(self.source)
        if current != self.initial_hashes:
            failed = failed or "source or lock files changed during build; results invalid"
        lockfiles = {}
        for path in (self.source / "Cargo.lock", self.benchmark / "go/go.sum", self.benchmark / "go/go.mod",
                     self.benchmark / "rust/Cargo.lock", self.benchmark / "node/package-lock.json",
                     self.benchmark / "client/go.mod"):
            lockfiles[str(path.relative_to(self.source))] = digest(path)
        receipt = dict(schema="ku-cross-stack-build-v1", candidate_base=self.candidate,
                       package_version="0.0.18", source_files=self.initial_hashes, lockfiles=lockfiles,
                       source_inventory_sha256=hashlib.sha256(json.dumps(self.initial_hashes, sort_keys=True).encode()).hexdigest(),
                       exact_versions=self.versions, commands=self.commands, failure=failed,
                       compiled_jobs=2, uname=list(os.uname()), published=False)
        write_json(self.logs / "build.json", receipt)
        if failed:
            raise RuntimeError(f"build preparation failed: {failed}; receipt={self.logs/'build.json'}")
        # The root-only DB preparer may complete while compilation runs. Merge
        # the shared manifest under a lock instead of dropping its new domain.
        with (self.root / ".manifest.lock").open("a") as lock:
            fcntl.flock(lock, fcntl.LOCK_EX)
            latest = json.loads(self.manifest_path.read_text()) if self.manifest_path.exists() else {}
            merged = dict(latest)
            for key, value in self.manifest.items():
                if key not in ("stacks", "client", "db_url", "db_cgroup", "database", "versions"):
                    merged[key] = value
            merged["stacks"] = dict(latest.get("stacks", {}))
            for name in self.built - {"client"}:
                merged["stacks"][name] = self.manifest["stacks"][name]
            if "client" in self.built:
                merged["client"] = self.manifest["client"]
            merged.update(build_receipt=str(self.logs / "build.json"),
                          versions=dict(latest.get("versions", {}), **self.versions),
                          source_inventory_sha256=receipt["source_inventory_sha256"], lockfiles=lockfiles)
            required = {"ku", "go", "rust", "node"}
            merged["build_complete"] = required <= merged["stacks"].keys() and "client" in merged
            write_json(self.manifest_path, merged)
            self.manifest = merged
        return dict(manifest=str(self.manifest_path), build_complete=self.manifest["build_complete"], receipt=str(self.logs/"build.json"))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", type=Path, default=Path("/opt/ku-bench"))
    parser.add_argument("--source", type=Path)
    parser.add_argument("--uid", type=int, default=501)
    parser.add_argument("--gid", type=int, default=1000)
    parser.add_argument("--peer-port", type=int, default=19191)
    parser.add_argument("--candidate", default=CANDIDATE)
    parser.add_argument("--only", nargs="+", choices=("ku", "go", "rust", "node", "client"),
                        default=["ku", "go", "rust", "node", "client"])
    args = parser.parse_args()
    builder = Builder(args.root, args.source or args.root/"src", args.uid, args.gid, args.peer_port, args.candidate)
    try:
        for item in args.only:
            print(f"building {item}", flush=True)
            {"ku": builder.ku, "go": builder.go_build, "rust": builder.rust,
             "node": builder.node_build, "client": lambda: builder.go_build(True)}[item]()
    except Exception as error:
        builder.finish(str(error))
        raise
    print(json.dumps(builder.finish()))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
