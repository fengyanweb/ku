#!/usr/bin/env python3
"""Build pinned optional compression packs without installing system software.

Supply verified upstream source archives with --zlib-source/--zstd-source, or
place them in ROOT/sources. There is no implicit network access. POSIX builds
use upstream configure/make; Windows uses an existing CMake/MSVC toolchain.
Only private temporary prefixes are installed. Completed immutable packs are
reused after their hashes, headers and native object architecture are checked.
"""
from __future__ import annotations

import argparse
from contextlib import contextmanager
import hashlib
import http.client
import importlib.util
import io
import json
import os
from pathlib import Path, PurePosixPath
import platform
import re
import shutil
import stat
import struct
import sys
import tarfile
import tempfile
import time
import urllib.error
import urllib.parse
import urllib.request

sys.dont_write_bytecode = True
SPEC = importlib.util.spec_from_file_location('compression_build_owner', Path(__file__).with_name('verify-native-three-os.py'))
assert SPEC and SPEC.loader
OWNER = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(OWNER)
OWNER.MAX_CAPTURE_BYTES = 2 * 1024 * 1024

SOURCES = {
    'zlib': {'version': '1.4', 'filename': 'zlib-1.4.tar.gz',
             'url': 'https://zlib.net/zlib-1.4.tar.gz',
             'sha256': '02dd159783677a749c04adbdd02b55b8a2bcc7e054f51b76b549199a916ad584',
             'license': 'Zlib'},
    'zstd': {'version': '1.5.7', 'filename': 'zstd-1.5.7.tar.gz',
             'url': 'https://github.com/facebook/zstd/releases/download/v1.5.7/zstd-1.5.7.tar.gz',
             'sha256': 'eb33e51f49a15e023950cd7825ca74a4a2b43db8354825ac24fc1b7ee09e6fa3',
             'checksum_url': 'https://github.com/facebook/zstd/releases/download/v1.5.7/zstd-1.5.7.tar.gz.sha256',
             'license': 'BSD-3-Clause'},
}
TARGETS = {
    'aarch64-apple-darwin': ('Darwin', ('arm64', 'aarch64'), 'macho-arm64', 'apple', 'system-dynamic'),
    'x86_64-unknown-linux-gnu': ('Linux', ('x86_64', 'AMD64'), 'elf-x86_64', 'gnu', 'system-dynamic'),
    'aarch64-unknown-linux-gnu': ('Linux', ('aarch64', 'ARM64', 'arm64'), 'elf-arm64', 'gnu', 'system-dynamic'),
    'x86_64-pc-windows-msvc': ('Windows', ('AMD64', 'x86_64'), 'coff-x86_64', 'msvc', 'msvc-dynamic'),
}
MAX_SOURCE_ARCHIVE = 16 * 1024 * 1024
MAX_SOURCE_BYTES = 128 * 1024 * 1024
MAX_NATIVE_ARCHIVE = 128 * 1024 * 1024
HEADER_NAMES = {'zlib': ('zlib.h', 'zconf.h'), 'zstd': ('zstd.h', 'zstd_errors.h')}


def unique_object(pairs):
    result = {}
    for key, value in pairs:
        if key in result:
            raise ValueError('compression manifest contains a duplicate JSON key')
        result[key] = value
    return result


