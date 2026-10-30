#!/usr/bin/env python3
"""Build the frozen seven-stack benchmark on its disposable Linux ARM64 guest.

Run as the benchmark user, never as root. Outputs/caches/dependencies belong to
ROOT/build, not the source tree. This prepares measured programs, not a Release.
"""
from __future__ import annotations

import argparse
from contextlib import contextmanager
import fcntl
import hashlib
import json
import os
from pathlib import Path
import platform
import re
import shutil
import subprocess
import sys
import tempfile
import time
import tomllib

from compile_variants import ku_cc_command

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


def file_identity(path):
    path = Path(path).absolute()
    real = path.resolve(strict=True)
    if not real.is_file():
        raise RuntimeError("compiler input is not a regular file: " + str(path))
    return dict(path=str(path), real_path=str(real), size=real.stat().st_size,
                sha256=digest(real))


def normal_compiler_receipt(config, arguments, cwd):
    """Inspect the real CLI invocation before its staged library disappears.

    This is deliberately limited to the frozen native ARM64 benchmark: no
    override flags, response files, additional object files or libraries.
    The CLI owns -o staging/installation; it need not equal the final output.
    """
    compiler = file_identity(config["compiler"]["path"])
    if compiler != config["compiler"]:
        raise RuntimeError("recorded native compiler identity changed")
    row = dict(schema="ku-normal-compiler-invocation-v1", compiler=compiler,
               argv=[compiler["path"], *arguments], cwd=str(Path(cwd).resolve()),
               helper_sha256=config["helper_sha256"])
    if arguments in (["-dumpmachine"], ["--version"]):
        return dict(row, kind="probe")
    sources, libraries, flags, output = [], [], [], None
    index = 0
    while index < len(arguments):
        argument = arguments[index]
        if argument == "-o":
            index += 1
            if output is not None or index == len(arguments):
                raise RuntimeError("expected one actual native compiler -o")
            output = str((Path(cwd) / arguments[index]).resolve())
        elif argument in ("-std=c11", "-O2", "-pthread", "-lm"):
            if argument in flags:
                raise RuntimeError("duplicate normal native compiler flag: " + argument)
            flags.append(argument)
        elif argument.startswith("-"):
            raise RuntimeError("unknown normal native compiler flag/input: " + argument)
        else:
            if not (argument.endswith(".c") or re.search(r"\.so(?:\.[0-9.]+)?$", argument)):
                raise RuntimeError("unknown native compiler input: " + argument)
            item = file_identity(Path(cwd) / argument)
            if argument.endswith(".c"):
                if Path(config["work"]).resolve() not in Path(item["real_path"]).parents:
                    raise RuntimeError("native compiler source escaped the private build directory")
                sources.append(item)
            elif re.search(r"\.so(?:\.[0-9.]+)?$", argument):
                expected = config["library"]
                if expected is None or item["sha256"] != expected["sha256"]:
                    raise RuntimeError("unknown native library input: " + argument)
                libraries.append(dict(item, role="libpq", selected_input=expected))
        index += 1
    if (len(sources) != 1 or output is None or "-std=c11" not in flags or "-O2" not in flags
            or len(libraries) != (1 if config["library"] else 0)):
        raise RuntimeError("native compiler invocation is not the exact normal O2 contract")
    return dict(row, kind="compile", source=sources[0], output=output,
                flags=flags, library_inputs=libraries)


def exec_recorded_compiler(config_path, arguments):
    config = json.loads(Path(config_path).read_text(encoding="utf-8"))
    if digest(__file__) != config["helper_sha256"]:
        raise RuntimeError("compiler receipt helper changed during the build")
    row = normal_compiler_receipt(config, arguments, Path.cwd())
    receipt = Path(config["receipt"])
    if row["kind"] == "probe":
        receipt = receipt.with_name("probe-" + arguments[0].lstrip("-") + ".json")
    # Explicit compiler selection has no fallback: a second compile is an
    # unexpected invocation, not permission to overwrite its first evidence.
    with receipt.open("x", encoding="utf-8") as stream:
        json.dump(row, stream, ensure_ascii=False, indent=2)
        stream.write("\n")
    os.execv(config["compiler"]["path"], row["argv"])


