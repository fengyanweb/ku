#!/usr/bin/env python3
"""Build/reuse the private, pinned shared calendar engine static pack.

Only ku-native-calendar is built. Existing Rust tools and cached dependencies
are required: this script never installs tools or accesses the network. Build
cache remains outside the immutable five-file pack. No Ku interpreter is linked.
"""
from __future__ import annotations

from contextlib import contextmanager
import argparse
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import re
import shlex
import shutil
import stat
import struct
import sys
import tempfile

sys.dont_write_bytecode = True
ROOT = Path(__file__).resolve().parents[2]
SPEC = importlib.util.spec_from_file_location(
    'calendar_process_owner', Path(__file__).with_name('verify-native-three-os.py'))
assert SPEC and SPEC.loader
OWNER = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(OWNER)
OWNER.MAX_CAPTURE_BYTES = 8 * 1024 * 1024

TOOLCHAIN = '1.99.1'
BUILD_ID = 'ku-native-calendar/0.1.0;abi=1;chrono=0.4.45;engine=shared'
HEADER = 'ku_native_calendar.h'
TARGETS = {
    'aarch64-apple-darwin': 'macho-arm64',
    'x86_64-unknown-linux-gnu': 'elf-x86_64',
    'aarch64-unknown-linux-gnu': 'elf-arm64',
    'x86_64-pc-windows-msvc': 'coff-x86_64',
}
SOURCE_FILES = tuple(sorted((
    'crates/ku-native-calendar/Cargo.toml',
    'crates/ku-native-calendar/build.rs',
    'crates/ku-native-calendar/src/lib.rs',
    'crates/ku-native-calendar/include/' + HEADER,
    'src/stdlib/time_calendar.rs',
)))
MAX_ARCHIVE = 128 * 1024 * 1024
MAX_TEXT = 1024 * 1024
CALENDAR_API = {b'__ku_calendar_abi_version', b'__ku_calendar_v1_build_id',
                b'__ku_calendar_call_v1', b'__ku_calendar_release_v1'}
BIGOBJ_CLASS = bytes.fromhex('c7a1bad1eebaa94baf20faf66aa4dcb8')
SYSTEM_DLLS = {b'kernel32.dll', b'advapi32.dll', b'bcrypt.dll', b'bcryptprimitives.dll',
               b'ntdll.dll', b'userenv.dll', b'ws2_32.dll', b'dbghelp.dll',
               b'synchronization.dll', b'msvcrt.dll', b'ucrtbase.dll',
               b'vcruntime140.dll', b'vcruntime140_1.dll'}


def archive_name(target):
    if target not in TARGETS:
        raise ValueError('unsupported native calendar target: ' + str(target))
    return 'ku_native_calendar.lib' if target.endswith('-msvc') else 'libku_native_calendar.a'


def plain(path, maximum):
    path = Path(path)
    info = path.lstat()
    if not stat.S_ISREG(info.st_mode) or info.st_size <= 0 or info.st_size > maximum:
        raise ValueError('expected bounded regular file: ' + str(path))
    return info


def digest(path):
    result = hashlib.sha256()
    with Path(path).open('rb') as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b''):
            result.update(chunk)
    return result.hexdigest()


def source_hash():
    result = hashlib.sha256()
    for name in SOURCE_FILES:
        path = ROOT / name
        plain(path, MAX_TEXT)
        result.update(name.encode('utf-8') + b'\0')
        result.update(path.read_bytes())
        result.update(b'\0')
    return result.hexdigest()


def record(path):
    info = plain(path, MAX_ARCHIVE)
    return {'name': Path(path).name, 'size': info.st_size, 'sha256': digest(path)}


def unique_object(pairs):
    value = {}
    for key, item in pairs:
        if key in value:
            raise ValueError('duplicate calendar manifest key')
        value[key] = item
    return value