def fetch_source(root, codec):
    """Explicit, bounded retrieval of one pinned public source archive.

    Nothing is installed globally and neither a bad cache entry nor a partial
    download can silently replace the published source identity.
    """
    spec = SOURCES[codec]
    sources = root / 'sources'
    sources.mkdir(parents=True, exist_ok=True)
    destination = sources / spec['filename']
    if destination.exists() or destination.is_symlink():
        plain(destination, MAX_SOURCE_ARCHIVE)
        if digest(destination) != spec['sha256']:
            raise ValueError('cached ' + codec + ' source SHA256 differs; refusing overwrite')
        return destination
    hosts = {'github.com', 'release-assets.githubusercontent.com', 'objects.githubusercontent.com'}
    if codec == 'zlib':
        hosts |= {'zlib.net', 'www.zlib.net'}

    def allowed(url):
        parsed = urllib.parse.urlsplit(url)
        if (parsed.scheme != 'https' or parsed.hostname not in hosts
                or parsed.username is not None or parsed.password is not None
                or parsed.port not in (None, 443)):
            raise ValueError('compression source redirect left its official HTTPS hosts')

    class OfficialRedirects(urllib.request.HTTPRedirectHandler):
        max_redirections = 5
        max_repeats = 2

        def redirect_request(self, request, fp, code, message, headers, new_url):
            allowed(new_url)
            return super().redirect_request(request, fp, code, message, headers, new_url)

    # Use the upstream project's published release asset, not GitHub's
    # automatically generated source tarball. Both origins require one pin.
    urls = (spec['url'], 'https://github.com/madler/zlib/releases/download/v1.4/zlib-1.4.tar.gz') if codec == 'zlib' else (spec['url'],)
    attempts = []
    for url in urls:
        allowed(url)
        opener = urllib.request.build_opener(OfficialRedirects())
        partial = None
        total, checksum, magic = 0, hashlib.sha256(), bytearray()
        complete, status, content_type, encoding, final_host = False, None, None, None, None
        reason = 'transport_error'

        def receipt():
            # Fixed public request URLs and the final hostname only. A signed
            # redirect query, response body, headers or credentials are never
            # printed. Hex magic is explicitly limited to the first 16 bytes.
            return {'codec': codec, 'source_url': url, 'final_host': final_host,
                    'status': status, 'size': total, 'actual_sha256': checksum.hexdigest(),
                    'expected_sha256': spec['sha256'],
                    'sha256_scope': 'complete_response' if complete else 'received_prefix',
                    'content_type': content_type, 'content_encoding': encoding,
                    'magic16_hex': bytes(magic).hex()}

        try:
            with tempfile.NamedTemporaryFile(prefix='.' + spec['filename'] + '.', suffix='.partial', dir=sources, delete=False) as copied:
                partial = Path(copied.name)
                try:
                    deadline = time.monotonic() + 120
                    request = urllib.request.Request(url, headers={'User-Agent': 'Ku-native-compression-builder/1'})
                    with opener.open(request, timeout=30) as response:
                        status = response.status
                        content_type = str(response.headers.get('Content-Type', ''))[:128]
                        encoding = str(response.headers.get('Content-Encoding', ''))[:128]
                        reason = 'unapproved_redirect'
                        allowed(response.geturl())
                        final_host = urllib.parse.urlsplit(response.geturl()).hostname
                        reason = 'http_status'
                        if status != 200:
                            raise ValueError('compression source download did not return HTTP 200')
                        reason = 'size_bound'
                        length = response.headers.get('Content-Length')
                        if length is not None and not 0 < int(length) <= MAX_SOURCE_ARCHIVE:
                            raise ValueError('compression source download exceeds its size bound')
                        while True:
                            reason = 'timeout'
                            if time.monotonic() >= deadline:
                                raise TimeoutError('compression source download exceeded 120 seconds')
                            reason = 'transport_error'
                            block = response.read(64 * 1024)
                            if not block:
                                complete = True
                                break
                            total += len(block)
                            checksum.update(block)
                            magic.extend(block[:16-len(magic)])
                            reason = 'size_bound'
                            if total > MAX_SOURCE_ARCHIVE:
                                raise ValueError('compression source download exceeds its size bound')
                            copied.write(block)
                        reason = 'length_mismatch'
                        if length is not None and total != int(length):
                            raise ValueError('compression source download length differs from HTTP metadata')
                    reason = 'sha256_mismatch'
                    if not total or checksum.hexdigest() != spec['sha256']:
                        raise ValueError('compression source SHA256 differs from its pinned official release')
                except (ValueError, OSError, urllib.error.URLError, http.client.HTTPException) as error:
                    if isinstance(error, urllib.error.HTTPError):
                        status = error.code
                        content_type = str(error.headers.get('Content-Type', ''))[:128]
                        encoding = str(error.headers.get('Content-Encoding', ''))[:128]
                        reason = 'http_status'
                        error.close()
                    elif isinstance(error, TimeoutError):
                        reason = 'timeout'
                    failed = dict(receipt(), failure=reason, error_type=type(error).__name__)
                    attempts.append(failed)
                    print('Compression source attempt failed: ' + json.dumps(failed, sort_keys=True), flush=True)
                    continue
            partial.chmod(0o644)
            # Publishing is outside the retry handler: a concurrent cache file
            # or filesystem error must not be mistaken for a download failure.
            os.link(partial, destination)
            print('Fetched verified official compression source: ' + json.dumps(receipt(), sort_keys=True), flush=True)
            return destination
        finally:
            if partial is not None:
                partial.unlink(missing_ok=True)
    raise ValueError('pinned official compression sources failed: ' + json.dumps(attempts, sort_keys=True))


