#!/usr/bin/env python3
"""Build isolated normal/tuned binaries; never relabel a baseline or run load.

Profile generation manifests are training-only. Profile-use needs an attested
normal-exit training receipt with independent training/validation plan hashes.
Use the common HTTP/batch harness for actual correctness and paired measurement.
"""
from __future__ import annotations
import argparse
import copy
import hashlib
import json
import os
from pathlib import Path
import platform
import shutil
import shlex
import signal
import subprocess
import time

STACKS = ('ku', 'go', 'rust', 'cpp')
VARIANTS = ('normal', 'o2', 'o3', 'thin', 'generate', 'use')
SUPPORTED = {'ku': VARIANTS, 'cpp': VARIANTS, 'go': ('normal', 'generate', 'use'),
             'rust': ('normal', 'o2', 'thin', 'generate', 'use')}


def digest(path):
    result = hashlib.sha256()
    with Path(path).open('rb') as stream:
        for block in iter(lambda: stream.read(1048576), b''):
            result.update(block)
    return result.hexdigest()


def save(path, value):
    path = Path(path)
    tmp = path.with_suffix(path.suffix + '.writing')
    tmp.write_text(json.dumps(value, indent=2) + '\n')
    os.replace(tmp, path)


def load(path):
    return json.loads(Path(path).read_text())


def clean_environment(tools):
    env = dict(os.environ)
    for key in ('RUSTFLAGS', 'CARGO_ENCODED_RUSTFLAGS', 'RUSTC_WRAPPER', 'GOFLAGS',
                'CARGO_PROFILE_RELEASE_OPT_LEVEL', 'CARGO_PROFILE_RELEASE_LTO',
                'CARGO_PROFILE_RELEASE_CODEGEN_UNITS', 'RUSTC_WORKSPACE_WRAPPER', 'GODEBUG', 'LLVM_PROFILE_FILE',
                'BENCH_CPU_PROFILE', 'CFLAGS', 'CXXFLAGS', 'LDFLAGS'):
        env.pop(key, None)
    env.update(CARGO_HOME=str(tools/'cargo'), RUSTUP_HOME=str(tools/'rustup'),
               CARGO_BUILD_JOBS='2', CARGO_INCREMENTAL='0', RUSTUP_AUTO_INSTALL='0', GOTOOLCHAIN='local',
               GOMAXPROCS='2', GOMODCACHE=str(tools/'gomodcache'), GOCACHE=str(tools/'variant-go-cache'), CC='clang-18', CXX='clang++-18')
    env['PATH'] = ':'.join([str(tools/'cargo/bin'), str(tools/'go/bin'), env.get('PATH', '')])
    return env


class Commands:
    def __init__(self, directory, environment):
        self.directory, self.environment, self.rows = directory, environment, []
        directory.mkdir()

    def run(self, label, command, cwd, env=None, timeout=900):
        log = self.directory/(str(len(self.rows))+'-'+label+'.log')
        row = {'label': label, 'argv': list(map(str, command)), 'cwd': str(cwd),
               'timeout_seconds': timeout, 'stdout_stderr_limit_bytes': 16777216,
               'started_wall_ns': time.time_ns(), 'started_monotonic_ns': time.monotonic_ns()}
        self.rows.append(row)
        with log.open('xb') as stream:
            process = subprocess.Popen(row['argv'], cwd=cwd,
                env=dict(self.environment, **(env or {})), stdin=subprocess.DEVNULL,
                stdout=stream, stderr=subprocess.STDOUT, start_new_session=True)
            try:
                deadline = time.monotonic()+timeout
                while process.poll() is None:
                    if time.monotonic() > deadline or log.stat().st_size > 16777216:
                        raise RuntimeError('bounded compile step time/output limit: '+label)
                    time.sleep(.05)
            finally:
                if process.poll() is None:
                    os.killpg(process.pid, signal.SIGKILL)
                    process.wait(timeout=5)
                row.update(exit_code=process.returncode, ended_wall_ns=time.time_ns(),
                    ended_monotonic_ns=time.monotonic_ns(), log=str(log), log_sha256=digest(log))
                save(self.directory/'commands.json', self.rows)
            if process.returncode:
                raise RuntimeError('compile failed; retained log '+str(log))
        return log.read_text(errors='replace').strip()