def native_tokens(raw, target):
    if not isinstance(raw, str) or not raw or len(raw) > 4096 or '\n' in raw or '\r' in raw:
        raise ValueError('invalid native-static-libs contract')
    tokens = shlex.split(raw)
    if not tokens:
        raise ValueError('empty native-static-libs contract')
    index = 0
    while index < len(tokens):
        token = tokens[index]
        if token == '-framework' and target.endswith('-apple-darwin'):
            index += 1
            if index == len(tokens) or tokens[index] not in ('CoreFoundation', 'Security', 'SystemConfiguration'):
                raise ValueError('unsupported calendar framework contract')
        elif target.endswith('-msvc'):
            if not re.fullmatch(r'[A-Za-z0-9_]+\.lib|/defaultlib:msvcrt', token):
                raise ValueError('unsupported MSVC calendar link token: ' + token)
        elif not re.fullmatch(r'-l[A-Za-z0-9_+.-]+', token):
            raise ValueError('unsupported calendar link token: ' + token)
        index += 1
    return tokens


def system_dll(name):
    lower = name.lower()
    return (len(name) <= 1024 and re.fullmatch(rb'[A-Za-z0-9_.-]+', name) is not None
            and (lower in SYSTEM_DLLS or re.fullmatch(
                rb'(?:api|ext)-ms-win-[a-z0-9]+(?:-[a-z0-9]+)*-l[0-9]+-[0-9]+-[0-9]+\.dll', lower) is not None))


def terminated(data, offset, what):
    stop = data.find(b'\0', offset)
    if offset < 0 or stop < 0 or stop - offset > 1024:
        raise ValueError('invalid or unterminated COFF ' + what)
    return data[offset:stop]


def validate_coff(body):
    """Rust-specific COFF policy; short imports cannot define the private ABI."""
    if len(body) < 20:
        raise ValueError('truncated COFF header')
    special = body[:4] == b'\x00\x00\xff\xff'
    if special and struct.unpack_from('<H', body, 4)[0] == 0:
        # Version, not member length, distinguishes a short import from BigObj.
        if struct.unpack_from('<H', body, 6)[0] != 0x8664:
            raise ValueError('wrong-target COFF short import')
        size = struct.unpack_from('<I', body, 12)[0]
        flags = struct.unpack_from('<H', body, 18)[0]
        if size != len(body) - 20 or size > 2050:
            raise ValueError('invalid COFF short import SizeOfData')
        if flags & 3 > 2 or (flags >> 2) & 7 > 3 or flags >> 5:
            raise ValueError('unsupported COFF short import type/name flags')
        symbol = terminated(body, 20, 'import symbol')
        dll_offset = 20 + len(symbol) + 1
        dll = terminated(body, dll_offset, 'import DLL')
        if not symbol or dll_offset + len(dll) + 1 != len(body) or not system_dll(dll):
            raise ValueError('invalid COFF short import system DLL/name')
        return False, set(), False
    if special:
        if (len(body) < 56 or struct.unpack_from('<HH', body, 4)[0] < 2
                or struct.unpack_from('<H', body, 6)[0] != 0x8664 or body[12:28] != BIGOBJ_CLASS):
            raise ValueError('invalid AMD64 COFF BigObj header')
        header, sections, symbols, count, entry = 56, *struct.unpack_from('<III', body, 44), 20
    else:
        if struct.unpack_from('<H', body)[0] != 0x8664 or struct.unpack_from('<H', body, 16)[0] != 0:
            raise ValueError('wrong-target or non-native COFF member')
        header, sections, entry = 20, struct.unpack_from('<H', body, 2)[0], 18
        symbols, count = struct.unpack_from('<II', body, 8)
    if (sections > 16384 or (sections == 0 and not special)
            or header + sections * 40 > len(body) or count > 262144
            or symbols + count * entry + 4 > len(body)):
        raise ValueError('COFF sections/symbol table exceeds member bounds')
    string_offset = symbols + count * entry
    size = struct.unpack_from('<I', body, string_offset)[0]
    if not 4 <= size <= 16 * 1024 * 1024 or string_offset + size > len(body):
        raise ValueError('invalid COFF string table')
    strings = body[string_offset:string_offset+size]
    imported = any(body[header+i*40:header+i*40+8].startswith((b'.idata', b'.didat'))
                   for i in range(sections))
    defined, index = set(), 0
    while index < count:
        start = symbols + index * entry
        item = body[start:start+entry]
        name = (terminated(strings, struct.unpack_from('<I', item, 4)[0], 'symbol')
                if item[:4] == b'\0'*4 else item[:8].split(b'\0', 1)[0])
        section = struct.unpack_from('<i' if special else '<h', item, 12)[0]
        external, aux = item[entry-2] == 2, item[entry-1]
        if not imported and external and 0 < section <= sections and name in CALENDAR_API:
            if name in defined:
                raise ValueError('duplicate defined calendar API symbol')
            defined.add(name)
        if index + aux >= count:
            raise ValueError('COFF auxiliary symbol overflow')
        index += 1 + aux
    identity = False
    for index in range(sections):
        start = header + index * 40
        length, offset = struct.unpack_from('<II', body, start + 16)
        characteristics = struct.unpack_from('<I', body, start + 36)[0]
        if length and characteristics & 0x40:
            if offset + length > len(body):
                raise ValueError('COFF section data exceeds member bounds')
            if not imported and BUILD_ID.encode() in body[offset:offset+length]:
                identity = True
    return True, defined, identity