def digest(path):
    result = hashlib.sha256()
    with Path(path).open('rb') as stream:
        for block in iter(lambda: stream.read(1024 * 1024), b''):
            result.update(block)
    return result.hexdigest()


def plain(path, maximum):
    info = Path(path).lstat()
    if not stat.S_ISREG(info.st_mode) or getattr(info, 'st_file_attributes', 0) & 0x400 or not 0 < info.st_size <= maximum:
        raise ValueError('expected a bounded plain file: ' + str(path))
    return info


def host_target(target):
    system, machines, _, _, _ = TARGETS[target]
    if platform.system() != system or platform.machine() not in machines:
        raise ValueError('compression builder requires a matching native host/toolchain for ' + target + '; it does not relabel cross-built artifacts')


def archive_name(codec, target):
    if target.endswith('windows-msvc'):
        return 'libzs.lib' if codec == 'zlib' else 'zstd_static.lib'
    return 'libz.a' if codec == 'zlib' else 'libzstd.a'


def extract_source(archive, destination, codec):
    """Snapshot and verify the small tarball before any source is extracted."""
    spec = SOURCES[codec]
    archive = Path(archive)
    plain(archive, MAX_SOURCE_ARCHIVE)
    data = archive.read_bytes()
    if len(data) > MAX_SOURCE_ARCHIVE or hashlib.sha256(data).hexdigest() != spec['sha256']:
        raise ValueError(codec + ' source archive SHA256 differs from the pinned official release')
    prefix = codec + '-' + spec['version']
    total, seen, omitted = 0, set(), []
    with tarfile.open(fileobj=io.BytesIO(data), mode='r:gz') as incoming:
        for count, member in enumerate(incoming, 1):
            if count > 20000:
                raise ValueError('compression source archive exceeds its entry bound')
            name = member.name.rstrip('/')
            parts = PurePosixPath(name).parts
            if (not parts or parts[0] != prefix or '\\' in name or ':' in name
                    or PurePosixPath(name).is_absolute() or any(p in ('.', '..') for p in parts)):
                raise ValueError('compression source archive contains an unexpected path')
            folded = name.casefold() if os.name == 'nt' else name
            if folded in seen:
                raise ValueError('compression source archive contains a duplicate path')
            seen.add(folded)
            output = destination.joinpath(*parts)
            if member.isdir():
                output.mkdir(parents=True, exist_ok=True)
                continue
            if member.issym() or member.islnk():
                # Upstream test/manpage aliases are not build inputs. Never
                # create links from an archive, including on Windows.
                if len(parts) > 1 and parts[1] in ('tests', 'doc', 'programs', 'contrib', '.github'):
                    omitted.append(name)
                    continue
                raise ValueError('compression library source contains an unsupported link')
            if not member.isfile() or member.size < 0:
                raise ValueError('compression source archive contains a non-file entry')
            total += member.size
            if total > MAX_SOURCE_BYTES:
                raise ValueError('compression source archive exceeds its expanded-size bound')
            output.parent.mkdir(parents=True, exist_ok=True)
            with incoming.extractfile(member) as source, output.open('xb') as copied:
                shutil.copyfileobj(source, copied, length=1024 * 1024)
            if output.stat().st_size != member.size:
                raise ValueError('compression source file length changed')
            output.chmod(0o755 if member.mode & 0o111 else 0o644)
    return destination / prefix, omitted