@contextmanager
def normal_compiler_recorder(directory, compiler, work, library=None):
    """Rebuild a disposable KU_CC recorder; keep only its receipt and recipe."""
    directory = Path(directory)
    directory.mkdir(parents=True, exist_ok=False)
    helper = Path(__file__).resolve()
    config = dict(compiler=file_identity(compiler), work=str(Path(work).resolve()),
                  library=file_identity(library) if library is not None else None,
                  helper_sha256=digest(helper), receipt=str(directory / "compiler.json"))
    with tempfile.TemporaryDirectory(prefix="ku-cc-", dir=directory.parent) as temporary:
        wrapper, settings = Path(temporary) / "record.py", Path(temporary) / "config.json"
        wrapper.write_text("import sys\nsys.dont_write_bytecode = True\n"
                           + "sys.path.insert(0, " + repr(str(helper.parent)) + ")\n"
                           + "from build_linux import exec_recorded_compiler\n"
                           + "exec_recorded_compiler(sys.argv[1], sys.argv[2:])\n", encoding="utf-8")
        write_json(settings, config)
        command = [sys.executable, "-B", str(wrapper), str(settings)]
        yield dict(command=command, ku_cc=ku_cc_command(command),
                   receipt=config["receipt"], recipe=dict(
                       function="build_linux.normal_compiler_recorder", helper=str(helper),
                       helper_sha256=config["helper_sha256"], wrapper_sha256=digest(wrapper),
                       compiler=config["compiler"], library=config["library"]))


def native_emission_source(emission, destination, work):
    # The recorder is silent: ordinary CLI stdout remains exactly two lines.
    lines = emission.splitlines()
    if len(lines) != 2 or not lines[0].startswith("native c ok: ") or lines[1] != "build ok: " + str(destination):
        raise RuntimeError("unexpected Ku C emission output; refusing to reuse generated source")
    generated = Path(lines[0].removeprefix("native c ok: "))
    if generated.is_symlink() or Path(work).resolve() not in generated.resolve(strict=True).parents:
        raise RuntimeError("generated C escaped the private build directory")
    return generated.resolve(strict=True)


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


def benchmark_stack_sources(directory):
    return {path.relative_to(directory).as_posix(): digest(path)
            for path in sorted(directory.rglob("*"))
            if path.is_file() and not path.is_symlink()
            and not {"target", "node_modules", "__pycache__", ".git"}.intersection(path.relative_to(directory).parts)}