def profile_input(path, stack, source_fingerprint):
    receipt = load(path)
    if (receipt.get('passed') is not True or receipt.get('domain_removed') is not True
            or receipt.get('stack') != stack or receipt.get('source_fingerprint') != source_fingerprint
            or receipt.get('normal_exit') is not True or receipt.get('all_business_results_correct') is not True
            or receipt.get('profile_complete') is not True or receipt.get('forced_termination') is not False):
        raise ValueError('profile training identity/completion mismatch')
    training, validation = receipt.get('training_plan_sha256'), receipt.get('validation_plan_sha256')
    if (not isinstance(training, str) or len(training) != 64 or not isinstance(validation, str)
            or len(validation) != 64 or training == validation):
        raise ValueError('distinct frozen training and validation plans required')
    inputs = [receipt.get('training_input_sha256'), receipt.get('validation_input_sha256')]
    if any(not isinstance(x, str) or len(x) != 64 for x in inputs) or inputs[0] == inputs[1]:
        raise ValueError('held-out validation inputs required, not merely a different rate')
    profile = Path(receipt['profile_path']).resolve(strict=True)
    if not profile.is_file() or not profile.stat().st_size or digest(profile) != receipt['profile_sha256']:
        raise ValueError('profile file absent, empty or changed')
    return profile, receipt


def input_files(manifest, stack, tools):
    source = Path(manifest['source_root'])/'tests/benchmarks/cross_stack'
    if stack == 'ku':
        build = manifest['stacks']['ku']['build']
        native = build['native']
        files = [Path(build['cli'])]
        for item in native.values():
            files += [Path(item['source']), Path(item['generated_c'])]
        return {str(path): digest(path) for path in files}
    base = source/stack
    result = {str(p): digest(p) for p in sorted(base.rglob('*')) if p.is_file()
              and not set(p.relative_to(base).parts)&{'target', 'node_modules', 'bin', 'obj', '__pycache__'}}
    if stack == 'cpp':
        for p in sorted((tools/'drogon-src').rglob('*')):
            if p.is_file() and not p.is_symlink() and '.git' not in p.parts:
                result[str(p)] = digest(p)
    return result


def verified_pg_identity(manifest, stack, inputs):
    """Carry only the source-attested mode contract, never old artifact identity."""
    if stack not in ('go', 'rust'):
        return {}
    reference = manifest['stacks'][stack].get('build', {})
    if 'pg_modes' not in reference:
        return {}
    directory = Path(manifest['source_root'])/'tests/benchmarks/cross_stack'/stack
    sources = {Path(path).relative_to(directory).as_posix(): value
               for path, value in inputs.items()}
    required = 'pg_mode.go' if stack == 'go' else 'src/pg_mode.rs'
    if (reference['pg_modes'] != ['normal', 'reset-equivalent'] or required not in sources
            or sources != reference.get('source_files_sha256')):
        raise ValueError(stack+' PG mode source identity mismatch; rebuild the base manifest')
    return {'pg_modes': list(reference['pg_modes']), 'source_files_sha256': sources}


def native_flags(stack, variant, profile, profiles):
    flags = ['-O2' if variant == 'o2' or (stack == 'ku' and variant == 'normal') else '-O3']
    if stack == 'cpp':
        flags.append('-DNDEBUG')
    if variant == 'thin':
        flags += ['-flto=thin', '-fuse-ld=lld-18', '-Wl,--thinlto-jobs=2']
    elif variant == 'generate':
        flags += ['-fprofile-generate='+str(profiles), '-fprofile-update=atomic']
    elif variant == 'use':
        flags += ['-fprofile-use='+str(profile), '-Werror=profile-instr-out-of-date']
    return flags