def validate_archive(path, target):
    """Check every code member, not just the static archive's filename."""
    plain(path, MAX_NATIVE_ARCHIVE)
    data = Path(path).read_bytes()
    if not data.startswith(b'!<arch>\n'):
        raise ValueError('compression library must be a normal static archive')
    offset, objects = 8, 0
    while offset < len(data):
        if offset + 60 > len(data) or data[offset+58:offset+60] != b'`\n':
            raise ValueError('compression archive member header is invalid')
        name = data[offset:offset+16].strip()
        try:
            size = int(data[offset+48:offset+58])
        except ValueError as error:
            raise ValueError('compression archive member size is invalid') from error
        start, stop = offset+60, offset+60+size
        if size < 0 or stop > len(data):
            raise ValueError('compression archive member is truncated')
        body = data[start:stop]
        offset = stop + (size & 1)
        if name in (b'/', b'//', b'/SYM64/'):
            continue
        if name.startswith(b'#1/'):
            length = int(name[3:])
            if not 0 < length <= len(body):
                raise ValueError('BSD archive extended filename is invalid')
            name, body = body[:length].rstrip(b'\0'), body[length:]
        if name.startswith(b'__.SYMDEF'):
            continue
        if target == 'aarch64-apple-darwin':
            matches = len(body) >= 32 and body[:4] == b'\xcf\xfa\xed\xfe' and struct.unpack_from('<I', body, 4)[0] == 0x100000c and struct.unpack_from('<I', body, 12)[0] == 1
        elif target.endswith('linux-gnu'):
            machine = 183 if target.startswith('aarch64-') else 62
            matches = len(body) >= 64 and body[:6] == b'\x7fELF\x02\x01' and struct.unpack_from('<HH', body, 16) == (1, machine)
        else:
            matches = len(body) >= 20 and struct.unpack_from('<H', body)[0] == 0x8664 and struct.unpack_from('<H', body, 16)[0] == 0
            if len(body) >= 56 and body[:4] == b'\x00\x00\xff\xff':
                matches = struct.unpack_from('<HH', body, 4) == (2, 0x8664)  # BigObj, not an import stub.
        if not matches:
            raise ValueError('compression archive contains a wrong-target or non-native object: ' + repr(name))
        objects += 1
    if offset != len(data) or not objects:
        raise ValueError('compression archive has no complete native code members')
    return objects


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


def run(command, cwd, env, timeout=600):
    print('compression build:', ' '.join(str(p) for p in command), flush=True)
    OWNER.COMMAND_TIMEOUT_SECONDS = timeout
    with environment(env):
        result = OWNER.run_bounded([str(p) for p in command], cwd, 'private compression dependency build', check=False)
    if result.returncode:
        text = (result.stdout + result.stderr).decode(errors='replace')[-8000:]
        raise RuntimeError('compression build command failed: ' + text)
    return result


def record_file(path):
    return {'name': Path(path).name, 'size': plain(path, MAX_NATIVE_ARCHIVE).st_size, 'sha256': digest(path)}


def check_headers(directory, codec):
    text = (directory / HEADER_NAMES[codec][0]).read_text(encoding='utf-8')
    if codec == 'zlib':
        match = re.search(r'^\s*#\s*define\s+ZLIB_VERSION\s+"([^"]+)"', text, re.M)
        version = match[1] if match else None
    else:
        values = [re.search(r'^\s*#\s*define\s+ZSTD_VERSION_' + part + r'\s+(\d+)', text, re.M) for part in ('MAJOR', 'MINOR', 'RELEASE')]
        version = '.'.join(value[1] for value in values) if all(values) else None
    if version != SOURCES[codec]['version']:
        raise ValueError(codec + ' header version differs from the frozen library')


