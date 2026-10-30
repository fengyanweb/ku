#!/usr/bin/env python3
"""Consume an actual exported archive on a separate publisher/consumer job.

Uses the existing bounded process-tree owner. Only owned temporary paths and,
with --hide-checkout on GitHub Actions, that disposable checkout are mutated.
No global Rust default, project configuration or user Cargo cache is changed.
"""
from __future__ import annotations

import argparse
from concurrent.futures import ThreadPoolExecutor
from contextlib import contextmanager
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import shutil
import stat
import sys
import tempfile
import time

sys.dont_write_bytecode = True


def load(name, filename):
    spec = importlib.util.spec_from_file_location(name, Path(__file__).with_name(filename))
    assert spec and spec.loader
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


EXPORT = load("consume_export", "export-release.py")
OWNER = load("consume_owner", "verify-native-three-os.py")
OWNER.MAX_CAPTURE_BYTES = 2 * 1024 * 1024


def write_report(path, value):
    """Serialize UTF-8 before opening anything; replace only a complete file.

    A serialization/encoding/write failure leaves the last valid report intact.
    The temporary file is owned, in the same directory, and always cleaned.
    """
    contents = (json.dumps(value, ensure_ascii=False, indent=2, allow_nan=False) + "\n").encode("utf-8")
    path = Path(path)
    path.parent.mkdir(parents=True, exist_ok=True)
    temporary_path = None
    try:
        with tempfile.NamedTemporaryFile(mode="wb", prefix="." + path.name + ".", suffix=".writing", dir=path.parent, delete=False) as temporary:
            temporary_path = Path(temporary.name)
            temporary.write(contents)
            temporary.flush()
            os.fsync(temporary.fileno())
        os.replace(temporary_path, path)
    finally:
        if temporary_path is not None:
            temporary_path.unlink(missing_ok=True)


@contextmanager
def environment(values):
    original = dict(os.environ)
    os.environ.clear()
    os.environ.update(values)
    try:
        yield
    finally:
        os.environ.clear()
        os.environ.update(original)


def output(result):
    return result.stdout.decode("utf-8", errors="replace").replace("\r\n", "\n")


def invoke(command, cwd, env, seconds=300):
    OWNER.COMMAND_TIMEOUT_SECONDS = seconds
    with environment(env):
        return OWNER.run_bounded([str(p) for p in command], cwd, "Runner archive consumer", check=False)


def checked(command, cwd, env):
    result = invoke(command, cwd, env)
    if result.returncode:
        raise RuntimeError(output(result) + result.stderr.decode(errors="replace")[-4096:])
    return output(result)


@contextmanager
def changed(path, contents):
    before = path.read_bytes()
    path.write_bytes(contents)
    try:
        yield
    finally:
        path.write_bytes(before)


def management_invocation(name):
    """Preserve Rustup's argv[0] alias; rustup-init is a different mode.

    abspath removes cwd dependence without resolving a management symlink.
    """
    found = shutil.which(name)
    return os.path.abspath(found) if found else None


def relocate_checkout_entry(source, destination):
    """CI-owned source/target entries may be on a different Windows volume."""
    shutil.move(str(source), str(destination))


def reported_artifact(text, prefix, cwd, owned_root):
    """Resolve CLI paths in its cwd and check actual directory identities.

    Windows normal and extended-length spellings can name the same file but
    have different lexical anchors. samefile() handles that without weakening
    the check to a textual prefix. Quoted display paths are handled explicitly.
    """
    matches = [line[len(prefix):].strip() for line in text.splitlines() if line.startswith(prefix)]
    if len(matches) != 1 or not matches[0] or len(matches[0]) > 4096:
        raise RuntimeError('expected one bounded artifact path; prefix=' + repr(prefix) + '; stdout=' + repr(text[:1000]))
    reported = matches[0]
    candidates = [reported]
    if len(reported) > 1 and reported[0] == reported[-1] and reported[0] in ('"', "'"):
        candidates.append(reported[1:-1])
        if reported[0] == '"':
            try:
                decoded = json.loads(reported)
                if isinstance(decoded, str) and decoded not in candidates:
                    candidates.append(decoded)
            except ValueError:
                pass
    diagnostics = []
    for spelling in candidates:
        path = Path(spelling)
        if not path.is_absolute():
            path = Path(cwd) / path
        try:
            resolved = path.resolve(strict=True)
            if not resolved.is_file():
                raise ValueError('reported artifact is not a regular file')
            parent = resolved.parent
            for _ in range(128):
                if os.path.samefile(parent, owned_root):
                    return resolved
                next_parent = parent.parent
                if next_parent == parent:
                    break
                parent = next_parent
            diagnostics.append('outside owned root; resolved=' + repr(str(resolved))[:500] + '; parent=' + repr(str(resolved.parent))[:500])
        except (OSError, RuntimeError, ValueError) as error:
            diagnostics.append(type(error).__name__ + ': ' + str(error)[:500])
    raise RuntimeError('IR artifact path unresolved/unowned: reported=' + repr(reported)[:600] +
        '; cwd=' + repr(str(cwd))[:500] + '; root=' + repr(str(owned_root))[:500] + '; attempts=' + repr(diagnostics)[:1500])