def build(manifest_path, output, tools, stack, variant, training_receipt=None):
    manifest = load(manifest_path)
    if stack not in manifest['stacks'] or variant not in SUPPORTED[stack]:
        raise ValueError('variant is not applicable to requested compiler/stack')
    if output.exists():
        raise ValueError('fresh output required; do not overwrite prior experiment')
    output.mkdir(parents=True)
    env = clean_environment(tools)
    commands = Commands(output/'logs', env)
    source = Path(manifest['source_root'])/'tests/benchmarks/cross_stack'
    inputs = input_files(manifest, stack, tools)
    pg_identity = verified_pg_identity(manifest, stack, inputs)
    fingerprint = hashlib.sha256(json.dumps({'stack': stack, 'inputs': inputs,
        'production_inventory': manifest.get('source_inventory_sha256')}, sort_keys=True).encode()).hexdigest()
    profile = training = None
    if variant == 'use':
        if not training_receipt:
            raise ValueError('profile-use requires verified training receipt')
        profile, training = profile_input(training_receipt, stack, fingerprint)
    receipt = {'schema': 1, 'stack': stack, 'variant': variant,
        'compiler_driver_sha256': digest(__file__),
        'source_fingerprint': fingerprint, 'source_inputs': inputs,
        'source_manifest': str(manifest_path), 'source_manifest_sha256': digest(manifest_path),
        'production_candidate': manifest['candidate_base'], 'commands': commands.rows,
        'training_only': variant == 'generate', 'performance_measured': False,
        'target_achieved': 'unassessed', 'training_receipt': training,
        'scope': 'compiler flag variant only; no timeout/resource/JSON/database semantics changes',
        'machine_specific_flags': False, 'fast_math_or_unsigned_overflow_shortcuts': False}
    save(output/'build.json', receipt)
    spec = copy.deepcopy(manifest['stacks'][stack])
    try:
        version_commands = {'go': [str(tools/'go/bin/go'), 'version'],
            'rust': [str(tools/'cargo/bin/rustc'), '+1.89.0', '-vV'],
            'ku': ['clang-18', '--version'], 'cpp': ['clang++-18', '--version']}
        command = version_commands[stack]
        receipt['compiler_version'] = commands.run('compiler-version', command, output, timeout=30)
        compiler = Path(shutil.which(command[0], path=env['PATH']) or command[0]).resolve(strict=True)
        receipt['compiler_launcher_sha256'] = digest(compiler)
        if variant == 'thin' and stack in ('ku', 'cpp'):
            receipt['linker_version'] = commands.run('linker-version', ['ld.lld-18', '--version'], output, timeout=30)
        profiles = output/'profiles'
        profiles.mkdir()
        if stack == 'ku':
            flags = native_flags(stack, variant, profile, profiles)
            artifacts = {}
            native = spec['build']['native']
            cli = Path(spec['build']['cli']).resolve(strict=True)
            if digest(cli) != spec['build']['cli_sha256']:
                raise ValueError('normal Ku CLI identity changed')
            work, binaries, private = output/'ku-native', output/'bin', output/'libpq'
            for directory in (work, binaries, private):
                directory.mkdir()
            libpq = Path(spec['build']['libpq_source']).resolve(strict=True)
            if digest(libpq) != spec['build']['libpq_sha256']:
                raise ValueError('normal libpq identity changed')
            shutil.copyfile(libpq, private/'libpq.so')
            native_extra = [flag for flag in flags if flag not in ('-O2', '-O3')]
            compiler_command = shlex.join(['clang-18', *native_extra])
            receipt['normal_cli'] = {'path': str(cli), 'sha256': digest(cli),
                'profile': 'release' if variant in ('normal','o2') else 'fast',
                'KU_CC': compiler_command, 'profile_contract':
                'ordinary Release remains O2; explicit Fast is O3; tuning flags are isolated compiler inputs'}
            for name, item in native.items():
                source = work/(name+'.ku')
                original = Path(item['source']).resolve(strict=True)
                if digest(original) != item['source_sha256']:
                    raise ValueError('expanded Ku source changed: '+name)
                shutil.copyfile(original, source)
                binary = binaries/('ku-'+name)
                argv = [str(cli),'build','--backend','c','--profile',
                    receipt['normal_cli']['profile'],'--emit-c','--verbose','-o',str(binary),str(source)]
                emitted = commands.run('ku-cli-'+name,argv,work,
                    env={'KU_CC':compiler_command,'KU_PG_LIB':str(private)},timeout=180)
                generated = [line.removeprefix('native c ok: ') for line in emitted.splitlines()
                    if line.startswith('native c ok: ')]
                if len(generated)!=1 or not any(line=='build ok: '+str(binary) for line in emitted.splitlines()):
                    raise ValueError('ordinary Ku CLI did not attest C and executable')
                generated = Path(generated[0]).resolve(strict=True)
                if work.resolve() not in generated.parents:
                    raise ValueError('Ku generated C escaped isolated build')
                artifacts[name] = {'binary': str(binary), 'sha256': digest(binary), 'bytes': binary.stat().st_size}
                item.update(source=str(source), source_sha256=digest(source),
                    generated_c=str(generated), generated_c_sha256=digest(generated),
                    binary=str(binary), binary_sha256=digest(binary),
                    linking='ordinary Ku CLI; isolated build profile and attested compiler flags')
            spec.update(command=[artifacts['server']['binary']], batch_command=[artifacts['batch']['binary']])
            spec['build'].update(native_flags=shlex.join(flags), cc=compiler_command)
            receipt['flags'] = flags
            receipt['artifacts'] = artifacts
        elif stack == 'go':
            binary = output/'go-server'
            argv = [str(tools/'go/bin/go'), 'build', '-trimpath', '-mod=readonly',
                    '-pgo='+str(profile) if profile else '-pgo=off', '-o', str(binary), '.']
            commands.run('go', argv, source/'go', timeout=300)
            spec.update(command=[str(binary)], batch_command=[str(binary), '--batch'])
        elif stack == 'rust':
            flags = {}
            if variant == 'o2':
                flags['CARGO_PROFILE_RELEASE_OPT_LEVEL'] = '2'
            elif variant == 'thin':
                flags['CARGO_PROFILE_RELEASE_LTO'] = 'thin'
            elif variant == 'generate':
                flags['RUSTFLAGS'] = '-Cprofile-generate='+str(profiles)
            elif variant == 'use':
                flags['RUSTFLAGS'] = '-Cprofile-use='+str(profile)+' -Cllvm-args=-pgo-warn-missing-function'
            target = output/'target'
            commands.run('rust', [str(tools/'cargo/bin/cargo'), '+1.89.0', 'build', '--release',
                '--locked', '--jobs', '2', '--target-dir', str(target)], source/'rust', env=flags, timeout=900)
            binary = target/'release/ku-cross-stack-rust'
            spec.update(command=[str(binary)], batch_command=[str(binary), '--batch', '{batch_input}'])
            receipt['compiler_environment'] = flags
        else:
            flags = native_flags(stack, variant, profile, profiles)
            compile_flags = [x for x in flags if not x.startswith(('-fuse-ld=', '-Wl,'))]
            link_flags = [x for x in flags if x.startswith(('-fuse-ld=', '-Wl,'))]
            cmake_link = ['-DCMAKE_EXE_LINKER_FLAGS='+' '.join(link_flags),
                '-DCMAKE_SHARED_LINKER_FLAGS='+' '.join(link_flags),
                '-DCMAKE_AR=/usr/bin/llvm-ar-18', '-DCMAKE_RANLIB=/usr/bin/llvm-ranlib-18']
            receipt['compile_flags'], receipt['link_only_flags'] = compile_flags, link_flags
            prefix, deps = output/'drogon', output/'drogon-cmake'
            # Compile framework and app consistently; static O2 libraries cannot
            # be relabelled as an O3/LTO/PGO complete C++ stack.
            commands.run('drogon-configure', ['cmake', '-S', str(tools/'drogon-src'), '-B', str(deps),
                '-DCMAKE_BUILD_TYPE=Release', '-DCMAKE_CXX_COMPILER=clang++-18', '-DCMAKE_C_COMPILER=clang-18',
                '-DCMAKE_CXX_FLAGS_RELEASE='+' '.join(compile_flags), '-DCMAKE_C_FLAGS_RELEASE='+' '.join(compile_flags),
                '-DCMAKE_INSTALL_PREFIX='+str(prefix), '-DBUILD_SHARED_LIBS=OFF', '-DBUILD_EXAMPLES=OFF',
                '-DBUILD_CTL=OFF', '-DBUILD_TESTING=OFF', '-DBUILD_ORM=ON', '-DBUILD_BROTLI=OFF', *cmake_link], output, timeout=120)
            commands.run('drogon-build', ['cmake', '--build', str(deps), '-j2'], output, timeout=600)
            commands.run('drogon-install', ['cmake', '--install', str(deps)], output, timeout=120)
            target = output/'app'
            commands.run('cpp-configure', ['cmake', '-S', str(source/'cpp'), '-B', str(target),
                '-DCMAKE_BUILD_TYPE=Release', '-DCMAKE_CXX_COMPILER=clang++-18',
                '-DCMAKE_CXX_FLAGS_RELEASE='+' '.join(compile_flags), '-DCMAKE_PREFIX_PATH='+str(prefix), *cmake_link], output, timeout=120)
            commands.run('cpp-build', ['cmake', '--build', str(target), '-j2'], output, timeout=300)
            binary = target/'cpp-server'
            spec.update(command=[str(binary)], batch_command=[str(binary), '--batch', '{batch_input}'])
            receipt['flags'] = flags
            receipt['framework_prefix'] = str(prefix)
        if variant == 'generate':
            # Rust build scripts/proc macros can execute while compiling. Their
            # profiles are build evidence, not application training input.
            compile_profiles = list(profiles.glob('*.profraw'))
            if compile_profiles:
                destination = output/'build-time-profiles'
                destination.mkdir()
                receipt['build_time_profiles_excluded'] = {}
                for item in compile_profiles:
                    saved = destination/item.name
                    item.rename(saved)
                    receipt['build_time_profiles_excluded'][str(saved)] = digest(saved)
        spec['env'] = dict(spec.get('env', {}))
        if variant == 'generate':
            if stack == 'go':
                spec['env']['BENCH_CPU_PROFILE'] = '{workdir}/cpu.pprof'
            else:
                spec['env']['LLVM_PROFILE_FILE'] = str(profiles/'%m-%p.profraw')
        artifacts = {str(Path(x)): {'sha256': digest(x), 'bytes': Path(x).stat().st_size}
                     for x in set(spec['command']+spec.get('batch_command', [])) if Path(x).is_file()}
        receipt['executable_artifacts'] = artifacts
        normal_reference = manifest['stacks'][stack].get('build', {})
        actual_build = spec.get('build', {}) if stack == 'ku' else pg_identity
        spec['build'] = dict(actual_build, variant_receipt=str(output/'build.json'),
                             variant_artifacts=artifacts, variant_flags=receipt.get('flags'),
                             variant_environment=receipt.get('compiler_environment'),
                             normal_flags_reference=normal_reference)
        if stack != 'ku':
            spec['build']['binary_sha256'] = digest(spec['command'][0])
        if stack == 'cpp':
            receipt['framework_artifacts'] = {str(p): {'sha256': digest(p), 'bytes': p.stat().st_size}
                for p in sorted((prefix/'lib').rglob('*')) if p.is_file()}
            spec['build']['framework_artifacts'] = receipt['framework_artifacts']
            spec['build']['framework_prefix'] = str(prefix)
            spec['env']['LD_LIBRARY_PATH'] = str(prefix/'lib')
        receipt['source_files_unchanged'] = inputs == input_files(manifest, stack, tools)
        if not receipt['source_files_unchanged']:
            raise RuntimeError('source changed during compilation')
        spec['compiler_variant'] = {'stack': stack, 'variant': variant,
            'receipt': str(output/'build.json'), 'source_fingerprint': fingerprint,
            'training_only': variant == 'generate'}
        manifest['stacks'][stack] = spec
        manifest['compiler_variant'] = spec['compiler_variant']
        manifest['formal_measurement_permitted'] = variant != 'generate'
        save(output/'manifest.json', manifest)
        receipt.update(passed=True, ended_wall_ns=time.time_ns())
        save(output/'build.json', receipt)
        return receipt
    except BaseException as error:
        receipt.update(passed=False, error=repr(error), ended_wall_ns=time.time_ns())
        save(output/'build.json', receipt)
        raise