def validate_pack(leaf, codec, target):
    path = leaf / 'manifest.json'
    plain(path, 64 * 1024)
    manifest = json.loads(path.read_text(encoding='utf-8'), object_pairs_hook=unique_object)
    if not isinstance(manifest, dict):
        raise ValueError('compression pack manifest must be a JSON object')
    _, _, object_format, abi, crt = TARGETS[target]
    expected = {'format': 'ku-native-compression-pack-v1', 'codec': codec, 'version': SOURCES[codec]['version'],
                'target': target, 'object_format': object_format, 'abi': abi, 'crt': crt, 'runtime_dependency': 'none'}
    if (any(manifest.get(k) != v for k, v in expected.items())
            or manifest.get('source', {}).get('sha256') != SOURCES[codec]['sha256']
            or manifest.get('source', {}).get('url') != SOURCES[codec]['url']):
        raise ValueError('existing compression pack identity differs; refusing to overwrite it')
    if manifest.get('archive', {}).get('name') != archive_name(codec, target):
        raise ValueError('existing compression pack static archive name differs')
    if [item.get('name') for item in manifest.get('headers', [])] != list(HEADER_NAMES[codec]):
        raise ValueError('existing compression pack header inventory differs')
    if (manifest.get('license', {}).get('name') != 'LICENSE'
            or manifest['license'].get('spdx') != SOURCES[codec]['license']):
        raise ValueError('existing compression pack is missing its original license')
    build = manifest.get('build')
    if (not isinstance(build, dict) or build.get('profile') != 'Release'
            or build.get('optimization') != 'O2' or build.get('multithread') is not False):
        raise ValueError('compression pack must identify the normal O2 single-threaded static build')
    for row, directory in [(manifest['archive'], leaf/'lib'), *[(row, leaf/'include') for row in manifest['headers']], (manifest['license'], leaf)]:
        if row.get('name') not in (archive_name(codec, target), *HEADER_NAMES[codec], 'LICENSE'):
            raise ValueError('compression pack file has an invalid name')
        current = record_file(directory / row['name'])
        if any(current[key] != row.get(key) for key in ('name', 'size', 'sha256')):
            raise ValueError('compression pack contains missing, changed or mixed files')
    check_headers(leaf/'include', codec)
    validate_archive(leaf/'lib'/manifest['archive']['name'], target)
    return manifest