def project_contract_fixture(project, source):
    """Use the public template layout; ku.mod deliberately rejects root='.'."""
    mod = project / 'ku.mod'
    source_root = project / 'src'
    if mod.exists() or source_root.exists():
        raise ValueError('project contract would replace existing owned fixture data')
    source_root.mkdir()
    entry = source_root / 'main.ku'
    entry.write_bytes(source.read_bytes())
    mod.write_text('name = "hello"\nversion = "0.1.0"\nroot = "src"\nmain = "main.ku"\ncache = ".ku/cache"\nout = ".ku/build"\n', encoding='utf-8')
    return mod, entry


def run(args, report):
    repo = args.repo.resolve()
    archive = args.archive.resolve()
    metadata = json.loads(args.manifest.read_text(encoding="utf-8"))
    if EXPORT.file_hash(archive) != metadata["sha256"] or archive.stat().st_size != metadata["size"]:
        raise ValueError("downloaded archive SHA/size mismatch")
    rustup = management_invocation("rustup")
    if not rustup:
        raise ValueError("consumer gate requires rustup; the separate no-Rust case isolates it")
    base_env = dict(os.environ, RUSTUP_AUTO_INSTALL="0")
    before_default = checked([rustup, "default"], repo, base_env)
    verifier_commit = checked(['git', 'rev-parse', 'HEAD'], repo, base_env).strip()
    if len(verifier_commit) != 40 or any(c not in '0123456789abcdef' for c in verifier_commit):
        raise ValueError('verification source commit is not a full Git SHA')
    if args.expected_sdk_commit is not None and metadata['commit'] != args.expected_sdk_commit:
        raise ValueError('archive source identity differs from the selected installer run')
    target = metadata["target"]
    report.update(schema="ku-runtime-consumer-v1", source_commit=metadata["commit"],
                  sdk_source_commit=metadata["commit"], verification_code_commit=verifier_commit,
                  installer_run_id=args.installer_run, verification_run_id=os.environ.get("GITHUB_RUN_ID"),
                  target=target, archive_sha256=metadata["sha256"], cases=[], status="running")
    with tempfile.TemporaryDirectory(prefix="ku-runner-consumer-") as temporary:
        root = Path(temporary).resolve()
        extracted = root / "first-install"
        extracted.mkdir()
        EXPORT.verify_archive(archive, metadata["files"], extracted)
        manifest = EXPORT.RUNTIME_BUNDLE.verify(extracted, metadata["version"], metadata["commit"], target)
        required = manifest["rustc"]["toolchain"]
        consumer_path = Path(checked([rustup, "which", "--toolchain", args.consumer_toolchain, "rustc"], repo, base_env).strip())
        compiler_text = checked([consumer_path, "-vV"], repo, base_env)
        consumer_host = next(line.split(": ", 1)[1] for line in compiler_text.splitlines() if line.startswith("host: "))
        consumer_release = next(line.split(": ", 1)[1] for line in compiler_text.splitlines() if line.startswith("release: "))
        if consumer_host != target or consumer_release == manifest["rustc"]["release"]:
            raise ValueError("consumer compiler must have this host and differ from publisher")
        consumer_name = consumer_release + "-" + target
        home = root / "isolated-rustup"
        (home / "toolchains").mkdir(parents=True)
        # Only the unlike consumer is copied. Exact publisher T starts missing;
        # doctor must install it into this disposable home using its real path.
        shutil.copytree(consumer_path.parent.parent, home / "toolchains" / consumer_name)
        (home / "settings.toml").write_text('version = "12"\ndefault_toolchain = "' + consumer_name + '"\nprofile = "minimal"\n', encoding="utf-8")
        env = dict(base_env, RUSTUP_HOME=str(home))
        env.pop("RUSTUP_TOOLCHAIN", None)
        env.pop("KU_RUNNER_RUSTC", None)
        install = root / "搬迁 中文 空格" / "portable Ku"
        install.parent.mkdir()
        extracted.rename(install)
        ku = install / ("ku.exe" if os.name == "nt" else "ku")
        project = root / "project outside install"
        project.mkdir()
        source = project / "hello.ku"
        source.write_bytes(EXPORT.canonical_example((repo / "examples/hello.ku").read_bytes()))
        expected = "Hello Ku\n"
        report.update(publisher=manifest["rustc"], consumer={"release": consumer_release, "host": consumer_host},
                      bundle_id=manifest["bundle_id"], installation_moved=True,
                      build_target_from_publisher="owned package target was deleted before archive export")

        def record(name, result, expected_exit="zero"):
            report["cases"].append(dict(name=name, exit_code=result.returncode, expected_exit=expected_exit,
                stdout_sha256=hashlib.sha256(result.stdout).hexdigest(), stderr_sha256=hashlib.sha256(result.stderr).hexdigest()))
            write_report(args.output, report)

        def success(name, arguments, wanted=None, selected_env=None, working=None):
            result = invoke([ku, *arguments], working or project, selected_env or env)
            record(name, result)
            if result.returncode or wanted is not None and output(result) != wanted:
                raise RuntimeError(name + ": " + output(result) + result.stderr.decode(errors="replace")[-4096:])
            return result

        def failure(name, selected_env=None):
            destination = project / ("preserved.exe" if os.name == "nt" else "preserved")
            destination.write_bytes(b"preexisting output must survive\n")
            result = invoke([ku, "build", "hello.ku", "-o", destination], project, selected_env or env)
            record(name, result, "nonzero-without-bare-E0514")
            text = output(result) + result.stderr.decode(errors="replace")
            if result.returncode == 0 or "E0514" in text or destination.read_bytes() != b"preexisting output must survive\n":
                raise RuntimeError(name + " did not fail before compiler/output replacement: " + text[-4096:])

        def remove_executable(path):
            path = Path(path)
            if os.name == 'nt' and path.suffix.lower() != '.exe':
                path = path.with_suffix('.exe')
            if not path.resolve().is_relative_to(root):
                raise ValueError('generated executable cleanup escaped owned root')
            if path.is_file():
                path.unlink()

        def execute_and_remove(name, destination, wanted=None):
            executable = Path(destination)
            if os.name == 'nt' and executable.suffix.lower() != '.exe':
                executable = executable.with_suffix('.exe')
            executed = invoke([executable], project, env)
            record(name, executed)
            if executed.returncode or output(executed) != (expected if wanted is None else wanted):
                raise RuntimeError(name + ' produced incorrect executable output')
            report['cases'][-1]['binary_sha256'] = EXPORT.file_hash(executable)
            remove_executable(executable)

        def build_run(name, arguments, destination, verbose_profile=None, entry="hello.ku", wanted=None):
            result = success(name, ['build', *arguments, '-o', destination, entry])
            if verbose_profile is not None:
                text = output(result)
                if 'build profile: ' + verbose_profile not in text or 'rustc command:' not in text:
                    raise RuntimeError(name + ' did not use expected Runner profile/verbose scope')
                report['cases'][-1]['observed_build_profile'] = verbose_profile
            execute_and_remove(name + '-result', destination, wanted)
            return result

        def exercise_cli_contracts():
            # The archive executable executes every spelling; source/parser
            # constants alone cannot satisfy this consumer contract.
            report['cli_contract'] = {'version': 'ku-cli-contract-v1', 'scope': 'real installed CLI invocations',
                                      'source_sha256': hashlib.sha256(source.read_bytes()).hexdigest()}
            for spelling in ('-v', '-V', '--version', 'version'):
                success('contract-version-' + spelling, [spelling], 'ku ' + metadata['version'] + '\n')
            help_reference = None
            before = {str(p.relative_to(project)) for p in project.rglob('*')}
            for spelling in ('-h', '-help', '--help', 'help'):
                result = success('contract-help-' + spelling, [spelling])
                if help_reference is None:
                    help_reference = output(result)
                if output(result) != help_reference:
                    raise RuntimeError('global help aliases differ')
            commands = [('run',), ('check',), ('create',), ('init',), ('template',), ('ir',), ('llvm',),
                        ('build',), ('package',), ('doctor',), ('supervise',), ('service',),
                        ('template', 'list'), *[('package', name) for name in ('gc', 'pack', 'publish', 'yank', 'resolve')]]
            for command in commands:
                label = '-'.join(command)
                first = success('contract-help-prefix-' + label, ['help', *command])
                second = success('contract-help-option-' + label, [*command, '--help'])
                if output(first) != output(second):
                    raise RuntimeError('subcommand help aliases differ: ' + label)
            if before != {str(p.relative_to(project)) for p in project.rglob('*')}:
                raise RuntimeError('help created project/build artifacts')
            for arguments in (['hello.ku', 'extra'], ['hello.ku', '--offline'], ['hello.ku', '--', 'arg1'],
                              ['r'], ['b'], ['c'], ['--v'], ['run', '--unknown-option']):
                result = invoke([ku, *arguments], project, env)
                record('contract-reject-' + '-'.join(arguments), result, 'nonzero')
                if result.returncode == 0 or output(result) == expected:
                    raise RuntimeError('unsupported implicit spelling unexpectedly ran source: ' + repr(arguments))
            for flag in ('--offline', '--locked'):
                success('contract-run-' + flag, ['run', flag, 'hello.ku'], expected)
            special = project / '-hello.ku'
            special.write_bytes(source.read_bytes())
            special_arg = '.' + os.sep + special.name
            success('contract-special-file-implicit', [special_arg], expected)
            success('contract-special-file-explicit', ['run', special_arg], expected)
            special.unlink()
            listed = success('contract-template-list', ['template', 'list'])
            if output(success('contract-create-list', ['create', '--list'])) != output(listed):
                raise RuntimeError('template list aliases differ')
            templates = root / 'template contract'
            for form, flag in (('short', '-t'), ('long', '--template')):
                create_root = templates / ('create-' + form)
                create_root.mkdir(parents=True)
                success('contract-create-template-' + form, ['create', 'demo', flag, 'http'], working=create_root)
                init_root = templates / ('init-' + form) / 'demo'
                init_root.mkdir(parents=True)
                success('contract-init-template-' + form, ['init', flag, 'cli'], working=init_root)
            def tree(path):
                return {p.relative_to(path).as_posix(): hashlib.sha256(p.read_bytes()).hexdigest()
                        for p in path.rglob('*') if p.is_file()}
            for kind in ('create', 'init'):
                if tree(templates / (kind + '-short') / 'demo') != tree(templates / (kind + '-long') / 'demo'):
                    raise RuntimeError('template short/long output differs: ' + kind)
            suffix = '.exe' if os.name == 'nt' else ''
            destination = project / 'dist contract' / ('hello' + suffix)
            for flag in ('-o', '--output'):
                result = success('contract-output-' + flag, ['build', flag, destination, 'hello.ku'])
                executed = invoke([destination], project, env)
                record('contract-output-' + flag + '-result', executed)
                if executed.returncode or output(executed) != expected:
                    raise RuntimeError('output alias produced incorrect program')
                remove_executable(destination)
            for profile in ('release', 'debug', 'small', 'fast'):
                destination = project / ('contract-profile-' + profile + suffix)
                for form, selection in (('short', ['--' + profile]), ('long', ['--profile', profile])):
                    build_run('contract-profile-' + profile + '-' + form, [*selection, '--verbose'], destination, profile)
            for flag in ('-v', '--verbose'):
                build_run('contract-build-verbose-' + flag, [flag], project / ('contract-verbose' + suffix), 'debug')
            for backend in ('runner', 'interp', 'interpreter', 'c', 'native-c'):
                build_run('contract-backend-' + backend, ['--backend', backend], project / ('contract-backend' + suffix))
            # LLVM is an explicitly limited text prototype. The ordinary
            # Hello Runner cases above remain unchanged; this empty-main IR
            # fixture is supported and its Runner executable emits no stdout.
            llvm_source = project / 'llvm-empty.ku'
            llvm_source.write_text('fn main() {}\n', encoding='utf-8')
            report['cli_contract']['llvm_fixture'] = dict(source_sha256=hashlib.sha256(llvm_source.read_bytes()).hexdigest(),
                expected_stdout='', scope='supported empty main;not the ordinary Hello fixture')
            limited_output = project / ('contract-hello-llvm-unsupported' + suffix)
            limited = invoke([ku, 'build', '--emit-ll', '-o', limited_output, 'hello.ku'], project, env)
            record('contract-hello-llvm-existing-limitation', limited, 'nonzero-println-requires-Result')
            limited_text = output(limited) + limited.stderr.decode('utf-8', errors='replace')
            if limited.returncode == 0 or "LLVM intrinsic 'println' requires a Result type" not in limited_text or limited_output.exists():
                raise RuntimeError('the documented Hello LLVM prototype limitation changed: ' + limited_text[-2048:])
            report['cli_contract']['hello_llvm_limitation'] = limited_text[-2048:]
            ir_hash = None
            for flag in ('--emit-ll', '--emit-llvm'):
                result = build_run('contract-' + flag, [flag], project / ('contract-emit' + suffix), entry=llvm_source.name, wanted='')
                ir = reported_artifact(output(result), 'llvm ir ok: ', project, root)
                report['cases'][-1]['ir_sha256'] = EXPORT.file_hash(ir)
                current = EXPORT.file_hash(ir)
                if ir_hash is not None and ir_hash != current:
                    raise RuntimeError('emit LLVM aliases produced different IR')
                ir_hash = current
            backend_ir_hash = None
            for backend in ('llvm', 'll'):
                destination = project / ('contract-llvm-no-executable' + suffix)
                result = invoke([ku, 'build', '--backend', backend, '-o', destination, llvm_source.name], project, env)
                record('contract-text-only-backend-' + backend, result, 'nonzero-after-text-IR')
                text = output(result) + result.stderr.decode(errors='replace')
                if result.returncode == 0 or 'LLVM backend does not link executables yet' not in text or destination.exists():
                    raise RuntimeError('LLVM backend contract changed: ' + backend + '; stdout=' + repr(output(result)[:1500]))
                ir = reported_artifact(output(result), 'llvm ir ok: ', project, root)
                current = EXPORT.file_hash(ir)
                if backend_ir_hash is not None and backend_ir_hash != current:
                    raise RuntimeError('LLVM backend aliases generated different IR')
                backend_ir_hash = current
            llvm_source.unlink()
            # Legacy --native only emits C when directly after build and no -o.
            c_source = source.with_suffix('.c')
            result = success('contract-native-legacy-C-only', ['build', '--native', 'hello.ku'])
            if not c_source.is_file() or not c_source.read_bytes():
                raise RuntimeError('legacy native form did not emit C')
            destination = project / ('contract-legacy-native' + suffix)
            build_run('contract-native-legacy-executable', ['--native'], destination)
            c_source.unlink()
            # Valid project context, not a directory-name guess. All commands
            # run from this same owned directory and restore its prior state.
            mod, project_entry = project_contract_fixture(project, source)
            try:
                success('contract-project-run-omitted', ['run'], expected)
                success('contract-project-check-omitted', ['check'])
                for label, arguments in (('omitted', []), ('directory', ['.']), ('host', ['--target', 'host'])):
                    success('contract-project-build-' + label, ['build', *arguments])
                    program = project / '.ku/build/debug' / ('hello' + suffix)
                    result = invoke([program], project, env)
                    record('contract-project-build-' + label + '-result', result)
                    if result.returncode or output(result) != expected:
                        raise RuntimeError('project entry shorthand produced incorrect executable')
                    remove_executable(program)
                # Only local operations: publish/yank are help-only above.
                success('contract-package-resolve-omitted', ['package', 'resolve', '--offline'])
                success('contract-package-gc-omitted', ['package', 'gc'])
                success('contract-package-pack-omitted', ['package', 'pack'])
            finally:
                mod.unlink()
                project_entry.unlink()
                project_entry.parent.rmdir()
            report['cli_contract']['status'] = 'passed'

        # Mechanism reproduction on known archive bytes, not a claim about
        # the user's original unidentified E:\ package. Unlike rustc directly
        # reading this package must reject its incompatible crate metadata.
        mismatch_source = root / 'compiler_mismatch.rs'
        mismatch_source.write_text('extern crate ku; fn main() { println!("{}", ku::cli::help_text()); }\n', encoding="utf-8")
        mismatch = invoke([consumer_path, '--edition=2021', '--crate-name', 'ku_consumer_mismatch',
                           mismatch_source, '--extern', 'ku=' + str(install / 'libku.rlib'),
                           '-L', 'dependency=' + str(install / 'deps'), '-o', root / 'must-not-build'], project, base_env)
        record('known-archive-direct-unlike-rustc-baseline', mismatch, 'nonzero-with-E0514')
        mismatch_text = output(mismatch) + mismatch.stderr.decode(errors='replace')
        if mismatch.returncode == 0 or 'E0514' not in mismatch_text:
            raise RuntimeError('known unlike-compiler baseline did not reproduce E0514: ' + mismatch_text[-4096:])
        report['known_identity_failure_baseline'] = dict(error='E0514', stderr_excerpt=mismatch_text[-4096:],
            consumer=consumer_release, publisher=manifest['rustc']['release'],
            source_sha256=hashlib.sha256(mismatch_source.read_bytes()).hexdigest(),
            user_original_archive_identity='unknown;not obtained')
        success("version-short", ["-v"], "ku " + metadata["version"] + "\n")
        if os.name == 'nt' and repo.drive.lower() != install.drive.lower():
            with tempfile.TemporaryDirectory(prefix='ku-cross-drive-', dir=repo.parent) as drive_temporary:
                old_install = install
                destination = Path(drive_temporary) / '换盘 中文 Ku'
                shutil.move(str(install), str(destination))
                install = destination
                ku = install / 'ku.exe'
                try:
                    success('cross-drive-interpret', ['hello.ku'], expected)
                finally:
                    shutil.move(str(install), str(old_install))
                    install = old_install
                    ku = install / 'ku.exe'
        success("implicit-run", ["hello.ku"], expected)
        success("explicit-run", ["run", "hello.ku"], expected)
        failure("exact-publisher-missing")
        installed = invoke([ku, "doctor", "--install-runner-toolchain"], project, env, seconds=660)
        record("explicit-install-exact-publisher", installed)
        if installed.returncode:
            raise RuntimeError("doctor install failed: " + output(installed) + installed.stderr.decode(errors="replace")[-4096:])
        doctor = success("runner-doctor", ["doctor", "--backend", "runner", "--json"])
        parsed_doctor = json.loads(output(doctor))
        selected = parsed_doctor['selected_compiler']
        if (parsed_doctor['schema'] != 'ku-doctor-v1' or selected['link_probe_passed'] is not True
                or any(selected['identity'][key] != manifest['rustc'][key] for key in ('release', 'commit_hash', 'commit_date', 'host'))):
            raise RuntimeError('doctor JSON does not identify an actually linkable exact compiler')
        report['selected_runner_compiler'] = selected
        other_install = root / 'other-ku-on-PATH'
        other_install.mkdir()
        shutil.copy2(ku, other_install / ku.name)
        conflicting_path = dict(env, PATH=str(other_install) + os.pathsep + env['PATH'])
        result = success('multiple-ku-actual-executable', ['doctor', '--json'], selected_env=conflicting_path)
        parsed = json.loads(output(result))
        if Path(parsed['executable']).resolve() != ku.resolve():
            raise RuntimeError('doctor does not identify the actually invoked installation')
        if os.name == 'nt' and repo.drive.lower() != install.drive.lower():
            with tempfile.TemporaryDirectory(prefix='ku-cross-drive-', dir=repo.parent) as drive_temporary:
                old_install = install
                destination = Path(drive_temporary) / '换盘 中文 Ku'
                shutil.move(str(install), str(destination))
                install = destination
                ku = install / 'ku.exe'
                try:
                    success('cross-drive-original-build', ['build', 'hello.ku', '-o', 'cross-drive-result'])
                    execute_and_remove('cross-drive-result', project / 'cross-drive-result')
                finally:
                    shutil.move(str(install), str(old_install))
                    install = old_install
                    ku = install / 'ku.exe'
        consumer_default = checked([rustup, "default"], project, env)
        if consumer_name not in consumer_default:
            raise RuntimeError("doctor changed isolated consumer default")
        before_project = b'[toolchain]\nchannel = "' + consumer_name.encode() + b'"\n'
        (project / "rust-toolchain.toml").write_bytes(before_project)
        configured = dict(env, RUSTUP_TOOLCHAIN=consumer_name)
        success("original-build-project-and-env-unlike", ["build", "hello.ku"], selected_env=configured)
        built = project / ".ku/build/debug" / ("hello.exe" if os.name == "nt" else "hello")
        result = invoke([built], root, env)
        record("original-build-result", result)
        if result.returncode or output(result) != expected:
            raise RuntimeError("original ku build output is incorrect")
        remove_executable(built)
        if (project / "rust-toolchain.toml").read_bytes() != before_project:
            raise RuntimeError("Ku changed the project toolchain file")
        failure("explicit-unlike-compiler", dict(env, KU_RUNNER_RUSTC=str(home / "toolchains" / consumer_name / "bin" / consumer_path.name)))

        # Add a genuinely incompatible libku with newer mtime, not a text mock.
        fake_source = root / "fake.rs"
        fake_source.write_text("pub fn wrong_library() {}\n", encoding="utf-8")
        wrong = install / "deps/libku-0000000000000000.rlib"
        result = invoke([consumer_path, "--crate-type", "rlib", "--crate-name", "ku", fake_source, "-o", wrong], project, base_env)
        if result.returncode:
            raise RuntimeError("cannot prepare real unlike-compiler interference")
        os.utime(wrong, (time.time() + 3600, time.time() + 3600))
        success("ignore-newer-incompatible-rlib", ["build", "hello.ku", "-o", "newer-ignored"])
        execute_and_remove("ignore-newer-incompatible-rlib-result", project / "newer-ignored")
        wrong.unlink()
        runtime_path = install / "ku-runtime.json"
        deps_path = install / "ku-runtime-deps.json"
        original_manifest = runtime_path.read_bytes()
        with changed(runtime_path, b"{invalid json"):
            failure("invalid-manifest")
        runtime_path.rename(root / "missing-manifest")
        try:
            failure("missing-manifest-no-development-fallback")
        finally:
            (root / "missing-manifest").rename(runtime_path)
        dependencies = json.loads(deps_path.read_text(encoding="utf-8"))
        dependency = install / dependencies["files"][-1]["path"]
        with changed(dependency, b"not the matching same-build dependency"):
            failure("mixed-or-corrupt-dependency")
        dependency.rename(root / "missing-dependency")
        try:
            failure("missing-dependency-no-ancestor-fallback")
        finally:
            (root / "missing-dependency").rename(dependency)
        mixed = json.loads(original_manifest)
        mixed["cli"]["sha256"] = "0" * 64
        with changed(runtime_path, json.dumps(mixed).encode()):
            failure("CLI-manifest-mixed")
        mixed = json.loads(original_manifest)
        mixed["runtime_target"] = "x86_64-pc-windows-gnu" if os.name == "nt" else "x86_64-pc-windows-msvc"
        with changed(runtime_path, json.dumps(mixed).encode()):
            failure("target-ABI-mismatch")
        escaped = json.loads(deps_path.read_text(encoding="utf-8"))
        escaped["files"][0]["path"] = "../outside.rlib"
        escaped_bytes = json.dumps(escaped).encode()
        mixed = json.loads(original_manifest)
        mixed["dependency_manifest_sha256"] = hashlib.sha256(escaped_bytes).hexdigest()
        with changed(deps_path, escaped_bytes), changed(runtime_path, json.dumps(mixed).encode()):
            failure("dependency-path-escape")

        # No Rust PATH: tools remain on disk but cannot be found through another
        # PATH entry or Rustup home. This case cannot accidentally use the copy.
        empty = root / "empty-path"
        empty.mkdir()
        no_rust = dict(env, PATH=str(empty), RUSTUP_HOME=str(root / "absent-rustup"))
        if os.name == "nt":
            no_rust["PATH"] += os.pathsep + str(Path(os.environ["SystemRoot"]) / "System32")
        success("no-rust-version", ["-v"], "ku " + metadata["version"] + "\n", no_rust)
        success("no-rust-interpreter", ["hello.ku"], expected, no_rust)
        failure("no-rust-runner", no_rust)
        standalone = root / "standalone-compiler-bin"
        standalone.mkdir()
        selected = Path(checked([rustup, "which", "--toolchain", required, "rustc"], project, env).strip())
        direct = dict(env, PATH=str(standalone), KU_RUNNER_RUSTC=str(selected))
        if os.name == "nt":
            # Keep linker/SDK PATH while remove only rustup/rustc entries.
            direct["PATH"] = os.pathsep.join(p for p in env["PATH"].split(os.pathsep)
                if not any((Path(p) / name).is_file() for name in ("rustup.exe", "rustc.exe")))
        else:
            direct["PATH"] = os.pathsep.join(p for p in env["PATH"].split(os.pathsep)
                if not any((Path(p) / name).is_file() for name in ("rustup", "rustc")))
        success("standalone-explicit-compiler-no-rustup", ["build", "hello.ku", "-o", "standalone-result"], selected_env=direct)
        execute_and_remove("standalone-explicit-compiler-result", project / "standalone-result")
        # Common immutable env for both processes; no per-thread env mutation.
        OWNER.COMMAND_TIMEOUT_SECONDS = 300
        with environment(env), ThreadPoolExecutor(max_workers=2) as pool:
            futures = [pool.submit(OWNER.run_bounded, [str(ku), "build", "hello.ku", "-o", str(project / ("parallel-" + str(i)))], project, "parallel Runner consumer", check=False) for i in range(2)]
            for i, future in enumerate(futures):
                result = future.result()
                record("parallel-build-" + str(i), result)
                if result.returncode:
                    raise RuntimeError("parallel build failed")
                execute_and_remove("parallel-result-" + str(i), project / ("parallel-" + str(i)))

        if args.hide_checkout:
            if os.environ.get("GITHUB_ACTIONS") != "true":
                raise ValueError("--hide-checkout is restricted to the disposable CI checkout")
            retired = []
            try:
                for name in ("src", "Cargo.toml", "target"):
                    old = repo / name
                    if old.exists():
                        destination = root / ("retired-checkout-" + name)
                        relocate_checkout_entry(old, destination)
                        retired.append((old, destination))
                success("source-and-target-unavailable", ["build", "hello.ku", "-o", "without-source"])
                execute_and_remove("source-and-target-unavailable-result", project / "without-source")
            finally:
                for old, destination in reversed(retired):
                    relocate_checkout_entry(destination, old)
        else:
            report["cases"].append({"name": "source-and-target-unavailable", "status": "not_requested_local;CI flag required"})
        exercise_cli_contracts()
        # Exercise the native path separately; it must not consume runner rlibs.
        success("native-doctor", ["doctor", "--backend", "c"])
        native = project / ("hello-native.exe" if os.name == "nt" else "hello-native")
        success("native-build-independent", ["build", "--backend", "c", "--release", "hello.ku", "-o", native])
        result = invoke([native], root, env)
        record("native-result", result)
        if result.returncode or output(result) != expected:
            raise RuntimeError("native guard output differs")
        remove_executable(native)
        # Enforce read-only installation, not only a no-write assertion.
        if os.name == "nt":
            sid = checked(["whoami", "/user", "/fo", "csv", "/nh"], root, base_env).strip().split(",")[-1].strip('"')
            # icacls requires '*' before numeric SIDs; otherwise it resolves
            # the SID text as an account name and fails before the probe.
            principal = "*" + sid
            # Generic W also denies SYNCHRONIZE, which read/execute needs.
            # Deny only mutation rights so this remains a runnable install.
            checked(["icacls", install, "/deny", principal + ":(OI)(CI)(WD,AD,WEA,WA,DC,D)"], root, base_env)
            try:
                probe = install / '.readonly-write-probe'
                try:
                    probe.write_bytes(b'must not be writable')
                except PermissionError:
                    pass
                else:
                    probe.unlink()
                    raise RuntimeError('Windows installation was not actually read-only')
                success("read-only-install", ["build", "hello.ku", "-o", "readonly-result"])
                execute_and_remove("read-only-install-result", project / "readonly-result")
            finally:
                checked(["icacls", install, "/remove:d", principal, "/T", "/C"], root, base_env)
        else:
            paths = [install, *install.rglob("*")]
            modes = [(p, stat.S_IMODE(p.stat().st_mode)) for p in paths]
            try:
                for p, mode in reversed(modes):
                    p.chmod(mode & ~0o222)
                probe = install / '.readonly-write-probe'
                try:
                    probe.write_bytes(b'must not be writable')
                except PermissionError:
                    pass
                else:
                    probe.unlink()
                    raise RuntimeError('Unix installation was not actually read-only')
                success("read-only-install", ["build", "hello.ku", "-o", "readonly-result"])
                execute_and_remove("read-only-install-result", project / "readonly-result")
            finally:
                for p, mode in modes:
                    p.chmod(mode)
        if checked([rustup, "default"], project, env) != consumer_default:
            raise RuntimeError("consumer default changed")
    if checked([rustup, "default"], repo, base_env) != before_default:
        raise RuntimeError("global default changed")
    report.update(status="passed", global_default_unchanged=True, isolated_default_unchanged=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--archive", type=Path, required=True)
    parser.add_argument("--manifest", type=Path, required=True)
    parser.add_argument("--repo", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--consumer-toolchain", default="1.96.0")
    parser.add_argument("--hide-checkout", action="store_true")
    parser.add_argument("--installer-run")
    parser.add_argument("--expected-sdk-commit")
    args = parser.parse_args()
    args.output = args.output.resolve()
    args.output.parent.mkdir(parents=True, exist_ok=True)
    report = {"status": "running"}
    try:
        run(args, report)
    except BaseException as error:
        report.update(status="failed", failure=type(error).__name__ + ": " + str(error)[-6000:])
        raise
    finally:
        write_report(args.output, report)


if __name__ == "__main__":
    main()