def validate_archive(path, target):
    """Inspect every native code member, including Rust's dependency objects."""
    plain(path, MAX_ARCHIVE)
    data = Path(path).read_bytes()
    if not data.startswith(b'!<arch>\n') or BUILD_ID.encode() not in data:
        raise ValueError('calendar archive lacks its expected container/build identity')
    offset, objects, members = 8, 0, 0
    windows_definitions, windows_identity = set(), False
    while offset < len(data):
        if offset + 60 > len(data) or data[offset+58:offset+60] != b'`\n':
            raise ValueError('invalid calendar archive member')
        name = data[offset:offset+16].strip()
        size = int(data[offset+48:offset+58])
        start, stop = offset + 60, offset + 60 + size
        if size < 0 or stop > len(data):
            raise ValueError('truncated calendar archive member')
        body = data[start:stop]
        offset = stop + (size & 1)
        if name in (b'/', b'//', b'/SYM64/'):
            continue
        if name.startswith(b'#1/'):
            length = int(name[3:])
            if not 0 < length <= len(body):
                raise ValueError('invalid BSD archive member name')
            name, body = body[:length].rstrip(b'\0'), body[length:]
        if name.startswith(b'__.SYMDEF'):
            continue
        members += 1
        if members > 16384:
            raise ValueError('calendar archive exceeds its member bound')
        if target == 'aarch64-apple-darwin':
            valid = (len(body) >= 32 and body[:4] == b'\xcf\xfa\xed\xfe'
                     and struct.unpack_from('<I', body, 4)[0] == 0x100000c
                     and struct.unpack_from('<I', body, 12)[0] == 1)
        elif target.endswith('linux-gnu'):
            machine = 183 if target.startswith('aarch64-') else 62
            valid = (len(body) >= 64 and body[:6] == b'\x7fELF\x02\x01'
                     and struct.unpack_from('<HH', body, 16) == (1, machine))
        else:
            native, defined, identity = validate_coff(body)
            if windows_definitions & defined:
                raise ValueError('duplicate defined calendar API symbol')
            windows_definitions.update(defined)
            windows_identity |= identity
            if not native:
                continue
            valid = True
        if not valid:
            raise ValueError('wrong-target or non-native calendar member: ' + repr(name))
        objects += 1
        if objects > 16384:
            raise ValueError('calendar archive exceeds its native member bound')
    if offset != len(data) or objects == 0:
        raise ValueError('calendar archive has no complete native objects')
    if target.endswith('-msvc') and (windows_definitions != CALENDAR_API or not windows_identity):
        raise ValueError('calendar archive lacks defined ABI symbols or native object build identity')
    return objects