def build_one(root, target, codec, archive, cc=None, cmake=None):
    leaf = root/'native-compression/v1'/target/codec
    if leaf.exists():
        manifest = validate_pack(leaf, codec, target)
        print('Verified reusable optional compression pack:', leaf, flush=True)
        return manifest
    spec = SOURCES[codec]
    archive = Path(archive) if archive else root/'sources'/spec['filename']
    if not archive.is_absolute() or not archive.is_file():
        raise ValueError('provide --' + codec + '-source with the official archive ' + spec['url'] + ' (SHA256 ' + spec['sha256'] + '); no source download is implicit')
    # Keep upstream make's own build/prefix paths free of spaces. Final SDK
    # paths are copied with Python and can contain spaces and Unicode.
    with tempfile.TemporaryDirectory(prefix='ku-native-compression-', dir=None if os.name == 'nt' else '/tmp') as temporary:
        work = Path(temporary).resolve()
        source, omitted_links = extract_source(archive, work, codec)
        prefix = work/'private-prefix'
        env = dict(os.environ)
        for name in ('MAKEFLAGS', 'MFLAGS', 'CFLAGS', 'CXXFLAGS', 'CPPFLAGS', 'LDFLAGS', 'CHOST', 'CROSS_COMPILE', 'CL', '_CL_'):
            env.pop(name, None)
        env['CMAKE_BUILD_PARALLEL_LEVEL'] = '1'
        env['MSBUILDDISABLENODEREUSE'] = '1'
        if os.name != 'nt':
            compiler = Path(cc or shutil.which('clang') or shutil.which('cc') or '')
            if not compiler.is_absolute() or not compiler.is_file():
                raise ValueError('a preinstalled host C compiler is required; use --cc with its absolute executable path')
            # zlib's configure treats CC as a shell word. An owned short link
            # supports compiler paths containing spaces without shell injection.
            proxy = work/'cc'
            proxy.symlink_to(compiler)
            env.update(CC=str(proxy), CFLAGS='-O2 -fPIC', CXXFLAGS='-O2 -fPIC', CPPFLAGS='', LDFLAGS='')
            if target == 'aarch64-apple-darwin':
                env.setdefault('MACOSX_DEPLOYMENT_TARGET', '11.0')
            make = Path(shutil.which('make') or '')
            if not make.is_absolute() or not make.is_file():
                raise ValueError('an existing make is required for private POSIX compression builds')
            compiler_version = run([compiler, '--version'], work, env, 30).stdout.decode(errors='replace')[:4096]
            if codec == 'zlib':
                run(['/bin/sh', 'configure', '--static', '--prefix=' + str(prefix)], source, env, 60)
                run([make, '-j1', 'libz.a'], source, env)
                run([make, '-j1', 'install'], source, env, 60)
                library = prefix/'lib/libz.a'
                headers = prefix/'include'
            else:
                run([make, '-j1', 'libzstd.a', 'CFLAGS=-O2 -fPIC'], source/'lib', env)
                library, headers = source/'lib/libzstd.a', source/'lib'
            method = 'upstream configure/make' if codec == 'zlib' else 'upstream single-threaded static make'
        else:
            cmake = Path(cmake or shutil.which('cmake') or '')
            if not cmake.is_absolute() or not cmake.is_file():
                raise ValueError('an existing CMake >=3.15 and MSVC C/C++ tools are required; no tool installation is implicit')
            build = work/'cmake-build'
            project = source if codec == 'zlib' else source/'build/cmake'
            command = [cmake, '-S', project, '-B', build, '-DCMAKE_BUILD_TYPE=Release',
                       '-DCMAKE_C_FLAGS_RELEASE=/O2 /DNDEBUG', '-DCMAKE_CXX_FLAGS_RELEASE=/O2 /DNDEBUG',
                       '-DCMAKE_POLICY_DEFAULT_CMP0091=NEW', '-DCMAKE_MSVC_RUNTIME_LIBRARY=MultiThreadedDLL',
                       '-DCMAKE_INTERPROCEDURAL_OPTIMIZATION=OFF', '-DCMAKE_INSTALL_PREFIX='+str(prefix)]
            if 'Ninja' not in env.get('CMAKE_GENERATOR', ''):
                command += ['-A', 'x64']
            if cc:
                if not Path(cc).is_absolute() or not Path(cc).is_file():
                    raise ValueError('--cc must name an existing absolute compiler executable')
                command += ['-DCMAKE_C_COMPILER='+str(Path(cc).resolve())]
            if codec == 'zlib':
                command += ['-DZLIB_BUILD_STATIC=ON', '-DZLIB_BUILD_SHARED=OFF', '-DZLIB_BUILD_TESTING=OFF']
                build_target = 'zlibstatic'
            else:
                command += ['-DZSTD_BUILD_STATIC=ON', '-DZSTD_BUILD_SHARED=OFF', '-DZSTD_BUILD_PROGRAMS=OFF',
                            '-DZSTD_BUILD_TESTS=OFF', '-DZSTD_BUILD_CONTRIB=OFF', '-DZSTD_MULTITHREAD_SUPPORT=OFF',
                            '-DZSTD_USE_STATIC_RUNTIME=OFF', '-DACTIVATE_MULTITHREADED_COMPILATION=OFF']
                build_target = 'libzstd_static'
            run(command, work, env, 120)
            run([cmake, '--build', build, '--config', 'Release', '--target', build_target, '--parallel', '1'], work, env)
            matches = list(build.rglob(archive_name(codec, target)))
            if len(matches) != 1:
                raise ValueError('expected exactly one upstream Release static library, found ' + str(matches))
            library = matches[0]
            headers = build if codec == 'zlib' else source/'lib'
            information = list(build.glob('CMakeFiles/*/CMakeCCompiler.cmake'))
            if len(information) != 1:
                raise ValueError('CMake did not provide one actual compiler identity')
            information = information[0].read_text(encoding='utf-8')
            compiler_version = '\n'.join(line for line in information.splitlines() if line.startswith(('set(CMAKE_C_COMPILER ', 'set(CMAKE_C_COMPILER_ID ', 'set(CMAKE_C_COMPILER_VERSION ', 'set(CMAKE_C_COMPILER_ARCHITECTURE_ID ')))
            if cc:
                used = re.search(r'set\(CMAKE_C_COMPILER "([^"]+)"\)', information)
                if not used or not os.path.samefile(used[1], cc):
                    raise ValueError('CMake did not use the explicitly requested compiler')
            method = 'upstream CMake static target /MD; parallel=1; /MP disabled'
        objects = validate_archive(library, target)
        leaf.parent.mkdir(parents=True, exist_ok=True)
        with tempfile.TemporaryDirectory(prefix='.'+codec+'-staging-', dir=leaf.parent) as stage_directory:
            stage = Path(stage_directory)
            (stage/'include').mkdir()
            (stage/'lib').mkdir()
            shutil.copyfile(library, stage/'lib'/archive_name(codec, target))
            for name in HEADER_NAMES[codec]:
                header = source/name if codec == 'zlib' and os.name == 'nt' and name == 'zlib.h' else headers/name
                shutil.copyfile(header, stage/'include'/name)
            shutil.copyfile(source/'LICENSE', stage/'LICENSE')
            check_headers(stage/'include', codec)
            _, _, object_format, abi, crt = TARGETS[target]
            manifest = {'format': 'ku-native-compression-pack-v1', 'codec': codec, 'version': spec['version'],
                        'target': target, 'object_format': object_format, 'abi': abi, 'crt': crt,
                        'archive': record_file(stage/'lib'/archive_name(codec, target)),
                        'headers': [record_file(stage/'include'/name) for name in HEADER_NAMES[codec]],
                        'license': dict(record_file(stage/'LICENSE'), spdx=spec['license']),
                        'source': {'url': spec['url'], 'sha256': spec['sha256']},
                        'runtime_dependency': 'none', 'runtime_dependency_scope': 'codec shared-library sidecars; system CRT remains required',
                        'build': {'method': method, 'profile': 'Release', 'optimization': 'O2', 'multithread': False,
                                  'native_object_members': objects, 'compiler': compiler_version,
                                  'omitted_nonbuild_archive_links': omitted_links}}
            (stage/'manifest.json').write_text(json.dumps(manifest, ensure_ascii=False, indent=2)+'\n', encoding='utf-8')
            for path in stage.rglob('*'):
                path.chmod(0o755 if path.is_dir() else 0o644)
            stage.chmod(0o755)
            validate_pack(stage, codec, target)
            if leaf.exists():
                raise ValueError('compression output appeared while building; refusing overwrite')
            stage.rename(leaf)
            # TemporaryDirectory owns its original name, not the published
            # leaf. Its cleanup therefore cannot remove the immutable pack.
        print('Built optional compression pack:', leaf, flush=True)
        return manifest


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--root', type=Path, required=True, help='private cache/output root, not a system install prefix')
    parser.add_argument('--target', choices=TARGETS, required=True)
    parser.add_argument('--library', choices=('all', 'zstd', 'zlib'), default='all')
    parser.add_argument('--zstd-source', type=Path)
    parser.add_argument('--zlib-source', type=Path)
    parser.add_argument('--fetch', action='store_true', help='explicitly download missing pinned official sources into the private root; default never accesses the network')
    parser.add_argument('--cc', help='existing absolute C compiler executable; no compiler flags')
    parser.add_argument('--cmake', help='existing CMake executable for Windows; never installed by this script')
    args = parser.parse_args()
    host_target(args.target)
    if not args.root.is_absolute():
        parser.error('--root must be absolute')
    args.root.mkdir(parents=True, exist_ok=True)
    root = args.root.resolve()
    for codec in ('zstd', 'zlib') if args.library == 'all' else (args.library,):
        source = getattr(args, codec+'_source')
        if args.fetch and source is None and not (root/'native-compression/v1'/args.target/codec).exists():
            source = fetch_source(root, codec)
        build_one(root, args.target, codec, source, args.cc, args.cmake)


if __name__ == '__main__':
    main()