def optimization_records(manifest_path, output, tools, variant):
    """Analysis objects/YAML only: never replace a measured executable."""
    if variant not in ('normal', 'o2', 'o3') or output.exists():
        raise ValueError('analysis requires normal/o2/o3 and fresh output')
    manifest=load(manifest_path)
    inputs=input_files(manifest,'ku',tools)
    output.mkdir(parents=True)
    commands=Commands(output/'logs',clean_environment(tools))
    result={'analysis_only':True,'performance_measured':False,'training_only':False,
        'compiler_driver_sha256':digest(__file__),
        'source_manifest':str(manifest_path),'source_manifest_sha256':digest(manifest_path),
        'production_candidate':manifest['candidate_base'],'source_inputs':inputs,
        'flags':['-O3' if variant=='o3' else '-O2'],'commands':commands.rows,
        'scope':'Linux Clang optimization records and separate analysis objects; no measured ELF replaced',
        'passed':False,'started_wall_ns':time.time_ns()}
    try:
        result['compiler_version']=commands.run('clang-version',['clang-18','--version'],output,timeout=30)
        result['artifacts']={}
        for name,item in manifest['stacks']['ku']['build']['native'].items():
            obj=output/(name+'.analysis.o')
            remarks=output/(name+'.opt.yaml')
            commands.run('remarks-'+name,['clang-18','-std=c11','-pthread',*result['flags'],
                '-c',item['generated_c'],'-o',str(obj),'-fsave-optimization-record=yaml',
                '-foptimization-record-file='+str(remarks)],output,timeout=180)
            if not remarks.is_file() or not remarks.stat().st_size:
                raise ValueError('compiler produced no optimization record')
            for path in (obj,remarks):
                result['artifacts'][str(path)]={'sha256':digest(path),'bytes':path.stat().st_size}
        result['source_files_unchanged']=inputs==input_files(manifest,'ku',tools)
        if not result['source_files_unchanged']:
            raise ValueError('analysis source identity changed')
        result['passed']=True
    except BaseException as error:
        result['error']=repr(error)
        raise
    finally:
        result['ended_wall_ns']=time.time_ns()
        save(output/'build.json',result)
    return result


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--manifest', type=Path, required=True)
    parser.add_argument('--output', type=Path, required=True)
    parser.add_argument('--tools', type=Path, default=Path('/opt/ku-bench/tools'))
    parser.add_argument('--stack', choices=STACKS, required=True)
    parser.add_argument('--variant', choices=VARIANTS, required=True)
    parser.add_argument('--training-receipt', type=Path)
    parser.add_argument('--analysis-only', action='store_true', help='Ku only: independent analysis objects and Linux LLVM optimization YAML, no executable manifest')
    args = parser.parse_args()
    if platform.system() != 'Linux' or os.geteuid() == 0:
        parser.error('use benchmark user on isolated Linux fixture, not root or host')
    if args.analysis_only:
        if args.stack!='ku':
            parser.error('--analysis-only requires --stack ku')
        result=optimization_records(args.manifest.resolve(strict=True), args.output.resolve(),
            args.tools.resolve(strict=True),args.variant)
        artifact=args.output/'build.json'
    else:
        result = build(args.manifest.resolve(strict=True), args.output.resolve(),
            args.tools.resolve(strict=True), args.stack, args.variant, args.training_receipt)
        artifact=args.output/'manifest.json'
    print(json.dumps({'passed': result['passed'], 'artifact': str(artifact),
                      'training_only': result['training_only']}))


if __name__ == '__main__':
    main()