def validate_pack(leaf, target, expected_source=None):
    """Read-only validation shared by the SDK exporter and this builder."""
    leaf = Path(leaf)
    if leaf.is_symlink() or not leaf.is_dir():
        raise ValueError('calendar pack root must be a real directory')
    expected_files = {'manifest.json', 'include/' + HEADER,
                      'lib/' + archive_name(target), 'LICENSE.ku', 'LICENSE.chrono'}
    actual_files = set()
    for path in leaf.rglob('*'):
        if path.is_symlink():
            raise ValueError('calendar pack may not contain links')
        if path.is_file():
            actual_files.add(path.relative_to(leaf).as_posix())
        elif not path.is_dir() or path.relative_to(leaf).as_posix() not in ('include', 'lib'):
            raise ValueError('unexpected calendar pack entry')
    if actual_files != expected_files:
        raise ValueError('calendar pack must contain exactly its five declared files')
    plain(leaf/'manifest.json', MAX_TEXT)
    manifest = json.loads((leaf/'manifest.json').read_text(encoding='utf-8'), object_pairs_hook=unique_object)
    expected = {'schema_version': 1, 'abi_version': 1, 'build_id': BUILD_ID,
                'crate_version': '0.1.0', 'chrono_version': '0.4.45',
                'rustc_version': TOOLCHAIN, 'target': target}
    if (not isinstance(manifest, dict)
            or any(type(manifest.get(k)) is not type(v) or manifest.get(k) != v
                   for k, v in expected.items())):
        raise ValueError('calendar manifest differs from current source/ABI/toolchain')
    if not re.fullmatch(r'[a-f0-9]{64}', manifest.get('source_sha256', '')):
        raise ValueError('calendar manifest lacks a source fingerprint')
    if expected_source is not None and manifest['source_sha256'] != expected_source:
        raise ValueError('calendar manifest differs from the expected source fingerprint')
    verbose = manifest.get('rustc_verbose')
    if not isinstance(verbose, str) or not re.search(r'^release: 1\.99\.1$', verbose, re.M):
        raise ValueError('calendar manifest lacks the exact actual Rust release identity')
    libraries = manifest.get('native_static_libs')
    if not isinstance(libraries, dict) or libraries.get('tokens') != native_tokens(libraries.get('raw'), target):
        raise ValueError('calendar native link contract is inconsistent')
    entries = [(manifest.get('archive'), leaf/'lib', archive_name(target)),
               (manifest.get('header'), leaf/'include', HEADER)]
    licenses = manifest.get('licenses')
    if not isinstance(licenses, list) or [row.get('name') for row in licenses if isinstance(row, dict)] != ['LICENSE.ku', 'LICENSE.chrono']:
        raise ValueError('calendar license inventory is invalid')
    entries.extend((row, leaf, row['name']) for row in licenses)
    for row, directory, name in entries:
        if not isinstance(row, dict) or row.get('name') != name or row != record(directory/name):
            raise ValueError('calendar pack file identity mismatch: ' + name)
    if expected_source is not None and digest(leaf/'include'/HEADER) != digest(ROOT/'crates/ku-native-calendar/include'/HEADER):
        raise ValueError('calendar header differs from current ABI header')
    if expected_source is not None and digest(leaf/'LICENSE.ku') != digest(ROOT/'LICENSE'):
        raise ValueError('calendar Ku license differs from its source')
    validate_archive(leaf/'lib'/archive_name(target), target)
    return manifest


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


def run(command, env, timeout=900):
    OWNER.COMMAND_TIMEOUT_SECONDS = timeout
    print('calendar build:', ' '.join(str(part) for part in command), flush=True)
    with environment(env):
        result = OWNER.run_bounded([str(part) for part in command], ROOT, 'private calendar pack build', check=False)
    if result.returncode:
        detail = (result.stdout + result.stderr).decode(errors='replace')[-12000:]
        raise RuntimeError('calendar build command failed: ' + detail)
    return result