def expand_ku_server(text, peer_port, mode="helpers", json_mode="text", pg_read="int", pg_execution="async"):
    if mode not in ("helpers", "direct"):
        raise ValueError("unknown Ku formatting mode")
    if json_mode not in ("text", "response", "typed"):
        raise ValueError("unknown Ku JSON response mode")
    if pg_read not in ("value", "int", "typed"):
        raise ValueError("unknown Ku PG result read mode")
    if pg_execution not in ("async", "sync"):
        raise ValueError("unknown Ku PG execution mode")
    if pg_execution == "async" and pg_read == "value":
        raise ValueError("async PG requires typed integer reads; legacy dynamic JSON reads are sync-only")
    if pg_read == "typed" and pg_execution != "async":
        raise ValueError("binary typed PG comparison requires the public async capture path")
    if text.count("__PEER_PORT__") != 1:
        raise RuntimeError("expected exactly one fixed peer substitution in Ku source")
    original = "    return (await work_response(req, total))"
    if text.count(original) != 1:
        raise RuntimeError("expected exactly one original helper formatting return")
    text = text.replace("__PEER_PORT__", str(peer_port))
    if mode == "direct":
        text = text.replace(original, '''    text = str(total)
    while (text.len() < 8) { text = "0" + text }
    return ok(http.text(req.query.rid + ":" + text))''')
    if json_mode == "response":
        original_json = '''    body = json.encode({id: req.query.rid, counter: counter + 1, data: data})?
    return ok(http.text(body))'''
        if text.count(original_json) != 1:
            raise RuntimeError("expected exactly one original JSON handler encode/response")
        text = text.replace(original_json, "    return ok(http.json({id: req.query.rid, counter: counter + 1, data: data}))")
    if json_mode == "typed":
        original_json = "    body = json.encode({id: req.query.rid, counter: counter + 1, data: data})?"
        original_handler = "fn encode(req: HttpRequest): HttpResponse! {"
        if text.count(original_json) != 1 or text.count(original_handler) != 1:
            raise RuntimeError("expected exactly one generic JSON envelope writer")
        text = text.replace(original_handler,
            "struct Envelope { id: str counter: int data: KuValue }\n\n" + original_handler)
        text = text.replace(original_json,
            "    envelope = Envelope {id: req.query.rid, counter: counter + 1, data: data}\n"
            "    body = json.encode(envelope)?")
    original_pg = "    async fn database(req: HttpRequest): HttpResponse! {"
    if text.count(original_pg) != 1:
        raise RuntimeError("expected exactly one public async PG handler")
    original_pg_end = '        return ok(http.text(status, body))\n    }\n    app.get("/db", database)'
    if text.count(original_pg_end) != 1:
        raise RuntimeError("expected exactly one public async PG response/registration")
    if pg_execution == "sync":
        text = text.replace(original_pg, '    app.get("/db", fn(req) {')
        text = text.replace(original_pg_end, '        return http.text(status, body)\n    })')
    if pg_read == "typed":
        original_query = ('            result = client.query("SELECT id,label,score FROM cross_stack_items WHERE id=$1",\n'
                          '                [req.query.key])?')
        if text.count(original_query) != 1:
            raise RuntimeError("expected exactly one original parameterized PG query")
        text = text.replace(original_query,
            '            result = client.query_one_typed("SELECT id,label,score FROM cross_stack_items WHERE id=$1::int4",\n'
            '                [req.query.key.clone()], ["text"], ["int4", "text", "int4"])?')
    for original_read, replacement in (
        ("            key = result.value_int(0, 0)?",
         "            key_value = json.decode(result.value(0, 0)?)?\n            key = key_value.as_int()?"),
        ("            score = result.value_int(0, 2)?",
         "            score_value = json.decode(result.value(0, 2)?)?\n            score = score_value.as_int()?"),
    ):
        if text.count(original_read) != 1:
            raise RuntimeError("expected exactly one typed PG numeric result read")
        if pg_read == "value":
            text = text.replace(original_read, replacement)
    return text