def dependency_licenses(metadata):
    """Preserve licenses of the actual selected crate dependency closure."""
    packages = {item['id']: item for item in metadata['packages']}
    nodes = {item['id']: item for item in metadata['resolve']['nodes']}
    roots = [key for key, item in packages.items() if item['name'] == 'ku-native-calendar']
    if len(roots) != 1:
        raise ValueError('metadata did not identify one native calendar crate')
    pending, seen = [roots[0]], set()
    while pending:
        node = pending.pop()
        if node in seen:
            continue
        seen.add(node)
        pending.extend(item['pkg'] for item in nodes[node]['deps'])
    chunks = []
    for key in sorted(seen, key=lambda item: (packages[item]['name'], packages[item]['version'])):
        package = packages[key]
        if package['name'] == 'ku-native-calendar':
            continue
        directory = Path(package['manifest_path']).parent
        candidates = sorted(path for path in directory.iterdir()
                            if path.is_file() and re.fullmatch(r'(?:LICENSE|LICENCE|COPYING)(?:[._-].+)?', path.name, re.I))
        if package.get('license_file'):
            explicit = directory/package['license_file']
            if explicit not in candidates:
                candidates.append(explicit)
        if not candidates:
            raise ValueError('missing license text for calendar dependency: ' + package['name'])
        chunks.append(('\n===== ' + package['name'] + ' ' + package['version'] + ' =====\n').encode())
        for path in candidates:
            plain(path, MAX_TEXT)
            chunks.append(('\n--- ' + path.name + ' ---\n').encode())
            chunks.append(path.read_bytes())
    result = b''.join(chunks)
    if not result or len(result) > MAX_TEXT:
        raise ValueError('calendar dependency licenses exceed their bound')
    return result