class Builder:
    def __init__(self, root, source, uid=501, gid=1000, peer_port=19191, candidate=CANDIDATE, ku_format="helpers", ku_json="text", ku_pg_read="int", ku_pg_execution="async"):
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
        self.package_version = tomllib.loads((self.source / "Cargo.toml").read_text(encoding="utf-8-sig"))["package"]["version"]
        self.candidate, self.uid, self.gid = candidate, uid, gid
        self.ku_format, self.ku_json, self.ku_pg_read = ku_format, ku_json, ku_pg_read
        self.ku_pg_execution = ku_pg_execution
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
                             package_version=self.package_version, release_status="benchmark build; release publication is verified separately",
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
        self.version("ku", [ku, "--version"], "ku " + self.package_version)
        self.ku_native(ku)

    def ku_native(self, ku):
        """Use an identified existing CLI for the ordinary native Release path.

        Modes expand this Builder's own immutable source, not another version's
        benchmark program. Reusing the CLI does not claim it was rebuilt here.
        """
        ku = Path(ku).resolve(strict=True)
        self.version("ku", [ku, "--version"], "ku " + self.package_version)
        compiler = shutil.which("clang-18", path=self.environment["PATH"])
        if compiler is None:
            raise RuntimeError("normal native compiler clang-18 is missing")
        self.version("cc", [compiler, "--version"], "18.")
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
        expanded = work / "server.ku"
        expanded.write_text(expand_ku_server(text, self.peer_port, self.ku_format, self.ku_json, self.ku_pg_read, self.ku_pg_execution), encoding="utf-8")
        batch = work / "batch.ku"
        shutil.copyfile(self.benchmark / "ku/batch.ku", batch)
        outputs = {}
        for name, path in (("server", expanded), ("batch", batch)):
            destination = self.bin / ("ku-" + name)
            with normal_compiler_recorder(self.logs / ("native-compiler-" + name), compiler, work,
                                          private / "libpq.so" if name == "server" else None) as recorder:
                emission = self.command("ku-native-" + name, [ku, "build", "--backend", "c", "--profile", "release",
                                                           "--emit-c", "-o", destination, path], cwd=work,
                                        environment={"KU_PG_LIB": str(private), "KU_CC": recorder["ku_cc"]})
            # Measure the ordinary CLI's Release output. ARM64 Linux linking
            # is now supported; a historical refusal is not a successful build.
            generated_source = native_emission_source(emission, destination, work)
            invocation = json.loads(Path(recorder["receipt"]).read_text(encoding="utf-8"))
            if (invocation["kind"] != "compile" or invocation["source"]["real_path"] != str(generated_source)
                    or invocation["source"]["sha256"] != digest(generated_source)):
                raise RuntimeError("actual compiler input does not match the CLI's retained generated C")
            elf_header = self.command("ku-elf-header-" + name, ["readelf", "-h", destination], timeout=30)
            if "AArch64" not in elf_header or "ELF64" not in elf_header:
                raise RuntimeError("native benchmark output is not actual ELF64 AArch64")
            dynamic = self.command("ku-elf-dynamic-" + name, ["readelf", "-d", destination], timeout=30)
            outputs[name] = dict(source=str(path), source_sha256=digest(path),
                                 generated_c=str(generated_source), generated_c_sha256=digest(generated_source),
                                 binary=str(destination), binary_sha256=digest(destination),
                                 compiler_invocation=invocation,
                                 compiler_receipt=recorder["receipt"],
                                 compiler_receipt_sha256=digest(recorder["receipt"]),
                                 compiler_recorder=recorder["recipe"],
                                 elf_header=elf_header, dynamic_dependencies=dynamic,
                                 linking="ordinary Ku CLI native Release build on ARM64 Linux; no separate official archive in this release")
        generated = {str(p.relative_to(work)): digest(p) for p in work.rglob("*.c")}
        if not generated:
            raise RuntimeError("Ku native build did not retain generated C identity")
        self.manifest["stacks"]["ku"] = dict(command=[str(self.bin / "ku-server")],
                                               batch_command=[str(self.bin / "ku-batch")], cwd="{workdir}",
                                               runtime_layout=dict(binary_dir="bin", fs_base_dir="ku-native"),
                                               env={}, build=dict(cli=str(ku), cli_sha256=digest(ku),
                                                                  formatting_mode=self.ku_format,
                                                                  json_response_mode=self.ku_json,
                                                                  pg_execution_mode=self.ku_pg_execution,
                                                                  pg_execution_semantics="async uses public local named handler capturing the original shared pool; query/reset suspend on the existing Task reactor; sync is the separately identified bounded HTTP worker baseline; no SQL retries",
                                                                  pg_read_mode=self.ku_pg_read,
                                                                  pg_read_semantics="value keeps text plus JSON numeric decode; int keeps text-protocol result.value_int; typed uses exact-one binary int4/text/int4, TEXT key and explicit $1::int4 SQL through public Task path; label, same-lease DISCARD, no retry and JSON/text response retained",
                                                                  json_response_semantics="text keeps generic encode+http.text; response uses public http.json; typed keeps full DOM decode/counter/take, then static Envelope writer+http.text without output DOM; database route uses http.text",
                                                                  original_server_source_sha256=digest(source),
                                                                  expanded_server_source_sha256=digest(expanded),
                                                                  formatting_semantics="helpers preserves original three formatting tasks; direct uses public str(int), same explicit eight-digit zero padding; user Task semantics unchanged",
                                                                  native=outputs, generated_c=generated,
                                                                  libpq_source=str(libpq), libpq_sha256=digest(libpq),
                                                                  libpq_private_sha256=digest(private / "libpq.so"),
                                                                  cc=file_identity(compiler),
                                                                  native_flags={name: item["compiler_invocation"]["flags"] for name, item in outputs.items()},
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
            control = self.bin / "control-server"
            self.command("go-control", [self.go, "build", "-trimpath", "-mod=readonly", "-o", control, "./control"], cwd=directory)
            self.manifest["stacks"]["control"] = dict(command=[str(control), "--address", "{address}"], cwd="{workdir}", env=dict(GOMAXPROCS="{cores}"),
                build=dict(binary_sha256=digest(control), flags="normal net/http control; not a seven-stack ranked implementation"))
            self.built.add("control")
        else:
            self.manifest["stacks"]["go"] = dict(command=[str(binary)], batch_command=[str(binary), "--batch"],
                                                   cwd="{workdir}", env=dict(GOMAXPROCS="{cores}"),
                                                   build=dict(binary_sha256=digest(binary), flags="-trimpath -mod=readonly; normal GC",
                                                              source_files_sha256=benchmark_stack_sources(directory),
                                                              pg_modes=["normal", "reset-equivalent"],
                                                                                  pg_apis=["normal", "typed-text-key"],
                                                                                  json_modes=["generic", "typed"]))
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
                                                                                  flags="cargo +1.89.0 build --locked --release -j2",
                                                                                  source_files_sha256=benchmark_stack_sources(directory),
                                                                                  pg_modes=["normal", "reset-equivalent"]))
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

    def java_build(self):
        java = self.root / "tools/jdk/bin/java"
        if not java.exists():
            java = Path("/usr/lib/jvm/java-25-openjdk-arm64/bin/java")
        self.version("java", [java, "--version"], "25.0.4.1")
        self.version("maven", ["mvn", "--version"])
        work = self.build / "java"
        work.mkdir(exist_ok=True)
        shutil.copyfile(self.benchmark / "java/pom.xml", work / "pom.xml")
        shutil.copytree(self.benchmark / "java/src", work / "src", dirs_exist_ok=True)
        log = self.command("java-compile", ["mvn", "--batch-mode", "--no-transfer-progress", "-X", "-Dmaven.repo.local=" + str(self.root / "tools/maven"), "compile"], cwd=work,
                     environment={"JAVA_HOME": str(java.parent.parent)}, timeout=600)
        # compiler-plugin resolves the actual Maven dependency closure already;
        # avoid bootstrapping a report plugin merely to copy these same JARs.
        matches = re.findall(r"\(f\) compilePath = \[(.*?)\]", log)
        if len(matches) != 1:
            raise RuntimeError("fixed compiler plugin did not expose exactly one resolved classpath")
        jars = [Path(p.strip()) for p in matches[0].split(",") if p.strip().endswith(".jar")]
        if not jars or len({p.name for p in jars}) != len(jars):
            raise RuntimeError("empty or ambiguous Java runtime dependency names")
        dependencies = work / "target/dependency"
        dependencies.mkdir(exist_ok=True)
        for stale in dependencies.glob("*.jar"):
            stale.unlink()
        for artifact in jars:
            shutil.copyfile(artifact, dependencies / artifact.name)
        artifacts = {p.name: digest(p) for p in sorted(dependencies.glob("*.jar"))}
        expected = json.loads((self.benchmark / "java/dependencies.lock.json").read_text())
        if artifacts != expected["artifacts_sha256"]:
            raise RuntimeError("Java resolved artifacts differ from frozen dependency lock")
        classpath = str(work / "target/classes") + ":" + str(work / "target/dependency/*")
        command = [str(java), "-XX:ActiveProcessorCount={cores}", "-XX:MaxRAMPercentage=65", "-XX:InitialRAMPercentage=5", "-cp", classpath, "ku.bench.Main"]
        self.manifest["stacks"]["java"] = dict(command=command, batch_command=command + ["--batch", "{batch_input}"], cwd="{workdir}", env={},
            build=dict(java_binary_sha256=digest(java), artifact_sha256=artifacts, class_files_sha256={str(p.relative_to(work)): digest(p) for p in (work / "target/classes").rglob("*.class")},
                flags="Java25 normal tiered JIT/G1; heap max65% whole cgroup limit; service tree separately constrained", pool_total=16))
        self.built.add("java")

    def dotnet_build(self):
        dotnet = self.root / "tools/dotnet/dotnet"
        self.version("dotnet", [dotnet, "--info"], "10.0.402")
        work = self.build / "dotnet"
        work.mkdir(exist_ok=True)
        for name in ("global.json", "Ku.CrossStack.csproj", "Program.cs", "packages.lock.json"):
            shutil.copyfile(self.benchmark / "dotnet" / name, work / name)
        env = {"DOTNET_ROOT": str(dotnet.parent), "DOTNET_CLI_TELEMETRY_OPTOUT": "1", "DOTNET_SKIP_FIRST_TIME_EXPERIENCE": "1", "NUGET_PACKAGES": str(self.root / "tools/nuget")}
        self.command("dotnet-restore", [dotnet, "restore", "Ku.CrossStack.csproj", "--locked-mode"], cwd=work, environment=env, timeout=300)
        self.command("dotnet-publish", [dotnet, "publish", "Ku.CrossStack.csproj", "-c", "Release", "--no-restore", "--no-self-contained", "-o", work / "publish"], cwd=work, environment=env, timeout=300)
        command = [str(dotnet), str(work / "publish/Ku.CrossStack.dll")]
        self.manifest["stacks"]["dotnet"] = dict(command=command, batch_command=command + ["--batch", "{batch_input}"], cwd="{workdir}",
            env=dict(DOTNET_ROOT=str(dotnet.parent), DOTNET_PROCESSOR_COUNT="{cores}", DOTNET_GCHeapHardLimitPercent="41", DOTNET_CLI_TELEMETRY_OPTOUT="1"),
            build=dict(sdk_binary_sha256=digest(dotnet), publish_files_sha256={p.name: digest(p) for p in (work / "publish").iterdir() if p.is_file()},
                flags="Release normal tiered JIT/server GC; GC heap max65% (hex41), not total process cap; service cgroup constrained", pool_total=16))
        self.built.add("dotnet")

    def cpp_build(self):
        self.version("cc", ["clang-18", "--version"], "18.")
        self.version("cmake", ["cmake", "--version"])
        dependency_lock = json.loads((self.benchmark / "cpp/dependencies.lock.json").read_text())
        receipt = json.loads((self.root / "tools/drogon-build.json").read_text())
        self.version("jsoncpp", ["pkg-config", "--modversion", "jsoncpp"], dependency_lock["system_json"]["version"])
        self.version("jsoncpp-package", ["dpkg-query", "-W", "-f=${Package}=${Version}", "libjsoncpp-dev"], dependency_lock["system_json"]["ubuntu_package"])
        if receipt["sources"] != dependency_lock["sources"]:
            raise RuntimeError("Drogon/Trantor source identities differ from frozen lock")
        if "-O3 -DNDEBUG" not in receipt.get("flags", ""):
            raise RuntimeError("normal C++ requires full framework Release O3; retain old O2 proof and prepare a new O3 prefix before using this build")
        target = self.build / "cpp"
        self.command("cpp-configure", ["cmake", "-S", self.benchmark / "cpp", "-B", target,
            "-DCMAKE_BUILD_TYPE=Release", "-DCMAKE_CXX_COMPILER=clang++-18", "-DCMAKE_CXX_FLAGS_RELEASE=-O3 -DNDEBUG", "-DCMAKE_PREFIX_PATH=" + str(self.root / "tools/drogon")], timeout=120)
        self.command("cpp-build", ["cmake", "--build", target, "-j2"], timeout=300)
        binary = target / "cpp-server"
        self.manifest["stacks"]["cpp"] = dict(command=[str(binary)], batch_command=[str(binary), "--batch", "{batch_input}"], cwd="{workdir}",
            env=dict(LD_LIBRARY_PATH=str(self.root / "tools/drogon/lib")),
            build=dict(binary_sha256=digest(binary), dependencies=receipt, installed_library_sha256={str(p.relative_to(self.root / "tools/drogon")): digest(p) for p in (self.root / "tools/drogon/lib").rglob("*") if p.is_file()}, flags="clang18 C++17 -O3 -DNDEBUG; Drogon normal async HTTP/PG, JsonCpp dynamic tree", pool_total=16))
        self.built.add("cpp")

    def finish(self, failed=None):
        current = identities(self.source)
        if current != self.initial_hashes:
            failed = failed or "source or lock files changed during build; results invalid"
        lockfiles = {}
        for path in (self.source / "Cargo.lock", self.benchmark / "go/go.sum", self.benchmark / "go/go.mod",
                     self.benchmark / "rust/Cargo.lock", self.benchmark / "node/package-lock.json",
                     self.benchmark / "client/go.mod", self.benchmark / "java/dependencies.lock.json",
                     self.benchmark / "dotnet/packages.lock.json", self.benchmark / "cpp/dependencies.lock.json"):
            lockfiles[str(path.relative_to(self.source))] = digest(path)
        receipt = dict(schema="ku-cross-stack-build-v1", candidate_base=self.candidate,
                       package_version=self.package_version, source_files=self.initial_hashes, lockfiles=lockfiles,
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
            required = {"ku", "go", "rust", "node", "java", "dotnet", "cpp"}
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
    parser.add_argument("--ku-format", choices=("helpers", "direct"), default="helpers",
                        help="helpers preserves legacy benchmark; direct requires a candidate CLI with public str(int), records separate source identity")
    parser.add_argument("--ku-json", choices=("text", "response", "typed"), default="text",
                        help="text keeps generic encode+http.text; response uses http.json; typed uses public static Envelope writer+http.text; separately identified")
    parser.add_argument("--ku-pg-execution", choices=("async", "sync"), default="async",
                        help="async uses the public captured-pool Task path; sync preserves the bounded-worker baseline, separately identified")
    parser.add_argument("--ku-pg-read", choices=("value", "int", "typed"), default="int",
                        help="int keeps text result.value_int; value keeps legacy JSON numeric reads (sync only); typed uses binary exact-one with TEXT key (async only)")
    parser.add_argument("--only", nargs="+", choices=("ku", "go", "rust", "node", "java", "dotnet", "cpp", "client"),
                        default=["ku", "go", "rust", "node", "java", "dotnet", "cpp", "client"])
    args = parser.parse_args()
    builder = Builder(args.root, args.source or args.root/"src", args.uid, args.gid, args.peer_port, args.candidate, args.ku_format, args.ku_json, args.ku_pg_read, args.ku_pg_execution)
    try:
        for item in args.only:
            print(f"building {item}", flush=True)
            {"ku": builder.ku, "go": builder.go_build, "rust": builder.rust,
             "node": builder.node_build, "java": builder.java_build, "dotnet": builder.dotnet_build,
             "cpp": builder.cpp_build, "client": lambda: builder.go_build(True)}[item]()
    except Exception as error:
        builder.finish(str(error))
        raise
    print(json.dumps(builder.finish()))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