def build(target, output, toolchain=TOOLCHAIN, target_dir=None):
    if toolchain != TOOLCHAIN:
        raise ValueError('native calendar requires frozen Rust ' + TOOLCHAIN)
    output = Path(output).absolute()
    if output.exists() or output.is_symlink():
        return validate_pack(output, target, source_hash())
    target_dir = Path(target_dir or ROOT/'target').resolve()
    if output == target_dir or target_dir in output.parents or output in target_dir.parents:
        raise ValueError('calendar pack and build cache must be separate directory trees')
    env = dict(os.environ)
    if any(env.get(key) for key in ('RUSTFLAGS', 'CARGO_ENCODED_RUSTFLAGS', 'RUSTC_WRAPPER', 'RUSTC_WORKSPACE_WRAPPER')):
        raise ValueError('calendar pack requires an unwrapped compiler without ambient Rust flags')
    env['CARGO_PROFILE_RELEASE_PANIC'] = 'unwind'
    env['CARGO_BUILD_JOBS'] = '1'
    if target.endswith('-apple-darwin'):
        env['MACOSX_DEPLOYMENT_TARGET'] = '11.0'
    verbose = run(['rustc', '+'+toolchain, '-Vv'], env, 60).stdout.decode('utf-8')
    if not re.search(r'^release: 1\.99\.1$', verbose, re.M):
        raise ValueError('the actual compiler is not the frozen release')
    before = source_hash()
    metadata = json.loads(run(['cargo', '+'+toolchain, 'metadata', '--locked', '--offline',
                              '--format-version=1', '--filter-platform', target], env, 120).stdout)
    license_text = dependency_licenses(metadata)
    command = ['cargo', '+'+toolchain, 'rustc', '--locked', '--offline', '--release',
               '--message-format=json-render-diagnostics', '--color', 'never',
               '--manifest-path', ROOT/'Cargo.toml', '--target-dir', target_dir,
               '-p', 'ku-native-calendar', '--target', target, '--', '--print', 'native-static-libs']
    result = run(command, env)
    diagnostics = [result.stderr.decode('utf-8', errors='replace')]
    artifacts = []
    for line in result.stdout.decode('utf-8').splitlines():
        try:
            item = json.loads(line)
        except json.JSONDecodeError:
            diagnostics.append(line)
            continue
        if item.get('reason') == 'compiler-message':
            diagnostics.extend((item['message'].get('rendered') or '', item['message'].get('message') or ''))
        if item.get('reason') == 'compiler-artifact' and item['target']['name'] == 'ku_native_calendar':
            artifacts.extend(Path(name) for name in item['filenames'] if Path(name).name == archive_name(target))
    libs = set(re.findall(r'(?m)^\s*(?:note:\s*)?native-static-libs:\s*([^\r\n]+)', '\n'.join(diagnostics)))
    if len(libs) != 1:
        raise ValueError('compiler did not report one actual native-static-libs contract')
    raw = libs.pop().strip()
    tokens = native_tokens(raw, target)
    artifacts = sorted(set(path.resolve() for path in artifacts))
    if len(artifacts) != 1 or artifacts[0] != (target_dir/target/'release'/archive_name(target)).resolve():
        raise ValueError('cargo did not identify one matching target Release static archive')
    objects = validate_archive(artifacts[0], target)
    if source_hash() != before:
        raise ValueError('calendar source changed during the build; refusing mixed identity')
    output.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix='.calendar-stage-', dir=output.parent) as name:
        stage = Path(name)
        (stage/'include').mkdir()
        (stage/'lib').mkdir()
        shutil.copyfile(artifacts[0], stage/'lib'/archive_name(target))
        shutil.copyfile(ROOT/'crates/ku-native-calendar/include'/HEADER, stage/'include'/HEADER)
        shutil.copyfile(ROOT/'LICENSE', stage/'LICENSE.ku')
        (stage/'LICENSE.chrono').write_bytes(license_text)
        manifest = {'schema_version': 1, 'abi_version': 1, 'build_id': BUILD_ID,
                    'crate_version': '0.1.0', 'chrono_version': '0.4.45',
                    'rustc_version': toolchain, 'rustc_verbose': verbose, 'target': target,
                    'source_sha256': before, 'archive': record(stage/'lib'/archive_name(target)),
                    'header': record(stage/'include'/HEADER),
                    'native_static_libs': {'raw': raw, 'tokens': tokens},
                    'licenses': [record(stage/'LICENSE.ku'), record(stage/'LICENSE.chrono')],
                    'build': {'command': [str(part) for part in command], 'profile': 'release',
                              'panic': 'unwind', 'native_object_members': objects,
                              'macos_deployment_target': '11.0' if target.endswith('-apple-darwin') else None}}
        (stage/'manifest.json').write_text(json.dumps(manifest, indent=2)+'\n', encoding='utf-8')
        for path in stage.rglob('*'):
            path.chmod(0o755 if path.is_dir() else 0o644)
        stage.chmod(0o755)
        validate_pack(stage, target, before)
        if output.exists() or output.is_symlink():
            raise ValueError('calendar output appeared during build; refusing overwrite')
        stage.rename(output)
    print('Built native calendar pack:', output, flush=True)
    print('native-static-libs:', raw, flush=True)
    return manifest


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--target', choices=TARGETS, required=True)
    parser.add_argument('--output', type=Path, required=True, help='immutable target pack leaf')
    parser.add_argument('--toolchain', default=TOOLCHAIN)
    parser.add_argument('--target-dir', type=Path)
    parser.add_argument('--verify-only', action='store_true', help='validate an existing pack without constructing one')
    args = parser.parse_args()
    if args.toolchain != TOOLCHAIN:
        raise ValueError('native calendar requires frozen Rust ' + TOOLCHAIN)
    if args.verify_only:
        validate_pack(args.output, args.target, source_hash())
    else:
        build(args.target, args.output, args.toolchain, args.target_dir)


if __name__ == '__main__':
    try:
        main()
    except (ValueError, OSError, RuntimeError, KeyError, TypeError) as error:
        print('native calendar pack:', error, file=sys.stderr)
        raise SystemExit(1)
