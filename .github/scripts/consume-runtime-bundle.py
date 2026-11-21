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
import posixpath
import re
import shutil
import socket
import ssl
import stat
import struct
import sys
import tempfile
import threading
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

# Fixed script, structured UTF-8 JSON input; paths are never interpolated into
# executable PowerShell text. Only a verified, owned SDK tree is affected.
WINDOWS_READONLY_ACL = r'''
$ErrorActionPreference = 'Stop'
[Console]::OutputEncoding = [Text.UTF8Encoding]::new($false)
$inputData = [IO.File]::ReadAllText($env:KU_READONLY_ACL_INPUT, [Text.Encoding]::UTF8) | ConvertFrom-Json
$root = [string]$inputData.root
$backup = [string]$inputData.backup
$access = [Security.AccessControl.AccessControlSections]::Access
$sidType = [Security.Principal.SecurityIdentifier]
function Rule-Fingerprint($acl) {
    $keys = @($acl.GetAccessRules($true, $true, $sidType) | ForEach-Object {
        "$($_.IdentityReference.Value)|$([int64]$_.FileSystemRights)|$($_.AccessControlType)|$($_.InheritanceFlags)|$($_.PropagationFlags)|$($_.IsInherited)"
    } | Sort-Object)
    return ConvertTo-Json -InputObject $keys -Compress
}
function Samples {
    $result = @()
    foreach ($relative in @('.', 'ku.exe', 'deps', 'ku-runtime.json')) {
        $path = if ($relative -eq '.') { $root } else { Join-Path $root $relative }
        $acl = Get-Acl -LiteralPath $path
        $rules = @($acl.GetAccessRules($true, $true, $sidType))
        $result += [ordered]@{ relative=$relative; protected=$acl.AreAccessRulesProtected; rules_truncated=($rules.Count -gt 16); rules=@($rules | Select-Object -First 16 | ForEach-Object {
            [ordered]@{sid=$_.IdentityReference.Value; mask=[int64]$_.FileSystemRights; rights=[string]$_.FileSystemRights; type=[string]$_.AccessControlType; inheritance=[string]$_.InheritanceFlags; propagation=[string]$_.PropagationFlags; inherited=$_.IsInherited}
        }) }
    }
    return $result
}
if ($inputData.mode -eq 'apply') {
    if (Test-Path -LiteralPath $backup) { throw 'ACL backup unexpectedly exists' }
    $items = @((Get-Item -LiteralPath $root -Force)) + @(Get-ChildItem -LiteralPath $root -Force -Recurse)
    if ($items.Count -gt 2048) { throw 'SDK ACL entry bound exceeded' }
    $rows = @($items | ForEach-Object {
        if (($_.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) { throw 'SDK ACL fixture contains a link' }
        $acl = Get-Acl -LiteralPath $_.FullName
        [ordered]@{ relative=[IO.Path]::GetRelativePath($root, $_.FullName); directory=$_.PSIsContainer; sddl=$acl.GetSecurityDescriptorSddlForm($access); protected=$acl.AreAccessRulesProtected; fingerprint=(Rule-Fingerprint $acl) }
    })
    $bytes = [Text.Encoding]::UTF8.GetBytes((ConvertTo-Json -InputObject $rows -Depth 6 -Compress))
    if ($bytes.Length -gt 4MB) { throw 'SDK ACL backup bound exceeded' }
    [IO.File]::WriteAllBytes($backup + '.writing', $bytes)
    [IO.File]::Move($backup + '.writing', $backup)
    $current = [Security.Principal.WindowsIdentity]::GetCurrent().User
    $system = [Security.Principal.SecurityIdentifier]::new('S-1-5-18')
    # Protect children before their parents. There are no deny ACEs and no
    # inherited administrator FullControl left to make the write probe pass.
    foreach ($row in @($rows | Sort-Object { $_.relative.Length } -Descending)) {
        $path = if ($row.relative -eq '.') { $root } else { Join-Path $root $row.relative }
        $acl = if ($row.directory) { [Security.AccessControl.DirectorySecurity]::new() } else { [Security.AccessControl.FileSecurity]::new() }
        $acl.SetAccessRuleProtection($true, $false)
        $acl.AddAccessRule([Security.AccessControl.FileSystemAccessRule]::new($current, [Security.AccessControl.FileSystemRights]::ReadAndExecute, [Security.AccessControl.AccessControlType]::Allow))
        $acl.AddAccessRule([Security.AccessControl.FileSystemAccessRule]::new($system, [Security.AccessControl.FileSystemRights]::FullControl, [Security.AccessControl.AccessControlType]::Allow))
        Set-Acl -LiteralPath $path -AclObject $acl
    }
    [ordered]@{mode='protected-allow-RX'; entry_count=$rows.Count; samples=@(Samples)} | ConvertTo-Json -Depth 8 -Compress
} elseif ($inputData.mode -eq 'restore') {
    $rows = @([IO.File]::ReadAllText($backup, [Text.Encoding]::UTF8) | ConvertFrom-Json)
    if ($rows.Count -gt 2048) { throw 'SDK ACL restore entry bound exceeded' }
    foreach ($row in @($rows | Sort-Object { $_.relative.Length })) {
        $path = if ($row.relative -eq '.') { $root } else { Join-Path $root $row.relative }
        $acl = Get-Acl -LiteralPath $path
        $acl.SetSecurityDescriptorSddlForm([string]$row.sddl, $access)
        Set-Acl -LiteralPath $path -AclObject $acl
    }
    $mismatches = @($rows | Where-Object {
        $path = if ($_.relative -eq '.') { $root } else { Join-Path $root $_.relative }
        $acl = Get-Acl -LiteralPath $path
        $acl.AreAccessRulesProtected -ne $_.protected -or (Rule-Fingerprint $acl) -cne $_.fingerprint
    } | Select-Object -First 8 | ForEach-Object { $_.relative })
    [ordered]@{restored=($mismatches.Count -eq 0); mismatch_paths=$mismatches; entry_count=$rows.Count; verification='original ACE tuples and inheritance-protection state'} | ConvertTo-Json -Depth 4 -Compress
    if ($mismatches.Count) { exit 1 }
} elseif ($inputData.mode -eq 'inspect') {
    [ordered]@{samples=@(Samples)} | ConvertTo-Json -Depth 8 -Compress
} else { throw 'Unknown ACL fixture action' }
'''


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


def assert_build_backend(result, backend, profile=None):
    """Require actual compiler execution evidence for the requested backend."""
    if backend not in ('c', 'runner'):
        raise ValueError('consumer expects a concrete C or Runner executable backend')
    text = output(result)
    expected, forbidden = (('c compiler command:', 'rustc command:') if backend == 'c' else
                           ('rustc command:', 'c compiler command:'))
    if result.returncode or expected not in text or forbidden in text:
        raise RuntimeError('build did not execute the expected ' + backend + ' compiler command')
    if profile is not None and 'build profile: ' + profile not in text:
        raise RuntimeError('build did not use the expected ' + backend + ' profile: ' + profile)


def native_without_rust(env, absent_home):
    """Hide Rust discovery while preserving the existing native tool paths."""
    selected = dict(env, RUSTUP_HOME=str(absent_home), RUSTUP_AUTO_INSTALL='0')
    selected.pop('KU_RUNNER_RUSTC', None)
    selected.pop('RUSTUP_TOOLCHAIN', None)
    names = ('rustup.exe', 'rustc.exe') if os.name == 'nt' else ('rustup', 'rustc')
    selected['PATH'] = os.pathsep.join(entry for entry in env.get('PATH', '').split(os.pathsep)
        if entry and not any((Path(entry) / name).is_file() for name in names))
    for name in names:
        if shutil.which(name, path=selected['PATH']) is not None:
            raise RuntimeError('native consumer could not isolate Rust discovery: ' + name)
    return selected


def record_native_build_diagnostics(detail, result):
    """Preserve bounded stderr and reject known CRT/deployment mismatches."""
    commands = [line for line in result.stdout.decode('utf-8', errors='replace').splitlines()
                if line.startswith('c compiler command: ')]
    detail['compiler_command_count'] = len(commands)
    detail['compiler_commands'] = [line.encode('utf-8')[:8192].decode('utf-8', errors='ignore')
                                   for line in commands[:4]]
    detail['compiler_commands_truncated'] = len(commands) > 4 or any(len(line.encode('utf-8')) > 8192 for line in commands)
    detail['build_stderr_bytes'] = len(result.stderr)
    encoded = result.stderr.decode('utf-8', errors='replace').encode('utf-8')
    detail['build_stderr_excerpt'] = encoded[:4096].decode('utf-8', errors='ignore')
    detail['build_stderr_truncated'] = len(encoded) > 4096
    # Compiler/linker warnings may be forwarded on either stream. Inspect all
    # bounded captured output, while storing only metadata and bounded stderr.
    text = (result.stdout + b'\n' + result.stderr).decode('utf-8', errors='replace')
    codes = sorted({code.upper() for code in re.findall(r'\bLNK(?:4098|4286|4217|2019|1120)\b', text, re.I)})
    if re.search(r'\bLNK2038\b[^\r\n]*(?:RuntimeLibrary|CRT)|'
                 r'(?:RuntimeLibrary|runtime librar(?:y|ies)|MSVCRT|LIBCMT|CRT)[^\r\n]{0,256}(?:mismatch|conflict)|'
                 r'(?:mismatch|conflict)[^\r\n]{0,256}(?:RuntimeLibrary|runtime librar(?:y|ies)|MSVCRT|LIBCMT|CRT)', text, re.I):
        codes.append('CRT/runtime-library mismatch')
    if re.search(r"built for newer ['\"]?macOS['\"]? version[^\r\n]{0,256}than (?:being )?linked", text, re.I):
        codes.append('macOS deployment-target mismatch')
    detail['build_link_contract_diagnostics'] = codes
    detail['build_link_contract_passed'] = result.returncode == 0 and not codes
    if codes:
        raise RuntimeError('native build reported incompatible CRT/linker/deployment diagnostics: ' + ', '.join(codes))


def invoke(command, cwd, env, seconds=300):
    OWNER.COMMAND_TIMEOUT_SECONDS = seconds
    with environment(env):
        return OWNER.run_bounded([str(p) for p in command], cwd, "installed archive consumer", check=False)


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


def tls_fixture_openssl(env):
    """Locate an existing certificate fixture tool; never install one."""
    explicit = env.get('KU_TEST_OPENSSL')
    if explicit is not None:
        path = Path(explicit)
        if not path.is_absolute() or not path.is_file():
            raise RuntimeError('KU_TEST_OPENSSL must name an existing absolute OpenSSL executable')
        return path
    candidates = []
    if sys.platform == 'darwin':
        candidates += [Path('/opt/homebrew/opt/openssl@3/bin/openssl'), Path('/usr/local/opt/openssl@3/bin/openssl')]
    found = shutil.which('openssl', path=env.get('PATH'))
    if found:
        candidates.append(Path(found))
    if os.name == 'nt':
        if env.get('PGROOT'):
            candidates.append(Path(env['PGROOT']) / 'bin/openssl.exe')
        git = shutil.which('git', path=env.get('PATH'))
        if git:
            candidates += [Path(git).parent.parent / 'usr/bin/openssl.exe', Path(git).parent.parent / 'mingw64/bin/openssl.exe']
    for candidate in candidates:
        if candidate.is_absolute() and candidate.is_file():
            return candidate
    raise RuntimeError('installed native TLS acceptance requires an existing OpenSSL certificate fixture tool; set KU_TEST_OPENSSL. This is a test-tool requirement, not a Ku runtime dependency')


def installed_tls_fixture(directory, env):
    certificate, key = directory / 'localhost.pem', directory / 'localhost-key.pem'
    tool = tls_fixture_openssl(env)
    result = invoke([tool, 'req', '-x509', '-sha256', '-newkey', 'ec', '-pkeyopt', 'ec_paramgen_curve:P-256',
                     '-nodes', '-days', '1', '-subj', '/CN=localhost',
                     '-addext', 'subjectAltName=DNS:localhost',
                     '-addext', 'basicConstraints=critical,CA:FALSE',
                     '-addext', 'keyUsage=critical,digitalSignature',
                     '-addext', 'extendedKeyUsage=serverAuth',
                     '-keyout', key, '-out', certificate], directory, env, seconds=15)
    if result.returncode:
        # Do not include any key material or arbitrary tool output in reports.
        raise RuntimeError('OpenSSL localhost certificate fixture creation failed; certificate tool exit=' + str(result.returncode))
    key.chmod(0o600)
    context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
    context.minimum_version = ssl.TLSVersion.TLSv1_2
    context.load_cert_chain(certificate, key)
    return certificate.read_text(encoding='ascii'), context


def installed_native_tls_consumer(install, root, env, report, report_path, target, *, checkout_hidden=False):
    """Run a moved archive's TLS program after its source and SDK disappear.

    The binary payload and native API mirror native_run_test's source-free TLS
    contract. Certificate files and the one loopback peer are private to this
    bounded temporary scope. The actual installed CLI must discover its own
    verified target pack; no source-tree compiler or substitute pack is used.
    """
    detail = {'status': 'running', 'target': target, 'scope': 'installed archive -> Release C11 O2 -> moved source-free TLS executable',
              'checkout_hidden_during_build': checkout_hidden,
              'installed_pack_files': {}, 'source_removed_before_run': False, 'sdk_unavailable_during_run': False,
              'installation_restored': False, 'peer_stopped': False, 'functional_check_passed': False}
    report['installed_native_tls'] = detail
    write_report(report_path, report)
    build_env = dict(env)
    for name in ('KU_NATIVE_TLS_PACK', 'KU_NATIVE_TLS_LINK_REQUIRED'):
        build_env.pop(name, None)
    ku = install / ('ku.exe' if os.name == 'nt' else 'ku')
    pack = install / 'native-tls/v1' / target
    archive = 'ku_native_tls.lib' if target.endswith('windows-msvc') else 'libku_native_tls.a'
    for relative in ('manifest.kutls', 'include/ku_native_tls.h', 'lib/' + archive):
        path = pack / relative
        if not path.is_file():
            raise RuntimeError('installed archive is missing its native TLS pack file: ' + relative)
        detail['installed_pack_files'][relative] = EXPORT.file_hash(path)
    with tempfile.TemporaryDirectory(prefix='ku-installed-tls-', dir=root) as temporary, socket.socket(socket.AF_INET, socket.SOCK_STREAM) as listener:
        temporary = Path(temporary)
        project = temporary / 'build project 中文'
        project.mkdir()
        deployment = temporary / '搬迁 TLS program'
        deployment.mkdir()
        unrelated = temporary / 'unrelated working directory'
        unrelated.mkdir()
        certificate, context = installed_tls_fixture(temporary, build_env)
        listener.bind(('127.0.0.1', 0))
        listener.listen(1)
        listener.settimeout(.1)
        port = listener.getsockname()[1]
        source = project / 'main.ku'
        source.write_text('''import bytes from "std.bytes"
import net from "std.net"
fn main(): null! {{
    payload = bytes.from_array([65, 0, 255])?
    client = net.client({{host: "127.0.0.1", port: {port}, tls: true,
        tls_server_name: "localhost", tls_ca_pem: {certificate},
        connect_timeout_ms: 3000, read_timeout_ms: 3000,
        write_timeout_ms: 3000, max_read_bytes: 32}})?
    client.write(payload)?
    reply = client.read(3)?
    println(reply.len())
    println(reply.get(0)?)
    println(reply.get(1)?)
    println(reply.get(2)?)
    client.close()
    return ok(null)
}}
'''.format(port=port, certificate=json.dumps(certificate)), encoding='utf-8')
        detail['source_sha256'] = EXPORT.file_hash(source)
        program = project / ('tls-client.exe' if os.name == 'nt' else 'tls-client')
        retired = root / 'installed-sdk-unavailable-for-tls'
        installation_retired = False
        stop = threading.Event()
        peer_result = {}
        peer = None
        active = []

        def serve():
            until = time.monotonic() + 10
            try:
                while not stop.is_set() and time.monotonic() < until:
                    try:
                        connection, _ = listener.accept()
                        break
                    except socket.timeout:
                        continue
                else:
                    if not stop.is_set():
                        raise TimeoutError('installed TLS client never connected')
                    return
                active.append(connection)
                connection.settimeout(3)
                with connection, context.wrap_socket(connection, server_side=True) as tls:
                    active.append(tls)
                    request = bytearray()
                    while len(request) < 3:
                        part = tls.recv(3-len(request))
                        if not part:
                            raise RuntimeError('installed TLS client closed before its binary payload')
                        request.extend(part)
                    if request != bytes((65, 0, 255)):
                        raise RuntimeError('installed TLS client changed its binary payload')
                    peer_result['tls_version'] = tls.version()
                    tls.sendall(bytes((66, 0, 254)))
                    peer_result['binary_echo'] = True
            except BaseException as error:
                peer_result['error'] = type(error).__name__ + ': ' + str(error)[:400]

        primary = None
        try:
            built = invoke([ku, 'build', '--backend', 'c', '--release', '--verbose', source.name, '-o', program], project, build_env)
            detail['build_exit'] = built.returncode
            detail['build_stdout_sha256'] = hashlib.sha256(built.stdout).hexdigest()
            detail['build_stderr_sha256'] = hashlib.sha256(built.stderr).hexdigest()
            record_native_build_diagnostics(detail, built)
            if built.returncode:
                raise RuntimeError('installed archive native TLS build failed: ' + (output(built)+built.stderr.decode(errors='replace'))[-4000:])
            assert_build_backend(built, 'c', 'release')
            emitted = reported_artifact(output(built), 'native c ok: ', project, temporary)
            if b'ku_tls_v1_client_new' not in emitted.read_bytes():
                raise RuntimeError('installed TLS build did not emit the native TLS engine path')
            detail['generated_c_sha256'] = EXPORT.file_hash(emitted)
            detail['binary_sha256'] = EXPORT.file_hash(program)
            deployed = deployment / program.name
            shutil.move(str(program), str(deployed))
            shutil.rmtree(project)
            detail['source_removed_before_run'] = not project.exists()
            if retired.exists():
                raise RuntimeError('TLS SDK quarantine destination unexpectedly exists')
            relocate_checkout_entry(install, retired)
            installation_retired = True
            detail['sdk_unavailable_during_run'] = not install.exists() and not ku.exists()
            if not detail['source_removed_before_run'] or not detail['sdk_unavailable_during_run']:
                raise RuntimeError('native TLS execution still has its original source or SDK')
            peer = threading.Thread(target=serve, name='installed-native-tls-peer')
            peer.start()
            started = invoke([deployed], unrelated, build_env, seconds=15)
            detail['run_exit'] = started.returncode
            detail['run_stdout_sha256'] = hashlib.sha256(started.stdout).hexdigest()
            detail['run_stderr_sha256'] = hashlib.sha256(started.stderr).hexdigest()
            if started.returncode or output(started) != '3\n66\n0\n254\n' or started.stderr:
                raise RuntimeError('moved source-free installed TLS program returned an incorrect result or exit')
            peer.join(timeout=5)
            if peer.is_alive() or peer_result.get('error') or not peer_result.get('binary_echo'):
                raise RuntimeError('installed TLS loopback peer failed: ' + str(peer_result))
            detail['tls_version'] = peer_result['tls_version']
            detail['binary_echo'] = True
            detail['functional_check_passed'] = True
        except BaseException as error:
            primary = error
        finally:
            cleanup = OWNER.CleanupErrors()
            stop.set()
            cleanup.attempt('TLS listener close', listener.close)
            for connection in active:
                cleanup.attempt('TLS accepted socket close', connection.close)
            if peer is not None:
                peer.join(timeout=5)
                if peer.is_alive():
                    cleanup.add('TLS peer thread still running')
            detail['peer_stopped'] = peer is None or not peer.is_alive()
            if installation_retired:
                if retired.is_dir() and not install.exists():
                    cleanup.attempt('restore installed SDK after TLS run', lambda: relocate_checkout_entry(retired, install))
                else:
                    cleanup.add('SDK restore paths changed; existing paths left untouched')
            detail['installation_restored'] = install.is_dir() and ku.is_file() and not retired.exists()
            if not detail['installation_restored']:
                cleanup.add('installed SDK restoration unconfirmed')
            detail['status'] = 'passed' if primary is None and not cleanup.failed else 'failed'
            write_report(report_path, report)
            cleanup.raise_if_any(primary)


def installed_native_compression_consumer(install, root, env, report, report_path, target, *, checkout_hidden=False):
    """Consume optional codec packs from this final moved archive only.

    The source exercises the same public bytes/codec contracts as the native
    integration suite. The resulting app must remain runnable after all source
    and generated C are deleted and the complete installed SDK is unavailable.
    """
    detail = {'status': 'running', 'target': target,
              'scope': 'installed archive -> Release C11 O2 -> moved source-free compression executable',
              'checkout_hidden_during_build': checkout_hidden, 'installed_packs': {},
              'source_removed_before_run': False, 'sdk_unavailable_during_run': False,
              'installation_restored': False, 'functional_check_passed': False}
    report['installed_native_compression'] = detail
    packs = install / 'native-compression/v1' / target
    if not (install / 'native-compression').exists():
        detail['status'] = 'not_in_archive'
        write_report(report_path, report)
        return
    write_report(report_path, report)
    for codec in ('zstd', 'zlib'):
        leaf = packs / codec
        manifest = EXPORT.NATIVE_COMPRESSION.validate_pack(leaf, codec, target)
        detail['installed_packs'][codec] = {
            'version': manifest['version'], 'target': manifest['target'],
            'manifest_sha256': EXPORT.file_hash(leaf / 'manifest.json'),
            'archive_sha256': manifest['archive']['sha256'],
            'headers': {row['name']: row['sha256'] for row in manifest['headers']},
            'license_sha256': manifest['license']['sha256']}
    build_env = dict(env)
    for name in ('KU_ZSTD_LIB', 'KU_ZSTD_INCLUDE', 'KU_ZLIB_LIB', 'KU_ZLIB_INCLUDE'):
        build_env.pop(name, None)
    ku = install / ('ku.exe' if os.name == 'nt' else 'ku')
    with tempfile.TemporaryDirectory(prefix='ku-installed-compression-', dir=root) as temporary:
        temporary = Path(temporary)
        project = temporary / 'codec build project 中文'
        project.mkdir()
        deployment = temporary / '搬迁 codec program'
        deployment.mkdir()
        unrelated = temporary / 'unrelated working directory'
        unrelated.mkdir()
        source = project / 'main.ku'
        source.write_text('''import {bytes, zstd, gzip, deflate} from "std"
fn main(): null! {
    source = bytes.from_array([0, 1, 127, 128, 255])?
    z = zstd.compress(source, 4096)?
    g = gzip.compress(source, 4096)?
    d = deflate.compress(source, 4096)?
    if (source.len() != 5 || source.get(4)? != 255) { fail "codec consumed input" }
    rz = zstd.decompress(z, 5)?
    rg = gzip.decompress(g, 5)?
    rd = deflate.decompress(d, 5)?
    if (rz.len() != 5 || rg.len() != 5 || rd.len() != 5) { fail "roundtrip length" }
    i = 0
    while (i < 5) {
        if (rz.get(i)? != source.get(i)? || rg.get(i)? != source.get(i)? || rd.get(i)? != source.get(i)?) {
            fail "roundtrip bytes"
        }
        i = i + 1
    }
    text = bytes.from_str("中文😀")?
    encoded = gzip.compress(text, 4096)?
    decoded = gzip.decompress(encoded, 4096)?
    if (decoded.to_str()? != "中文😀") { fail "Unicode roundtrip" }
    limited = false
    try { gzip.decompress(g, 4)? } catch(error) { limited = error.code == "output_limit" }
    if (!limited) { fail "output bound" }
    println("installed-native-compression-roundtrip-ok")
    return ok(null)
}
''', encoding='utf-8')
        detail['source_sha256'] = EXPORT.file_hash(source)
        program = project / ('codec.exe' if os.name == 'nt' else 'codec')
        retired = root / 'installed-sdk-unavailable-for-compression'
        installation_retired = False
        primary = None
        try:
            built = invoke([ku, 'build', '--backend', 'c', '--release', '--verbose', source.name, '-o', program], project, build_env)
            detail['build_exit'] = built.returncode
            detail['build_stdout_sha256'] = hashlib.sha256(built.stdout).hexdigest()
            detail['build_stderr_sha256'] = hashlib.sha256(built.stderr).hexdigest()
            record_native_build_diagnostics(detail, built)
            if built.returncode:
                raise RuntimeError('installed archive native compression build failed: ' + (output(built)+built.stderr.decode(errors='replace'))[-4000:])
            assert_build_backend(built, 'c', 'release')
            emitted = reported_artifact(output(built), 'native c ok: ', project, temporary)
            generated = emitted.read_bytes()
            for marker in (b'#define KU_FEATURE_ZSTD 1', b'#define KU_FEATURE_ZLIB 1', b'ku_zstd_compress', b'ku_gzip_compress', b'ku_deflate_compress'):
                if marker not in generated:
                    raise RuntimeError('installed compression build is missing a native codec path: ' + marker.decode())
            detail['generated_c_sha256'] = EXPORT.file_hash(emitted)
            detail['binary_sha256'] = EXPORT.file_hash(program)
            deployed = deployment / program.name
            shutil.move(str(program), str(deployed))
            shutil.rmtree(project)
            detail['source_removed_before_run'] = not project.exists()
            if retired.exists():
                raise RuntimeError('compression SDK quarantine destination unexpectedly exists')
            relocate_checkout_entry(install, retired)
            installation_retired = True
            detail['sdk_unavailable_during_run'] = not install.exists() and not ku.exists()
            if not detail['source_removed_before_run'] or not detail['sdk_unavailable_during_run']:
                raise RuntimeError('compression execution still has its original source or SDK')
            result = invoke([deployed], unrelated, build_env, seconds=15)
            detail['run_exit'] = result.returncode
            detail['run_stdout_sha256'] = hashlib.sha256(result.stdout).hexdigest()
            detail['run_stderr_sha256'] = hashlib.sha256(result.stderr).hexdigest()
            if result.returncode or output(result) != 'installed-native-compression-roundtrip-ok\n' or result.stderr:
                raise RuntimeError('moved source-free installed compression program returned an incorrect result or exit')
            detail['binary_roundtrip'] = True
            detail['unicode_roundtrip'] = True
            detail['output_bound'] = True
            detail['functional_check_passed'] = True
        except BaseException as error:
            primary = error
        finally:
            cleanup = OWNER.CleanupErrors()
            if installation_retired:
                if retired.is_dir() and not install.exists():
                    cleanup.attempt('restore installed SDK after compression run', lambda: relocate_checkout_entry(retired, install))
                else:
                    cleanup.add('SDK restore paths changed; existing paths left untouched')
            detail['installation_restored'] = install.is_dir() and ku.is_file() and not retired.exists()
            if not detail['installation_restored']:
                cleanup.add('installed SDK restoration unconfirmed')
            detail['status'] = 'passed' if primary is None and not cleanup.failed else 'failed'
            write_report(report_path, report)
            cleanup.raise_if_any(primary)


def windows_system_root(env):
    # os.environ is case-insensitive on Windows; a plain snapshot dict is not.
    value = next((value for key, value in env.items() if key.upper() == 'SYSTEMROOT'), None)
    if not value:
        raise RuntimeError('Windows loader diagnostic requires the existing SystemRoot environment')
    return Path(value)


def record_windows_gnu_pe_imports(detail, program, cwd, build_env, runtime_env, *, _seen=None, _depth=0, _app_dir=None):
    """Read the final EXE with the selected compiler's existing bounded tool.

    PATH resolutions are observations, not a replacement for Windows loader
    rules. In particular API-set names need not be files in System32.
    """
    seen = set() if _seen is None else _seen
    app_dir = program.parent if _app_dir is None else _app_dir
    identity = str(program.resolve()).casefold()
    if identity in seen:
        return
    seen.add(identity)
    data = {'status': 'running', 'scope': 'PE imports; observed file resolution, no PATH changes',
            'path': str(program), 'runtime_path': runtime_env.get('PATH', ''),
            'imports': [], 'dependencies': []}
    detail['pe_imports'] = data
    compiler = Path(build_env['KU_GNU_CC']).resolve(strict=True)
    tool = compiler.with_name('objdump.exe')
    if not compiler.is_file() or not tool.is_file():
        raise RuntimeError('selected Windows GNU compiler has no sibling objdump.exe')
    data['tool'] = str(tool)
    original_limit = OWNER.MAX_CAPTURE_BYTES
    inspection_directory = None
    try:
        OWNER.MAX_CAPTURE_BYTES = min(original_limit, 2 * 1024 * 1024)
        # Only objdump sees this same-hash ASCII copy. The real executable,
        # its Unicode deployment path and System-only runtime stay unchanged.
        with tempfile.TemporaryDirectory(prefix='ku-pe-imports-') as path:
            inspection_directory = Path(path)
            if not str(inspection_directory).isascii():
                raise RuntimeError('GNU PE inspection requires an existing ASCII temporary root')
            inspected = inspection_directory / ('image.dll' if program.suffix.lower() == '.dll' else 'image.exe')
            shutil.copyfile(program, inspected)
            data['binary_sha256'] = EXPORT.file_hash(program)
            data['inspected_copy_sha256'] = EXPORT.file_hash(inspected)
            if data['binary_sha256'] != data['inspected_copy_sha256']:
                raise RuntimeError('GNU PE inspection copy identity changed')
            result = invoke([tool, '-p', inspected.name], inspection_directory, build_env, seconds=15)
    finally:
        OWNER.MAX_CAPTURE_BYTES = original_limit
        data['inspection_copy_removed'] = inspection_directory is not None and not inspection_directory.exists()
    data.update(exit=result.returncode, stdout_bytes=len(result.stdout),
                stdout_sha256=hashlib.sha256(result.stdout).hexdigest(),
                stderr_excerpt=result.stderr[:2048].decode('utf-8', errors='replace'))
    text = result.stdout.decode('utf-8', errors='replace')
    raw = [line for line in text.splitlines() if re.match(r'^\s*DLL Name:', line)]
    data['dll_name_excerpt'] = '\n'.join(raw).encode('utf-8')[:4096].decode('utf-8', errors='ignore')
    names = list(dict.fromkeys(re.findall(r'^\s*DLL Name:\s*(\S+)\s*$', text, re.M)))
    data['pe_x64_verified'] = bool(re.search(r'\bfile format pei-x86-64\b', text))
    if result.returncode or not data['pe_x64_verified'] or (not names and program.suffix.lower() != '.dll') or len(names) > 64:
        data['status'] = 'failed'
        raise RuntimeError('bounded GNU objdump did not return a valid final EXE import list')
    system32 = windows_system_root(build_env) / 'System32'
    build_paths = [Path(entry) for entry in build_env.get('PATH', '').split(os.pathsep) if entry]
    runtime_paths = [Path(entry) for entry in runtime_env.get('PATH', '').split(os.pathsep) if entry]
    for name in names:
        if Path(name).name != name or '/' in name or '\\' in name:
            raise RuntimeError('GNU objdump returned a non-basename DLL import')
        system = system32 / name
        row = {'name': name, 'system32': str(system.resolve()) if system.is_file() else None,
               'api_set_contract': name.lower().startswith(('api-ms-win-', 'ext-ms-win-'))}
        if row['api_set_contract']:
            row['resolution_scope'] = 'Windows API-set contract; a PATH shim is not loader resolution evidence'
        elif not system.is_file():
            match = next((entry / name for entry in build_paths if (entry / name).is_file()), None)
            row['build_path_first_match'] = str(match.resolve()) if match is not None else None
            row['build_path_match_sha256'] = EXPORT.file_hash(match) if match is not None else None
        match = next((entry / name for entry in [app_dir, cwd, *runtime_paths]
                      if (entry / name).is_file()), None)
        row['runtime_file_first_match'] = str(match.resolve()) if match is not None else None
        data['imports'].append(row)
    for row in data['imports']:
        if row['api_set_contract']:
            continue
        found = row.get('build_path_first_match')
        if not found or str(Path(found).resolve()).casefold() in seen:
            continue
        if _depth >= 3 or len(seen) >= 8:
            data['recursion_incomplete'] = True
            continue
        child = {}
        record_windows_gnu_pe_imports(child, Path(found), cwd, build_env, runtime_env,
                                     _seen=seen, _depth=_depth + 1, _app_dir=app_dir)
        if 'pe_imports' in child:
            data['dependencies'].append(child['pe_imports'])
    data['status'] = 'captured'


def pe_imports_require_non_system_libgcc(data):
    pending, found, count = [data], False, 0
    while pending:
        item = pending.pop()
        count += 1
        if count > 8 or item.get('recursion_incomplete') or item.get('status') != 'captured':
            return False
        found |= any(row['name'].lower().startswith('libgcc_s') and row.get('build_path_first_match')
                     is not None for row in item.get('imports', []))
        pending.extend(item.get('dependencies', []))
    return found


def static_libgcc_loader_trial(detail, root, generated, archive, archive_sha256, tokens, env, runtime_env):
    """One diagnostic link of the exact C/archive; never product defaults.

    GCC documents -static-libgcc as selecting only the static libgcc, not
    -static. No shared-library exception ABI or panic policy is changed here.
    https://gcc.gnu.org/onlinedocs/gcc/Link-Options.html
    """
    trial = {'scope': 'conditional diagnostic only; original failure retained', 'flag': '-static-libgcc',
             'generated_c_sha256': hashlib.sha256(generated).hexdigest(), 'archive_sha256': archive_sha256}
    detail['static_libgcc_trial'] = trial
    with tempfile.TemporaryDirectory(prefix='ku-static-libgcc-probe-', dir=root) as path:
        directory = Path(path)
        if not str(directory).isascii():
            raise RuntimeError('conditional GNU link probe requires an existing ASCII temporary root')
        (directory / 'source.c').write_bytes(generated)
        shutil.copyfile(archive, directory / 'calendar.a')
        if EXPORT.file_hash(directory / 'calendar.a') != archive_sha256:
            raise RuntimeError('conditional GNU link probe archive identity changed')
        temporary = directory / 'tmp'
        temporary.mkdir()
        selected = dict(env, TMP=str(temporary), TEMP=str(temporary), TMPDIR=str(temporary))
        command = [env['KU_GNU_CC'], 'source.c', '-std=c11', '-o', 'static-libgcc.exe', '-O2',
                   'calendar.a', *tokens, '-static-libgcc']
        linked = invoke(command, directory, selected, seconds=30)
        trial.update(command=command, build_exit=linked.returncode,
                     build_stderr_excerpt=linked.stderr[:2048].decode('utf-8', errors='replace'))
        if linked.returncode:
            return
        program = directory / 'static-libgcc.exe'
        trial.update(binary_sha256=EXPORT.file_hash(program), binary_bytes=program.stat().st_size)
        record_windows_gnu_pe_imports(trial, program, root, env, runtime_env)
        result = invoke([program], root, runtime_env, seconds=15)
        trial.update(run_exit=result.returncode, run_stdout=output(result)[:1024],
                     run_stderr_excerpt=result.stderr[:1024].decode('utf-8', errors='replace'),
                     functional_check_passed=result.returncode == 0 and output(result) == 'native-time-array-ok\n' and not result.stderr)


def installed_native_time_array_consumer(install, root, env, report, report_path, target, *, checkout_hidden=False, loader_probe=False):
    """Consume the target calendar pack from this final moved archive only.

    The source exercises the public time and nullable array contracts as the native
    integration suite. The resulting app must remain runnable after all source
    and generated C are deleted and the complete installed SDK is unavailable.
    """
    detail = {'status': 'running', 'target': target,
              'scope': 'installed archive -> Release C11 O2 -> moved source-free time/array executable',
              'checkout_hidden_during_build': checkout_hidden, 'installed_packs': {},
              'source_removed_before_run': False, 'sdk_unavailable_during_run': False,
              'installation_restored': False, 'functional_check_passed': False}
    report['installed_native_time_array'] = detail
    leaf = install / 'native-calendar/v1' / target
    if not (install / 'native-calendar').exists():
        detail['status'] = 'not_in_archive'
        write_report(report_path, report)
        return
    manifest = EXPORT.NATIVE_CALENDAR.validate_pack(leaf, target)
    detail['installed_packs']['calendar'] = {
        'build_id': manifest['build_id'], 'source_sha256': manifest['source_sha256'],
        'manifest_sha256': EXPORT.file_hash(leaf / 'manifest.json'),
        'archive_sha256': manifest['archive']['sha256']}
    build_env = dict(env)
    build_env.pop('KU_NATIVE_CALENDAR_PACK', None)
    runtime_env = build_env
    if target == 'x86_64-pc-windows-gnu':
        system_root = Path(os.environ['SystemRoot']).resolve()
        runtime_env = dict(build_env, PATH=os.pathsep.join(
            (str(system_root / 'System32'), str(system_root))))
        detail['runtime_search_path'] = 'Windows system directories only; no MinGW or SDK DLL search path'
    ku = install / ('ku.exe' if os.name == 'nt' else 'ku')
    with tempfile.TemporaryDirectory(prefix='ku-installed-time-array-', dir=root) as temporary:
        temporary = Path(temporary)
        project = temporary / 'module build project 中文'
        project.mkdir()
        deployment = temporary / '搬迁 module program'
        deployment.mkdir()
        unrelated = temporary / 'unrelated working directory'
        unrelated.mkdir()
        source = project / 'main.ku'
        fixture = Path(__file__).resolve().parents[2] / 'examples/native_time_array.ku'
        source.write_text(fixture.read_text(encoding='utf-8'), encoding='utf-8')
        detail['source_sha256'] = EXPORT.file_hash(source)
        program = project / ('modules.exe' if os.name == 'nt' else 'modules')
        retired = root / 'installed-sdk-unavailable-for-time-array'
        installation_retired = False
        primary = None
        try:
            entry = ku
            if target == 'x86_64-pc-windows-gnu':
                launcher = invoke([ku, '--version'], project, runtime_env)
                detail['system_path_cli_exit'] = launcher.returncode
                if launcher.returncode or not output(launcher).startswith('ku '):
                    raise RuntimeError('GNU installed CLI requires an undeclared non-system runtime dependency')
            detail['cli_entry_kind'] = 'direct'
            if os.name != 'nt':
                aliases = temporary / 'command aliases 中文'
                aliases.mkdir()
                entry = aliases / 'ku'
                entry.symlink_to(ku)
                if not entry.is_symlink() or not os.path.samefile(entry, ku):
                    raise RuntimeError('time-array launcher does not resolve to the installed CLI')
                detail['cli_entry_kind'] = 'posix-symlink'
            built = invoke([entry, 'build', '--backend', 'c', '--release', '--verbose', source.name, '-o', program], project, build_env)
            detail['build_exit'] = built.returncode
            detail['build_stdout_sha256'] = hashlib.sha256(built.stdout).hexdigest()
            detail['build_stderr_sha256'] = hashlib.sha256(built.stderr).hexdigest()
            record_native_build_diagnostics(detail, built)
            if built.returncode:
                raise RuntimeError('installed archive native time-array build failed: ' + (output(built)+built.stderr.decode(errors='replace'))[-4000:])
            assert_build_backend(built, 'c', 'release')
            emitted = reported_artifact(output(built), 'native c ok: ', project, temporary)
            generated = emitted.read_bytes()
            for marker in (b'#define KU_FEATURE_NATIVE_CALENDAR 1', b'__ku_calendar_call_v1', b'__ku_array_first_', b'__ku_array_concat_'):
                if marker not in generated:
                    raise RuntimeError('installed module build is missing native implementation: ' + marker.decode())
            detail['generated_c_sha256'] = EXPORT.file_hash(emitted)
            detail['binary_sha256'] = EXPORT.file_hash(program)
            detail['binary_bytes'] = program.stat().st_size
            deployed = deployment / program.name
            shutil.move(str(program), str(deployed))
            shutil.rmtree(project)
            detail['source_removed_before_run'] = not project.exists()
            if retired.exists():
                raise RuntimeError('time-array SDK quarantine destination unexpectedly exists')
            relocate_checkout_entry(install, retired)
            installation_retired = True
            detail['sdk_unavailable_during_run'] = not install.exists() and not ku.exists()
            if not detail['source_removed_before_run'] or not detail['sdk_unavailable_during_run']:
                raise RuntimeError('time-array execution still has its original source or SDK')
            if target == 'x86_64-pc-windows-gnu':
                record_windows_gnu_pe_imports(detail, deployed, unrelated, build_env, runtime_env)
                write_report(report_path, report)
            result = invoke([deployed], unrelated, runtime_env, seconds=15)
            detail['run_exit'] = result.returncode
            detail['run_stdout_sha256'] = hashlib.sha256(result.stdout).hexdigest()
            detail['run_stderr_sha256'] = hashlib.sha256(result.stderr).hexdigest()
            write_report(report_path, report)
            if loader_probe and pe_imports_require_non_system_libgcc(detail.get('pe_imports', {})):
                static_libgcc_loader_trial(detail, root, generated,
                    retired / 'native-calendar/v1' / target / 'lib' / manifest['archive']['name'],
                    manifest['archive']['sha256'], manifest['native_static_libs']['tokens'], build_env, runtime_env)
            if result.returncode or output(result) != 'native-time-array-ok\n' or result.stderr:
                raise RuntimeError('moved source-free installed time-array program returned an incorrect result or exit')
            detail['binary_roundtrip'] = True
            detail['unicode_roundtrip'] = True
            detail['output_bound'] = True
            detail['functional_check_passed'] = True
        except BaseException as error:
            primary = error
        finally:
            cleanup = OWNER.CleanupErrors()
            if installation_retired:
                if retired.is_dir() and not install.exists():
                    cleanup.attempt('restore installed SDK after time-array run', lambda: relocate_checkout_entry(retired, install))
                else:
                    cleanup.add('SDK restore paths changed; existing paths left untouched')
            detail['installation_restored'] = install.is_dir() and ku.is_file() and not retired.exists()
            if not detail['installation_restored']:
                cleanup.add('installed SDK restoration unconfirmed')
            detail['status'] = 'passed' if primary is None and not cleanup.failed else 'failed'
            write_report(report_path, report)
            cleanup.raise_if_any(primary)


def installed_native_http_client_consumer(install, root, env, report, report_path, target, *,
                                          checkout_hidden=False, task_client=False, structured_task=False,
                                          backend='c'):
    if backend != 'c' and not (backend == 'runner' and structured_task):
        raise ValueError('only the structured Task example has a Runner comparison')
    task_client = task_client or structured_task
    example = 'native_http_client'
    if structured_task:
        example = 'native_task_structured_http'
    elif task_client:
        example = 'native_task_http_client'
    detail={'status':'running','target':target,'scope':'installed optional '+('Task' if task_client else 'synchronous')+' HTTP client; bounded loopback, moved source-free executable','checkout_hidden_during_build':checkout_hidden,'example':example+'.ku'}
    detail['backend'] = backend
    report['installed_'+example+('_runner' if backend == 'runner' else '')]=detail
    if not (install/'native-http').exists():
        detail['status']='not_in_archive';write_report(report_path,report);return
    leaf=install/'native-http/v1'/target
    manifest=EXPORT.NATIVE_HTTP_CLIENT.validate_pack(leaf,target)
    detail['pack_source_sha256']=manifest['source_sha256']
    if not task_client:
        calendar_manifest=EXPORT.NATIVE_CALENDAR.validate_pack(install/'native-calendar/v1'/target,target)
        detail['calendar_pack_source_sha256']=calendar_manifest['source_sha256']
    build_env=dict(env)
    build_env.pop('KU_NATIVE_HTTP_CLIENT_PACK',None)
    build_env.pop('KU_NATIVE_CALENDAR_PACK',None)
    runtime_env=build_env
    if target=='x86_64-pc-windows-gnu' and backend == 'c':
        system_root=Path(os.environ['SystemRoot']).resolve()
        runtime_env=dict(build_env,PATH=os.pathsep.join((str(system_root/'System32'),str(system_root))))
    with tempfile.TemporaryDirectory(prefix='ku-installed-http-client-',dir=root) as temp:
        temp=Path(temp);project=temp/'project outside SDK 中文';project.mkdir()
        deployment=temp/'deployed';deployment.mkdir();unrelated=temp/'unrelated';unrelated.mkdir()
        ku=install/('ku.exe' if os.name=='nt' else 'ku');retired=root/'installed-sdk-unavailable-for-http-client'
        stop=threading.Event();errors=[];received=[];peer=None;retired_sdk=False;primary=None
        with socket.socket(socket.AF_INET,socket.SOCK_STREAM) as listener:
            listener.bind(('127.0.0.1',0));listener.listen(4);listener.settimeout(.1)
            address='http://127.0.0.1:'+str(listener.getsockname()[1])
            source=(Path(__file__).resolve().parents[2]/'examples'/(example+'.ku')).read_text(encoding='utf-8').replace('http://127.0.0.1:8080',address)
            (project/'main.ku').write_text(source,encoding='utf-8');program=project/('client.exe' if os.name=='nt' else 'client')
            def serve():
                deadline=time.monotonic()+20
                try:
                    while len(received)<3 and not stop.is_set() and time.monotonic()<deadline:
                        try:connection,_=listener.accept()
                        except socket.timeout:continue
                        with connection:
                            connection.settimeout(2);data=bytearray()
                            while b'\r\n\r\n' not in data:
                                block=connection.recv(4096)
                                if not block:raise RuntimeError('incomplete HTTP fixture request')
                                data.extend(block)
                                if len(data)>16384:raise RuntimeError('HTTP fixture header bound')
                            head,body=bytes(data).split(b'\r\n\r\n',1);line=head.split(b'\r\n',1)[0]
                            length=0
                            structured_header = False
                            for header in head.split(b'\r\n')[1:]:
                                if header.lower().startswith(b'content-length:'):length=int(header.split(b':',1)[1])
                                if header.lower().startswith(b'x-ku-input:'):
                                    structured_header = header.split(b':',1)[1].strip() == b'structured'
                            if structured_task and not structured_header:
                                raise RuntimeError('structured Task HTTP configuration header did not reach the server')
                            if not 0<=length<=4096:raise RuntimeError('HTTP fixture body bound')
                            while len(body)<length:
                                block=connection.recv(length-len(body))
                                if not block:raise RuntimeError('incomplete HTTP fixture body')
                                body+=block
                            received.append(line.decode('ascii'))
                            if line.startswith(b'GET /hello '):status,payload=200,'hello 中文🙂'.encode()
                            elif line.startswith(b'POST /post '):status,payload=201,body
                            elif line.startswith(b'GET /missing '):status,payload=404,b'missing'
                            else:raise RuntimeError('unexpected HTTP fixture request')
                            if structured_task and line.startswith(b'GET /hello '):
                                # The native client's first read sees a live connection with no response.
                                if stop.wait(0.05):
                                    return
                            connection.sendall(('HTTP/1.1 '+str(status)+' Result\r\nX-Ku: yes\r\nContent-Length: '+str(len(payload))+'\r\nConnection: close\r\n\r\n').encode()+payload)
                    if len(received)!=3 and not stop.is_set():raise RuntimeError('HTTP fixture did not finish')
                except BaseException as e:errors.append(repr(e))
            try:
                entry=ku;detail['cli_entry_kind']='direct'
                if os.name!='nt':
                    aliases=temp/'launcher';aliases.mkdir();entry=aliases/'ku';entry.symlink_to(ku);detail['cli_entry_kind']='posix-symlink'
                selection = ['--backend', 'runner'] if backend == 'runner' else []
                built=invoke([entry,'build','--release','--verbose',*selection,'main.ku','-o',program],project,build_env)
                detail['build_exit']=built.returncode
                if backend == 'c':
                    record_native_build_diagnostics(detail,built)
                if built.returncode:raise RuntimeError('installed HTTP client build failed: '+(output(built)+built.stderr.decode(errors='replace'))[-4096:])
                assert_build_backend(built,backend,'release')
                if backend == 'c':
                    generated=reported_artifact(output(built),'native c ok: ',project,temp)
                    for feature in (b'#define KU_FEATURE_NATIVE_HTTP_CLIENT 1',b'#define KU_TASK_OUTBOUND_HTTP 1' if task_client else b'#define KU_FEATURE_NATIVE_CALENDAR 1'):
                        if feature not in generated.read_bytes():raise RuntimeError('combined native HTTP/calendar feature is absent: '+feature.decode())
                    detail['generated_c_sha256']=EXPORT.file_hash(generated)
                detail['binary_sha256']=EXPORT.file_hash(program)
                deployed=deployment/program.name;shutil.move(str(program),str(deployed));shutil.rmtree(project)
                if retired.exists():raise RuntimeError('HTTP client SDK quarantine exists')
                relocate_checkout_entry(install,retired);retired_sdk=True
                detail['source_removed_before_run']=not project.exists();detail['sdk_unavailable_during_run']=not install.exists()
                peer=threading.Thread(target=serve,name='installed-native-http-client-peer');peer.start()
                result=invoke([deployed],unrelated,runtime_env,seconds=15)
                detail['run_exit']=result.returncode;detail['run_stderr_bytes']=len(result.stderr)
                detail['process_owner_cleanup']=getattr(result,'process_owner_cleanup',None)
                expected='native-task-http-client-ok\n' if task_client else '[object]\nnative-http-client-ok\n'
                if structured_task:
                    expected = 'plan-ready\nstatus-ready\n'+expected
                stdout = output(result)
                if result.returncode or stdout!=expected or result.stderr:raise RuntimeError('installed HTTP client produced incorrect output: '+(stdout+result.stderr.decode(errors='replace'))[-4096:])
                if structured_task:
                    detail['run_stdout'] = stdout
                peer.join(3)
                if peer.is_alive() or errors or len(received)!=3:raise RuntimeError('HTTP fixture failed: '+repr(errors))
                if structured_task and [line.rsplit(' ',1)[0] for line in received] != ['GET /hello', 'GET /missing', 'POST /post']:
                    raise RuntimeError('structured Task HTTP recovery order failed: '+repr(received))
                detail['requests']=received
            except BaseException as e:primary=e
            finally:
                stop.set()
                if peer:peer.join(3)
                cleanup=OWNER.CleanupErrors()
                if peer and peer.is_alive():cleanup.add('HTTP loopback fixture thread did not exit')
                detail['fixture_thread_exited']=not peer or not peer.is_alive()
                if retired_sdk:
                    if retired.is_dir() and not install.exists():cleanup.attempt('restore HTTP SDK',lambda:relocate_checkout_entry(retired,install))
                    else:cleanup.add('HTTP SDK restoration paths changed')
                detail['installation_restored']=install.is_dir() and ku.is_file() and not retired.exists()
                detail['status']='passed' if primary is None and not cleanup.failed else 'failed'
                write_report(report_path,report);cleanup.raise_if_any(primary)


def stage_committed_ku_sources(repo, commit, env, project, directories, *, file_limit=64):
    tree = invoke(['git', 'ls-tree', '-rz', commit, '--', *directories], repo, env)
    if tree.returncode:
        raise RuntimeError('cannot read committed Ku source tree')
    names, hashes = set(), []
    total = 0
    for record in tree.stdout.decode('utf-8').split('\0'):
        if not record:
            continue
        description, name = record.split('\t', 1)
        if not name.endswith('.ku'):
            continue
        mode, kind, oid = description.split(' ')
        relative = EXPORT.checked_relative(name)
        if (mode != '100644' or kind != 'blob' or relative.casefold() in names or
                (file_limit is not None and len(names) >= file_limit)):
            raise ValueError('invalid committed Ku source: ' + relative)
        names.add(relative.casefold())
        blob = invoke(['git', 'cat-file', 'blob', oid], repo, env)
        if blob.returncode:
            raise RuntimeError('cannot copy committed Ku source: ' + relative)
        data = EXPORT.canonical_example(blob.stdout)
        total += len(data)
        # Bound the whole copied checkout fixture independently of the
        # compiler's per-source and 64-module limits.
        if total > 1024 * 1024:
            raise ValueError('committed Ku source tree exceeds its fixture bound')
        destination = project / relative
        destination.parent.mkdir(parents=True, exist_ok=True)
        destination.write_bytes(data)
        hashes.append((relative, hashlib.sha256(data).hexdigest()))
    return hashes


def installed_declaration_inventory_consumer(install, root, env, report, repo, commit, target):
    """Build the commit's inventory tool and exercise both installed backend contracts."""
    report['installed_declaration_inventory'] = detail = {'status': 'running', 'backends': {}}
    directories = ('bootstrap/stage1', 'bootstrap/stage2', 'bootstrap/stage3',
                   'bootstrap/tools/declarations')
    project = root / 'declaration-project'
    entry = project / 'bootstrap/tools/declarations/main.ku'
    hashes = stage_committed_ku_sources(repo, commit, env, project, directories)
    if not entry.is_file() or not (entry.parent / 'collect.ku').is_file():
        raise ValueError('the selected commit lacks the declaration inventory entry or helper')
    detail.update(source_files=len(hashes),
                  source_tree_sha256=hashlib.sha256(EXPORT.json_bytes(sorted(hashes))).hexdigest())

    deployment = root / 'declaration-deployment'
    deployment.mkdir()
    ku = install / ('ku.exe' if os.name == 'nt' else 'ku')
    programs = {}
    for backend in ('c', 'runner'):
        check = detail['backends'][backend] = {'status': 'building'}
        name = 'declarations-' + backend + ('.exe' if os.name == 'nt' else '')
        program = entry.parent / name
        selection = ['--backend', 'runner'] if backend == 'runner' else []
        built = invoke([ku, 'build', '--release', '--verbose', *selection, entry.name, '-o', program],
                       entry.parent, env)
        check['build_exit'] = built.returncode
        if built.returncode:
            raise RuntimeError('installed declaration inventory ' + backend + ' build failed: ' +
                               (output(built) + built.stderr.decode('utf-8', errors='replace'))[-4096:])
        assert_build_backend(built, backend, 'release')
        if backend == 'c':
            diagnostics = {}
            record_native_build_diagnostics(diagnostics, built)
            check['build_link_contract_passed'] = diagnostics['build_link_contract_passed']
        check['binary_sha256'] = EXPORT.file_hash(program)
        programs[backend] = deployment / name
        relocate_checkout_entry(program, programs[backend])
    input_source = (
        'struct Decl { label: str }\n'
        'fn KeepZeta(): int { return 42 }\n'
        'fn Drop(): bool { return false }\n'
        'fn KeepAlpha(): null! { return ok(null) }\n')
    retired = root / 'installed-sdk-unavailable-for-declarations'
    expected = '2:1 KeepZeta\n4:1 KeepAlpha\n'
    policy = 'prefix.txt is empty'
    relocate_checkout_entry(install, retired)
    try:
        detail['sdk_unavailable_during_run'] = not install.exists()
        for backend in ('runner', 'c'):
            check = detail['backends'][backend]
            # Runner embeds its entry but loads local imports from the original
            # project. Native captures the module graph at build time.
            inputs = entry.parent
            if backend == 'c':
                shutil.rmtree(project)
                inputs = deployment
            check['project_sources_unavailable'] = not project.exists()
            (inputs / 'source.ku').write_text(input_source, encoding='utf-8')
            prefix = inputs / 'prefix.txt'
            run_env = env
            if target == 'x86_64-pc-windows-gnu' and backend == 'c':
                system_root = Path(os.environ['SystemRoot']).resolve()
                run_env = dict(env, PATH=os.pathsep.join((str(system_root / 'System32'), str(system_root))))
            prefix.write_text('Keep\n', encoding='utf-8')
            success = invoke([programs[backend]], deployment, run_env, seconds=20)
            check.update(success_exit=success.returncode, inventory=output(success))
            if success.returncode or success.stderr or output(success) != expected:
                raise RuntimeError('installed declaration inventory ' + backend + ' output differed: ' +
                                   repr((output(success) + success.stderr.decode('utf-8', errors='replace'))[-1024:]))
            prefix.write_text('\n', encoding='utf-8')
            failure = invoke([programs[backend]], deployment, run_env, seconds=20)
            diagnostic = failure.stderr.decode('utf-8', errors='replace')
            check.update(error_exit=failure.returncode, error_stderr=diagnostic[-1024:])
            if failure.returncode != 1 or failure.stdout or policy not in diagnostic:
                raise RuntimeError('installed declaration inventory ' + backend + ' lost its policy error')
            check['status'] = 'passed'
    finally:
        relocate_checkout_entry(retired, install)
        detail['installation_restored'] = ku.is_file() and not retired.exists()
    detail['status'] = 'passed'


BOOTSTRAP_MANIFEST_LIMIT = 64
BOOTSTRAP_LEGACY_PROJECTION = 'semantics/projection_main.ku'
# Recognize imports in the staged compiler source without treating text
# inside comments or string/template literals as a module dependency.
BOOTSTRAP_IMPORT_TOKENS = re.compile(
    r"""(?P<comment>//[^\r\n]*|/\*.*?\*/)|"(?:\\.|[^"\\])*"|'(?:\\.|[^'\\])*'|"""
    r"`(?:\\.|[^`\\])*`|\bimport\b", re.DOTALL)
BOOTSTRAP_IMPORT_DECL = re.compile(
    r"""\s*(?:(?:\{[^}]*\}\s*|[^\W\d]\w*\s+)from\s*)?(?:"([^"\\\r\n]*)"|'([^'\\\r\n]*)')""")


def bootstrap_import_dependencies(module, source, available):
    source = BOOTSTRAP_IMPORT_TOKENS.sub(
        lambda token: ' ' if token.group('comment') else token.group(), source)
    dependencies = set()
    for token in BOOTSTRAP_IMPORT_TOKENS.finditer(source):
        if token.group() != 'import':
            continue
        declaration = BOOTSTRAP_IMPORT_DECL.match(source, token.end())
        if declaration is None:
            raise ValueError(f'cannot read staged bootstrap import in {module}')
        path = declaration[1] if declaration[1] is not None else declaration[2]
        if path == 'std' or path.startswith('std.'):
            continue
        if not path.startswith(('./', '../')) or '' in path.split('/'):
            raise ValueError(f'invalid staged bootstrap import in {module}: {path!r}')
        basename = path.rsplit('/', 1)[-1]
        if basename != '.' and '.' not in basename[1:]:
            path += '.ku'
        target = EXPORT.checked_relative(posixpath.normpath(posixpath.join(posixpath.dirname(module), path)))
        if target not in available:
            raise ValueError(f'missing staged bootstrap dependency in {module}: {target}')
        dependencies.add(target)
    return dependencies


def bootstrap_compiler_manifests(directory, compiler_modules):
    available = set(compiler_modules)
    direct = {module: bootstrap_import_dependencies(
        module, (directory / module).read_text(encoding='utf-8'), available)
        for module in compiler_modules}

    closures = {}
    for module in compiler_modules:
        closure, pending = set(), [module]
        while pending:
            dependency = pending.pop()
            if dependency not in closure:
                closure.add(dependency)
                pending.extend(direct[dependency] - closure)
        closures[module] = closure

    manifests, installed_checks = [], {}
    for module, closure in sorted(closures.items(), key=lambda item: (-len(item[1]), item[0])):
        if len(closure) > BOOTSTRAP_MANIFEST_LIMIT:
            if module != BOOTSTRAP_LEGACY_PROJECTION:
                raise ValueError(f'bootstrap closure for {module} has {len(closure)} modules, '
                                 f'exceeding the {BOOTSTRAP_MANIFEST_LIMIT}-module manifest limit')
            installed_checks[module] = len(closure)
            continue
        for manifest in manifests:
            if len(manifest | closure) <= BOOTSTRAP_MANIFEST_LIMIT:
                manifest.update(closure)
                break
        else:
            manifests.append(set(closure))
    return [sorted(manifest) for manifest in manifests], installed_checks


def installed_bootstrap_module_consumer(install, root, env, report, repo, commit, target):
    """Run Ku's installed module checker on its real compiler source graph."""
    report['installed_bootstrap_module'] = detail = {'status': 'running'}
    compiler = root / 'bootstrap-compiler'
    deployment = root / 'bootstrap-deployment'
    deployment.mkdir()
    hashes = stage_committed_ku_sources(repo, commit, env, compiler, (
        'bootstrap/stage1', 'bootstrap/stage2', 'bootstrap/stage3', 'bootstrap/semantics',
        'bootstrap/tools/compiler_driver',
    ), file_limit=None)
    inventory_hashes = stage_committed_ku_sources(repo, commit, env, compiler, ('bootstrap/tools/declarations',))
    detail['compiler_source_tree_sha256'] = hashlib.sha256(EXPORT.json_bytes(sorted(hashes))).hexdigest()
    modules = ('stage2/context.ku', 'stage2/span.ku', 'stage1/token.ku')
    compiler_modules = [name.removeprefix('bootstrap/') for name, _ in hashes]
    compiler_manifests, installed_checks = bootstrap_compiler_manifests(compiler / 'bootstrap', compiler_modules)
    binder_modules = {module for project in compiler_manifests for module in project}
    if binder_modules | set(installed_checks) != set(compiler_modules):
        raise RuntimeError('bootstrap compiler manifests omitted committed source modules')
    inventory_sources = [name.removeprefix('bootstrap/') for name, _ in inventory_hashes]
    inventory_modules = [name for name in compiler_modules if name.startswith(('stage1/', 'stage2/', 'stage3/'))]
    inventory_modules.extend(inventory_sources)
    for relative in compiler_modules + inventory_sources:
        path = deployment / relative
        path.parent.mkdir(parents=True, exist_ok=True)
        shutil.copyfile(compiler / 'bootstrap' / relative, path)
    detail['input_sha256'] = {relative: EXPORT.file_hash(deployment / relative) for relative in modules}
    manifest = deployment / 'modules.txt'
    manifest.write_bytes(('\r\n'.join(modules) + '\r\n').encode('utf-8'))

    entry_directory = compiler / 'bootstrap/semantics'
    name = 'ku-check-project' + ('.exe' if os.name == 'nt' else '')
    program = entry_directory / name
    ku = install / ('ku.exe' if os.name == 'nt' else 'ku')
    detail['installed_ku_checks'] = checks = []
    for module, count in installed_checks.items():
        result = invoke([ku, 'check', module], compiler / 'bootstrap', env)
        evidence = {'module': module, 'closure_modules': count, 'exit': result.returncode}
        checks.append(evidence)
        if result.returncode or result.stderr or output(result) != f'check ok: {module}\n':
            raise RuntimeError(f'installed Ku could not check oversized bootstrap closure {module}: ' +
                               (output(result) + result.stderr.decode('utf-8', errors='replace'))[-4096:])
    built = invoke([ku, 'build', '--release', '--verbose', 'project_main.ku', '-o', program], entry_directory, env)
    if built.returncode:
        raise RuntimeError('installed bootstrap module compiler failed: ' +
                           (output(built) + built.stderr.decode('utf-8', errors='replace'))[-4096:])
    assert_build_backend(built, 'c', 'release')
    record_native_build_diagnostics(detail, built)
    detail['binary_sha256'] = EXPORT.file_hash(program)
    runnable = deployment / name
    relocate_checkout_entry(program, runnable)
    shutil.rmtree(compiler)

    run_env = env
    if target == 'x86_64-pc-windows-gnu':
        system = windows_system_root(env)
        run_env = dict(env, PATH=os.pathsep.join((str(system / 'System32'), str(system))))
    retired = root / 'installed-sdk-unavailable-for-bootstrap'
    relocate_checkout_entry(install, retired)
    try:
        detail['sdk_and_compiler_source_unavailable'] = not install.exists() and not compiler.exists()
        success = invoke([runnable], root, run_env, seconds=20)
        detail['bound_modules'] = rows = output(success).splitlines()
        expected_order = ['MODULE|' + module for module in reversed(modules)]
        if (success.returncode or success.stderr or
                [row for row in rows if row.startswith('MODULE|')] != expected_order or
                'IMPORT|stage2/span.ku|Token|stage1/token.ku|Token|19' not in rows or
                'IMPORT|stage2/context.ku|Token|stage1/token.ku|Token|19' not in rows or
                'TYPE|stage2/context.ku|Span|stage2/span.ku|7|15' not in rows):
            raise RuntimeError('relocated bootstrap module binding differed: ' +
                               repr((output(success) + success.stderr.decode('utf-8', errors='replace'))[-4096:]))
        manifest.write_bytes(('\n'.join(modules[:2]) + '\n').encode('utf-8'))
        failure = invoke([runnable], root, run_env, seconds=20)
        detail['missing_dependency'] = diagnostic = failure.stderr.decode('utf-8', errors='replace')
        if failure.returncode != 1 or failure.stdout or '|unresolved_import|' not in diagnostic:
            raise RuntimeError('relocated bootstrap tool lost its missing dependency diagnostic')

        # Bounded compiler closures and the declaration inventory run
        # through the shipped binder with the SDK and build sources removed.
        def bind_project(label, project_modules):
            ordered = sorted(project_modules)
            manifest.write_bytes(('\n'.join(ordered) + '\n').encode('utf-8'))
            whole_project = invoke([runnable], root, run_env, seconds=20)
            project_rows = output(whole_project).splitlines()
            bound_paths = [row.split('|', 1)[1] for row in project_rows if row.startswith('MODULE|')]
            if whole_project.returncode or whole_project.stderr or sorted(bound_paths) != ordered:
                raise RuntimeError(f'relocated bootstrap tool could not bind {label}: ' +
                                   whole_project.stderr.decode('utf-8', errors='replace')[-4096:])
            return project_rows

        def bound_summary(project_modules, project_rows):
            return {
                'modules': len(project_modules),
                'rows': len(project_rows),
                'rows_sha256': hashlib.sha256(('\n'.join(project_rows) + '\n').encode('utf-8')).hexdigest(),
            }

        compiler_rows = []
        manifest_results = []
        for index, project in enumerate(compiler_manifests, start=1):
            result = bind_project(f'compiler manifest {index}', project)
            compiler_rows.extend(result)
            manifest_results.append(bound_summary(project, result))
        if len(compiler_manifests) > 1:
            compiler_rows = sorted(set(compiler_rows))
        detail['complete_project'] = bound_summary(binder_modules, compiler_rows)
        detail['complete_project']['source_modules'] = len(compiler_modules)
        if len(compiler_manifests) > 1:
            detail['complete_project']['manifests'] = manifest_results
        inventory = bind_project('the declaration inventory', inventory_modules)
        detail['inventory_project'] = bound_summary(inventory_modules, inventory)
    finally:
        relocate_checkout_entry(retired, install)
        detail['installation_restored'] = ku.is_file() and not retired.exists()
    detail['status'] = 'passed'


def validate_installed_relative_program(install, project, env, target, files, expected, *,
                                        native_only_diagnostic=None):
    project.mkdir()
    for name, contents in files.items():
        (project / name).write_text(contents, encoding='utf-8')
    entry = project / 'main.ku'
    cwd = project.parent
    ku = install / ('ku.exe' if os.name == 'nt' else 'ku')
    run_env = env
    if target == 'x86_64-pc-windows-gnu':
        system = windows_system_root(env)
        run_env = dict(env, PATH=os.pathsep.join((str(system / 'System32'), str(system))))
    outputs = {}
    programs = {}
    for backend in ('c', 'runner'):
        program = project / (backend + ('.exe' if os.name == 'nt' else ''))
        selection = ['--backend', 'runner'] if backend == 'runner' else []
        result = invoke([ku, 'build', '--release', '--verbose', *selection, entry, '-o', program], cwd, env)
        if result.returncode:
            raise RuntimeError(project.name + ' installed build failed: ' +
                               result.stderr.decode('utf-8', errors='replace')[-4096:])
        assert_build_backend(result, backend, 'release')
        programs[backend] = program
    for mode, command, selected_env in (
        ('interpreter', [ku, 'run', entry], env),
        ('runner', [programs['runner']], env),
        ('c', [programs['c']], run_env),
    ):
        if mode == 'c':
            for name in files:
                (project / name).unlink()
        result = invoke(command, cwd, selected_env, seconds=20)
        observed = output(result)
        if native_only_diagnostic is not None and mode != 'c':
            diagnostic = result.stderr.decode('utf-8', errors='replace')
            if result.returncode != 1 or observed or native_only_diagnostic not in diagnostic:
                raise RuntimeError(project.name + ' installed diagnostic differed: ' +
                                   repr((observed + diagnostic)[-4096:]))
            outputs[mode] = {'exit_code': result.returncode, 'diagnostic': native_only_diagnostic}
            continue
        if result.returncode or result.stderr or observed != expected:
            raise RuntimeError(project.name + ' installed execution differed: ' +
                               repr((observed + result.stderr.decode('utf-8', errors='replace'))[-4096:]))
        outputs[mode] = observed
    return {'status': 'passed', 'outputs': outputs,
            'native_sources_removed': all(not (project / name).exists() for name in files)}


def installed_forward_layout_consumer(install, root, env, report, target):
    """Check owned forward layout binding through a relative import."""
    files = {
        'data.ku': 'struct Packet { detail: Payload }\n'
                   'fn Make(): Packet { return Packet { detail: Payload { name: "Ku🙂" } } }\n'
                   'fn Read(): str { main = Make(); return main.detail.name }\n'
                   'struct Payload { name: str }\n',
        'main.ku': 'import { Read } from "./data.ku"\n'
                   'fn main(): null { println(Read()); return null }\n',
    }
    report['installed_forward_layout'] = validate_installed_relative_program(
        install, root / 'forward-layout-project', env, target, files, 'Ku🙂\n')


def installed_native_only_bytes_consumer(install, root, env, report, target):
    """An installed interpreter or Runner explains the native-only bytes call."""
    files = {
        'helper.ku': 'import { bytes } from "std"\n'
                     'fn Encoded(text: str): int! {\n'
                     '    packed = bytes.from_str(text)?\n'
                     '    return ok(packed.len())\n'
                     '}\n',
        'main.ku': 'import { Encoded as Count } from "./helper.ku"\n'
                   'fn main(): null! { println(Count("中文🙂")?) return ok(null) }\n',
    }
    report['installed_native_only_bytes'] = validate_installed_relative_program(
        install, root / 'native-only-bytes-project', env, target, files, '10\n',
        native_only_diagnostic='std.bytes currently requires native C; use `ku build --backend c`')


def installed_task_error_consumer(install, root, env, report, target):
    """Check inline Error evaluation and same-Task catch ownership after suspension."""
    files = {
        'helper.ku': """import time from "std.time"
struct Reply { body: str }
async fn Field(marker: int, value: str): str! {
    time.sleep(2)?
    println(marker)
    return ok(value + "!")
}
async fn FailCase(): int! {
    fail {
        message: (await Field(1, "no input"))?,
        domain: (await Field(2, "spec"))?,
        code: (await Field(3, "blocked"))?
    }
}
async fn ReplyPacket(good: bool): Reply! {
    time.sleep(2)?
    if (!good) { fail "upstream" }
    return ok(Reply { body: "过🙂" })
}
""",
        'main.ku': """import time from "std.time"
import { FailCase, ReplyPacket } from "./helper.ku"
async fn main(): null! {
    try { (await FailCase())? } catch (issue) {
        read: fn(): bool = () => issue.code == "blocked!"
        alias = read
        time.sleep(2)?
        println(alias())
        println(issue.message)
    }
    println(((await ReplyPacket(true))?).body)
    try { println(((await ReplyPacket(false))?).body) }
    catch (issue) { println(issue.message) }
    println(7)
    return ok(null)
}
""",
    }
    report['installed_task_error'] = validate_installed_relative_program(
        install, root / 'task-error-project', env, target, files, '1\n2\n3\ntrue\nno input!\n过🙂\nupstream\n7\n')


def installed_task_optional_consumer(install, root, env, report, target):
    """Exercise optional owned Task results from a relocated multi-file program."""
    files = {
        'helper.ku': """struct Reply { detail: Detail }
struct Detail { body: str title: str | null }
async fn Fetch(): Reply! {
    return ok(Reply { detail: Detail { body: "Ku" + "🙂", title: "owned" + " title" } })
}
""",
        'main.ku': """import time from "std.time"
import { Fetch } from "./helper.ku"
async fn main(): null! {
    named = (await Fetch())?
    println(named?.detail?.unknown)
    println(((await Fetch())?)?.detail?.unknown)
    println(((await Fetch())?)?.detail?.title == null)
    println(((await Fetch())?)?.detail?.title != null)
    time.sleep(2)?
    println(named.detail.body)
    return ok(null)
}
""",
    }
    report['installed_task_optional'] = validate_installed_relative_program(
        install, root / 'task-optional-project', env, target, files, 'null\nnull\nfalse\ntrue\nKu🙂\n')


def installed_pattern_capture_consumer(install, root, env, report, target):
    """Capture imported defaults and a dynamic rest in relocated native closures."""
    files = {
        'data.ku': """struct Rule { prefix: str minimum: int | null }
fn Make(minimum: int | null): Rule { return Rule { prefix: "Ke" + "ep", minimum: minimum } }
""",
        'filter.ku': """import { Rule } from "./data.ku"
fn Default(value: int): int { println(value) return value }
fn Select(rule: Rule): fn(str): int | null {
    {prefix, minimum = Default(70), missing: fallback = Default(2)} = rule
    read: fn(str): int | null = fn(name: str): int | null {
        if (!name.starts_with(prefix)) { return null }
        if (minimum == null) { return fallback }
        return minimum
    }
    return read
}
fn Label(): fn(): str! {
    options = {prefix: "old", label: "tail" + "🙂"}
    {prefix, ...rest} = options
    read: fn(): str! = fn(): str! { return ok(prefix + ":" + rest["label"]?.as_str()?) }
    prefix = "new"
    return read
}
""",
        'main.ku': """import { Make } from "./data.ku"
import { Select, Label } from "./filter.ku"
fn Apply(handler: fn(str): int | null): int | null { return handler("KeepEntry") }
fn main(): null! {
    filter = Select(Make(null))
    println(Apply(filter.clone()))
    println(filter("DropEntry"))
    zero = Select(Make(0))
    println(Apply(zero))
    label = Label()
    println(label()?)
    return ok(null)
}
""",
    }
    report['installed_pattern_capture'] = validate_installed_relative_program(
        install, root / 'pattern-capture-project', env, target, files, '2\n2\nnull\n2\n0\nnew:tail🙂\n')


def installed_task_array_import_consumer(install, root, env, report, repo, commit, target):
    """Exercise imported Task arrays using the same files as the native source regression."""
    fixture = 'tests/fixtures/native_task_array_import'

    def read_fixture(name):
        result = invoke(['git', 'show', f'{commit}:{fixture}/{name}'], repo, env)
        if result.returncode:
            raise RuntimeError(f'cannot read committed Task array fixture: {fixture}/{name}')
        return EXPORT.canonical_example(result.stdout).decode('utf-8')

    files = {name: read_fixture(name) for name in ('main.ku', 'worker.ku')}
    expected = read_fixture('expected.txt')
    detail = validate_installed_relative_program(install, root / 'task-array-import-project', env, target, files, expected)
    report['installed_task_array_import'] = dict(
        detail, fixture_sha256=hashlib.sha256(EXPORT.json_bytes({**files, 'expected.txt': expected})).hexdigest())


def installed_build_interrupt_consumer(install, root, repo, env, manifest, report):
    name = 'interrupted_build_reaps_its_compiler_group_and_preserves_previous_output'
    ku = install / manifest['cli']['path']
    digest = EXPORT.file_hash(ku)
    if digest != manifest['cli']['sha256']:
        raise ValueError('interrupted build CLI differs from the verified SDK')
    detail = dict(status='running', executable_sha256=digest, test=name)
    report['installed_build_interrupt'] = detail
    # Cargo builds the test harness. Its KU_BIN override selects the verified
    # archive executable for the actual interrupted build in the test's own workspace.
    test_env = dict(env, KU_BIN=str(ku), CARGO_TARGET_DIR=str(root / 'cli-interrupt-target'))
    result = invoke(['cargo', '+' + manifest['rustc']['toolchain'], 'test', '--locked', '--color', 'never',
                     '--test', 'cli_v002_test', name, '--', '--exact', '--test-threads=1'],
                    repo, test_env, seconds=600)
    stdout = output(result)
    if result.returncode or f'test {name} ... ok' not in stdout.splitlines():
        detail['status'] = 'failed'
        raise RuntimeError('installed interrupted-build regression failed: ' +
                           (stdout + result.stderr.decode('utf-8', errors='replace'))[-4096:])
    detail['status'] = 'passed'


def installed_collection_and_copy_consumers(install, root, repo, env, report, target):
    cases = [
        ('array_store', 'tests/native_collection_test.rs', 'RECEIVER_ORDER_SOURCE', 'collection-order-ok\n'),
        ('copy_field_call', 'tests/borrow_checker_test.rs', 'COPY_FIELD_CALL_SOURCE', ''),
        ('copy_index_call', 'tests/borrow_checker_test.rs', 'COPY_INDEX_CALL_SOURCE', ''),
        ('c_keyword_identifiers', 'tests/native_collection_test.rs', 'C_KEYWORD_SOURCE', 'Ku\n147\n'),
    ]
    for name, path, constant, expected in cases:
        fixture = (repo / path).read_text(encoding='utf-8')
        case = re.search(rf'const {constant}: &str = r#"\n(.*?)\n"#;', fixture, re.S)
        if case is None:
            raise RuntimeError(f'installed {name} fixture is missing')
        source = case[1] + '\n'
        detail = validate_installed_relative_program(install, root / name, env, target, {'main.ku': source}, expected)
        report['installed_' + name] = dict(detail, source_sha256=hashlib.sha256(source.encode('utf-8')).hexdigest())


def installed_runner_embedded_entry_consumer(install, root, env, report, target):
    ku = install / ('ku.exe' if '-windows-' in target else 'ku')
    detail = report['installed_runner_embedded_entry'] = {'status': 'running', 'cases': []}

    def record(name, result, expected_exit='zero'):
        case = dict(name='runner-embedded-entry-' + name, exit_code=result.returncode,
                    expected_exit=expected_exit, stdout_sha256=hashlib.sha256(result.stdout).hexdigest(),
                    stderr_sha256=hashlib.sha256(result.stderr).hexdigest())
        detail['cases'].append(case)
        if 'cases' in report:
            report['cases'].append(case)

    with tempfile.TemporaryDirectory(prefix='ku-embedded-entry-', dir=root) as temporary:
        working = Path(temporary)
        project = working / 'project'
        entry = project / 'src' / 'nested' / 'main.ku'
        entry.parent.mkdir(parents=True)
        (project / 'ku.mod').write_text(
            'name = "embedded_entry"\nroot = "src"\nmain = "nested/main.ku"\n', encoding='utf-8')
        entry.write_text(
            'import { Message } from "helper"\nfn main() { println(Message()) }\n', encoding='utf-8')
        dependency = project / 'src' / 'helper.ku'
        dependency.write_text('fn Message(): str { return "original 中文🙂" }\n', encoding='utf-8')
        launch = working / 'launch'
        launch.mkdir()
        executable = launch / ('anchor.exe' if '-windows-' in target else 'anchor')
        built = invoke([ku, 'build', '--release', '--offline', '--backend', 'runner', '--verbose',
                        '-o', executable], project, env)
        record('build', built)
        assert_build_backend(built, 'runner', 'release')
        detail['binary_sha256'] = EXPORT.file_hash(executable)
        entry.unlink()
        for label, wanted in [('original', 'original 中文🙂\n'), ('edited', 'edited 中文🙂\n')]:
            if label == 'edited':
                dependency.write_text('fn Message(): str { return "edited 中文🙂" }\n', encoding='utf-8')
            launched = invoke([executable], launch, env)
            record(label, launched)
            if launched.returncode or output(launched) != wanted or entry.exists():
                raise RuntimeError('embedded Runner did not use the original package dependency: ' +
                                   launched.stderr.decode(errors='replace')[-2000:])
        dependency.unlink()
        missing = invoke([executable], launch, env)
        record('dependency-missing', missing, 'nonzero')
        missing_error = missing.stderr.decode(errors='replace')
        if not missing.returncode or 'helper.ku' not in missing_error or 'failed to resolve' not in missing_error:
            raise RuntimeError('embedded Runner did not report its missing external dependency: ' + missing_error[-2000:])
    detail.update(status='passed', owned_temp_removed=not working.exists())


def run_structured_only(args, report):
    """Check the structured Task and declaration examples from a verified installed archive."""
    repo = args.repo.resolve()
    if repo != Path(__file__).resolve().parents[2]:
        raise ValueError('structured Task consumer requires its own checkout')
    metadata = json.loads(args.manifest.read_text(encoding='utf-8'))
    archive = args.archive.resolve()
    if (metadata['commit'] != args.expected_sdk_commit or
            EXPORT.file_hash(archive) != metadata['sha256'] or archive.stat().st_size != metadata['size']):
        raise ValueError('structured Task SDK archive identity mismatch')
    env = dict(os.environ, RUSTUP_AUTO_INSTALL='0')
    env.pop('KU_RUNNER_RUSTC', None)
    env.pop('RUSTUP_TOOLCHAIN', None)
    target = metadata['target']
    verifier = checked(['git', 'rev-parse', 'HEAD'], repo, env).strip()
    if verifier != metadata['commit']:
        raise ValueError('structured Task example and SDK must come from the same commit')
    schema = 'ku-bootstrap-generations-consumer-v1' if args.bootstrap_generations else 'ku-structured-task-consumer-v1'
    report.update(schema=schema, target=target, sdk_source_commit=metadata['commit'],
                  verification_code_commit=verifier, archive_sha256=metadata['sha256'],
                  installer_run_id=args.installer_run)
    if not args.bootstrap_generations:
        example = 'examples/native_task_structured_http.ku'
        source = EXPORT.canonical_example((repo / example).read_bytes())
        committed = checked(['git', 'show', f'{verifier}:{example}'], repo, env).encode('utf-8')
        if source != committed:
            raise ValueError('structured Task example differs from the selected commit')
        report['example_sha256'] = hashlib.sha256(source).hexdigest()
    with tempfile.TemporaryDirectory(prefix='ku-structured-task-consumer-') as temporary:
        root = Path(temporary).resolve()
        install = root / '安装 SDK'
        install.mkdir()
        EXPORT.verify_archive(archive, metadata['files'], install)
        manifest = EXPORT.RUNTIME_BUNDLE.verify(install, metadata['version'], metadata['commit'], target)
        report.update(bundle_id=manifest['bundle_id'], sdk_source_sha256=manifest['build_source_sha256'])
        if args.bootstrap_generations:
            verification = repo / 'bootstrap/tools/compiler_driver/selfhost/verify.py'
            cc = env.get('KU_GNU_CC') if target == 'x86_64-pc-windows-gnu' else 'clang'
            if not cc:
                raise RuntimeError('bootstrap generations require the selected GNU C compiler')
            ku = install / ('ku.exe' if '-windows-' in target else 'ku')
            result = invoke([sys.executable, '-B', verification, '--repo', repo,
                             '--revision', verifier, '--ku', ku, '--cc', cc,
                             '--output-dir', root / 'generations', '--temp-dir', root],
                            root, env, seconds=2400)
            if result.returncode:
                raise RuntimeError('installed bootstrap generations failed: ' +
                                   (output(result) + result.stderr.decode('utf-8', errors='replace'))[-4096:])
            receipt = json.loads(output(result))
            if receipt.get('status') != 'passed' or receipt.get('source_commit') != verifier:
                raise RuntimeError('installed bootstrap generations did not verify the same source')
            report['installed_bootstrap_generations'] = receipt
        else:
            installed_runner_embedded_entry_consumer(install, root, env, report, target)
            if target == 'x86_64-unknown-linux-gnu':
                installed_build_interrupt_consumer(install, root, repo, env, manifest, report)
            for backend in ('c', 'runner'):
                installed_native_http_client_consumer(install, root, env, report, args.output, target,
                                                      structured_task=True, backend=backend)
            native = report['installed_native_task_structured_http']
            runner = report['installed_native_task_structured_http_runner']
            if any(check['status'] != 'passed' for check in (native, runner)):
                raise RuntimeError('both installed structured Task backends must pass')
            if any(native[key] != runner[key] for key in ('run_stdout', 'requests')):
                raise RuntimeError('installed structured Task backends disagreed on output or request order')
            installed_declaration_inventory_consumer(install, root, env, report, repo, verifier, target)
            installed_bootstrap_module_consumer(install, root, env, report, repo, verifier, target)
            driver_test = repo / 'bootstrap/tools/compiler_driver/test_driver.py'
            driver_cc = env.get('KU_GNU_CC') if target == 'x86_64-pc-windows-gnu' else 'clang'
            if not driver_cc:
                raise RuntimeError('bootstrap driver verification requires the selected GNU C compiler')
            driver_ku = install / ('ku.exe' if '-windows-' in target else 'ku')
            result = invoke([sys.executable, '-B', driver_test, '--ku', driver_ku, '--cc', driver_cc],
                            root, env, seconds=300)
            if result.returncode or result.stderr or output(result) != 'bootstrap argv, module checks, signature and standalone C verified\n':
                raise RuntimeError('installed bootstrap command driver failed: ' +
                                   (output(result) + result.stderr.decode('utf-8', errors='replace'))[-4096:])
            report['installed_bootstrap_cli'] = {'status': 'passed', 'sha256': EXPORT.file_hash(driver_test)}
            installed_forward_layout_consumer(install, root, env, report, target)
            installed_native_only_bytes_consumer(install, root, env, report, target)
            installed_task_error_consumer(install, root, env, report, target)
            installed_task_optional_consumer(install, root, env, report, target)
            installed_pattern_capture_consumer(install, root, env, report, target)
            installed_task_array_import_consumer(install, root, env, report, repo, verifier, target)
            installed_collection_and_copy_consumers(install, root, repo, env, report, target)
    if not args.bootstrap_generations:
        report['backend_equivalence'] = True
    report.update(status='passed', owned_temp_removed=not root.exists())


def installed_native_http_relay_consumer(install, root, env, report, report_path, target, *, checkout_hidden=False):
    """One bounded real View -> Task outbound -> dynamic-header service.

    Socket fixture threads never mutate environment or start child processes.
    The existing owner runs and reaps the sole service process tree.
    """
    detail={'status':'running','target':target,'scope':'installed native HTTP end-to-end functionality, not performance',
            'checkout_hidden_during_build':checkout_hidden}
    report['installed_native_http_relay']=detail
    if not (install/'native-http').exists():
        raise RuntimeError('HTTP relay requires the matching optional HTTP pack')
    build_env=dict(env)
    for name in ('KU_NATIVE_HTTP_CLIENT_PACK','KU_NATIVE_CALENDAR_PACK'):
        build_env.pop(name,None)
    runtime_env=dict(build_env,KU_HTTP_MAX_REQUESTS='10')
    if target=='x86_64-pc-windows-gnu':
        system=windows_system_root(env)
        runtime_env['PATH']=os.pathsep.join((str(system/'System32'),str(system)))
    with tempfile.TemporaryDirectory(prefix='ku-installed-http-relay-',dir=root) as temporary:
        temporary=Path(temporary);project=temporary/'project outside SDK 中文';project.mkdir()
        deployed=temporary/('relay.exe' if os.name=='nt' else 'relay');unrelated=temporary/'unrelated';unrelated.mkdir()
        ku=install/('ku.exe' if os.name=='nt' else 'ku');unavailable=root/'sdk-unavailable-for-http-relay'
        stop=threading.Event();cancel_started=threading.Event();cancel_finished=threading.Event()
        errors=[];received=[];responses=[];threads=[];moved=False;primary=None
        with socket.socket(socket.AF_INET,socket.SOCK_STREAM) as upstream, socket.socket(socket.AF_INET,socket.SOCK_STREAM) as reserve:
            upstream.bind(('127.0.0.1',0));upstream.listen(4);upstream.settimeout(.1)
            reserve.bind(('127.0.0.1',0));service_address=reserve.getsockname()
            source=(Path(__file__).resolve().parents[2]/'examples/native_http_relay.ku').read_text(encoding='utf-8')
            source=source.replace('http://127.0.0.1:8080','http://127.0.0.1:'+str(upstream.getsockname()[1]))
            source=source.replace('127.0.0.1:8081','127.0.0.1:'+str(service_address[1]))
            address='http://127.0.0.1:'+str(upstream.getsockname()[1])
            extra='''import time from "std.time"
async fn LateFetch():HttpResponse! { config={url:__SLOW__,timeout_ms:5000,max_body_bytes:4096} return http.request(config) }
async fn Deadline():HttpResponse! { pending=LateFetch() time.sleep(1)? return await pending }
async fn Cancelled():HttpResponse! { config={url:__CANCEL__} r=http.request(config)? return ok(http.text(r.body.clone())) }
async fn InvalidConfig():HttpResponse! {
 try { r=http.request({url:__HELLO__,headers:{"Bad Name":"x"},timeout_ms:0})? return ok(http.text("continued")) }
 catch(e) { return ok(http.text("caught")) }
 return ok(http.text("continued"))
}
'''.replace('__SLOW__',json.dumps(address+'/slow')).replace('__CANCEL__',json.dumps(address+'/cancel')).replace('__HELLO__',json.dumps(address+'/hello'))
            source=extra+source.replace('    app.get("/relay", Relay)','    app.get("/relay", Relay)\n    app.get("/deadline", Deadline)\n    app.get("/cancel", Cancelled)\n    app.get("/invalid-config", InvalidConfig)')
            (project/'main.ku').write_text(source,encoding='utf-8');program=project/deployed.name
            def upstream_peer():
                deadline=time.monotonic()+20
                kept=None
                @contextmanager
                def next_connection(index):
                    nonlocal kept
                    if index==1:
                        if kept is None:raise RuntimeError('first upstream connection was not retained')
                        connection=kept;kept=None
                    else:
                        while not stop.is_set() and time.monotonic()<deadline:
                            try:connection,_=upstream.accept();break
                            except socket.timeout:continue
                        else:raise RuntimeError('HTTP relay upstream accept bound')
                    try:yield connection
                    finally:
                        if index==0:kept=connection
                        else:connection.close()
                try:
                    for index in range(6):
                        with next_connection(index) as connection:
                            connection.settimeout(2);request=bytearray()
                            while b'\r\n\r\n' not in request:
                                chunk=connection.recv(4096)
                                if not chunk or len(request)+len(chunk)>16384:raise RuntimeError('HTTP relay request bound')
                                request.extend(chunk)
                            line=bytes(request).split(b'\r\n',1)[0]
                            expected=(b'/hello',b'/hello',b'/hello',b'/slow',b'/cancel',b'/hello')[index]
                            if not line.startswith(b'GET '+expected+b' '):raise RuntimeError('HTTP relay used another upstream path')
                            received.append(line.decode('ascii'))
                            if index==1:detail['upstream_connection_reused']=True
                            payload='hello 中文🙂'.encode() if index!=1 else b'\xff'
                            if index==4:payload=b'cancelled-old-result';cancel_started.set()
                            connection_mode='keep-alive' if index==0 else 'close'
                            head=('HTTP/1.1 200 OK\r\nContent-Length: '+str(len(payload))+'\r\nConnection: '+connection_mode+'\r\n\r\n').encode()
                            if index in (2,3,4):
                                if stop.wait({2:1.2,3:1.7,4:.2}[index]):return
                                # This belongs to an already expired operation; it
                                # may have been closed, and must not revive it.
                                try:connection.sendall(head+payload)
                                except (BrokenPipeError,ConnectionResetError,OSError):pass
                                if index==4:cancel_finished.set()
                            else:
                                connection.sendall(head+payload[:1])
                                if stop.wait(.02):return
                                connection.sendall(payload[1:])
                except BaseException as error:
                    if not stop.is_set():errors.append('upstream: '+repr(error))
                finally:
                    if kept is not None:kept.close()
            def request_peer():
                deadline=time.monotonic()+18
                try:
                    paths=('/info','/relay','/relay','/relay','/deadline','/cancel','/invalid-config','/info','/relay','/info')
                    for index,path in enumerate(paths):
                        while True:
                            try:connection=socket.create_connection(service_address,timeout=.25);break
                            except OSError:
                                if stop.is_set() or time.monotonic()>=deadline:raise RuntimeError('HTTP relay startup bound')
                                stop.wait(.01)
                        with connection:
                            connection.settimeout(4)
                            connection.sendall(('GET '+path+' HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n').encode())
                            if path=='/cancel':
                                if not cancel_started.wait(2):raise RuntimeError('outbound operation did not begin before cancellation')
                                connection.setsockopt(socket.SOL_SOCKET,socket.SO_LINGER,struct.pack('hh' if os.name=='nt' else 'ii',1,0))
                                connection.close()
                                if not cancel_finished.wait(2):raise RuntimeError('late cancelled upstream did not finish')
                                responses.append({'path':path,'cancel':'reset-after-upstream-start'})
                                continue
                            response=bytearray()
                            while True:
                                chunk=connection.recv(4096)
                                if not chunk:break
                                response.extend(chunk)
                                if len(response)>32768:raise RuntimeError('HTTP relay response bound')
                            head,body=bytes(response).split(b'\r\n\r\n',1)
                            status=int(head.split(b' ',2)[1]);headers={}
                            for header in head.split(b'\r\n')[1:]:
                                name,value=header.split(b':',1);headers[name.lower()]=value.strip()
                            responses.append({'path':path,'status':status,'body_sha256':hashlib.sha256(body).hexdigest()})
                            if path=='/info':
                                if status!=200 or body!=b'/info':raise RuntimeError('View route or post-error recovery failed')
                            elif index in (1,8):
                                if status!=200 or body!='hello 中文🙂'.encode() or headers.get(b'x-ku-route')!=b'/relay' or headers.get(b'x-ku-upstream')!=b'local':raise RuntimeError('Task HTTP output/header ownership failed')
                            elif path=='/deadline':
                                if status!=504:raise RuntimeError('child outbound request extended the original handler deadline')
                            elif status!=500:raise RuntimeError('HTTP Result failure did not preserve the existing 500 contract')
                except BaseException as error:errors.append('driver: '+repr(error))
            try:
                built=invoke([ku,'build','--release','--verbose','main.ku','-o',program],project,build_env)
                detail['build_exit']=built.returncode;record_native_build_diagnostics(detail,built)
                if built.returncode:raise RuntimeError('installed HTTP relay build failed: '+(output(built)+built.stderr.decode(errors='replace'))[-4096:])
                assert_build_backend(built,'c','release')
                generated=reported_artifact(output(built),'native c ok: ',project,temporary)
                code=generated.read_bytes()
                for witness in (b'KU_FEATURE_NATIVE_HTTP_CLIENT',b'ku_task_host_http',b'ku_http_task_net_interests'):
                    if witness not in code:raise RuntimeError('native relay execution-path witness absent: '+witness.decode())
                detail.update(generated_c_sha256=EXPORT.file_hash(generated),binary_sha256=EXPORT.file_hash(program))
                bad_source='''import http from "std.http"
async fn main():null! {
 try { r=http.request({url:"http://127.0.0.1:1/",headers:{"Bad Name":"x"},timeout_ms:0})? println("continued") }
 catch(e) { println("caught") }
 return ok(null)
}
'''
                (project/'bad-config.ku').write_text(bad_source,encoding='utf-8')
                bad_program=temporary/('bad-config.exe' if os.name=='nt' else 'bad-config')
                bad_built=invoke([ku,'build','--release','--verbose','bad-config.ku','-o',bad_program],project,build_env)
                bad_detail={};detail['config_fatal']=bad_detail;record_native_build_diagnostics(bad_detail,bad_built)
                bad_issue=None
                if bad_built.returncode:bad_issue=RuntimeError('installed Task config-error program did not build')
                else:
                    assert_build_backend(bad_built,'c','release');bad_detail['binary_sha256']=EXPORT.file_hash(bad_program)
                shutil.move(program,deployed);shutil.rmtree(project)
                if unavailable.exists():raise RuntimeError('HTTP relay SDK quarantine exists')
                relocate_checkout_entry(install,unavailable);moved=True
                detail.update(source_removed_before_run=not project.exists(),sdk_unavailable_during_run=not install.exists())
                if bad_issue is None:
                    bad_result=invoke([bad_program],unrelated,runtime_env,seconds=10)
                    bad_detail.update(exit=bad_result.returncode,stdout=output(bad_result),stderr=bad_result.stderr[-4096:].decode(errors='replace'),
                                      process_owner_cleanup=getattr(bad_result,'process_owner_cleanup',None))
                    if bad_result.returncode==0 or bad_result.stdout or b'header' not in bad_result.stderr.lower():
                        bad_issue=RuntimeError('HTTP config fatal was caught, continued, or reported timeout before malformed headers')
                reserve.close()
                for name,body in (('installed-http-relay-upstream',upstream_peer),('installed-http-relay-driver',request_peer)):
                    peer=threading.Thread(target=body,name=name);threads.append(peer);peer.start()
                result=invoke([deployed],unrelated,runtime_env,seconds=22)
                detail.update(run_exit=result.returncode,process_owner_cleanup=getattr(result,'process_owner_cleanup',None),
                              run_stdout_excerpt=output(result)[-4096:],run_stderr_excerpt=result.stderr[-4096:].decode(errors='replace'))
                for peer in threads:peer.join(3)
                if result.returncode or errors or len(responses)!=10 or len(received)!=6:raise RuntimeError('installed HTTP relay failed: '+repr(errors)+' '+repr(responses))
                detail.update(responses=responses,upstream_requests=received,view_registered=True,
                              async_outbound_result=True,dynamic_headers=True,utf8_error_recovery=True,deadline_late_result_recovery=True,
                              inherited_deadline_before_await=True,cancelled_late_result_recovery=True,config_fatal_not_caught=bad_issue is None)
                if bad_issue is not None:raise bad_issue
            except BaseException as error:primary=error
            finally:
                stop.set()
                cleanup=OWNER.CleanupErrors()
                for peer in threads:
                    peer.join(3)
                    if peer.is_alive():cleanup.add('HTTP relay fixture did not exit: '+peer.name)
                if moved:
                    if unavailable.is_dir() and not install.exists():cleanup.attempt('restore relay SDK',lambda:relocate_checkout_entry(unavailable,install))
                    else:cleanup.add('HTTP relay SDK restoration paths changed')
                detail['installation_restored']=install.is_dir() and ku.is_file() and not unavailable.exists()
                detail['fixture_threads_exited']=all(not peer.is_alive() for peer in threads)
                detail.update(responses=responses,upstream_requests=received,fixture_errors=errors)
                detail['status']='passed' if primary is None and not cleanup.failed else 'failed'
                write_report(report_path,report);cleanup.raise_if_any(primary)


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


def windows_readonly_installation(install, project, root, env, report, report_path, version):
    """One controlled RX fixture with original-DACL recovery and real probes."""
    env = native_without_rust(env, root / 'absent-rustup-readonly')
    ku = install / 'ku.exe'
    backup = root / 'windows-readonly-dacl.json'
    input_path = root / 'windows-readonly-input.json'
    acl_env = dict(env, KU_READONLY_ACL_INPUT=str(input_path))
    detail = report.setdefault('windows_readonly_acl', {})
    created_probes = []

    def save():
        write_report(report_path, report)

    def command(name, argv, wanted=None):
        try:
            result = invoke(argv, project, env)
        except OSError as error:
            report['cases'].append(dict(name=name, start_error=type(error).__name__, winerror=getattr(error, 'winerror', None)))
            save()
            raise
        report['cases'].append(dict(name=name, exit_code=result.returncode, expected_exit='zero',
            stdout_sha256=hashlib.sha256(result.stdout).hexdigest(), stderr_excerpt=result.stderr.decode('utf-8', errors='replace')[-1500:]))
        save()
        if result.returncode or wanted is not None and output(result) != wanted:
            raise RuntimeError(name + ': ' + output(result)[-1500:] + result.stderr.decode('utf-8', errors='replace')[-1500:])
        return result

    def acl_action(mode):
        write_report(input_path, dict(mode=mode, root=str(install), backup=str(backup)))
        result = invoke(['pwsh', '-NoLogo', '-NoProfile', '-NonInteractive', '-Command', WINDOWS_READONLY_ACL], root, acl_env, seconds=60)
        if result.returncode:
            raise RuntimeError('ACL ' + mode + ' failed: ' + output(result)[-2000:] + result.stderr.decode('utf-8', errors='replace')[-2000:])
        return json.loads(output(result))

    command('readonly-control-version-before-ACL', [ku, '-v'], 'ku ' + version + '\n')
    try:
        detail['applied'] = acl_action('apply')
        save()
        failures = []
        try:
            with ku.open('rb') as source:
                detail['MZ_read_passed'] = source.read(2) == b'MZ'
            if not detail['MZ_read_passed']:
                failures.append('executable MZ read incorrect')
        except OSError as error:
            detail['MZ_read_error'] = str(error)[:500]
            failures.append('executable read denied')
        try:
            command('readonly-version-under-RX-ACL', [ku, '-v'], 'ku ' + version + '\n')
            detail['execute_passed'] = True
        except (OSError, RuntimeError) as error:
            detail['execute_error'] = str(error)[:1000]
            failures.append('version execution failed')
        detail['write_probes'] = []
        for relative, existing in (('.readonly-write-probe', False), ('deps/.readonly-write-probe', False), ('ku-runtime.json', True)):
            path = install / relative
            probe = dict(relative=relative, expected='PermissionError', existing=existing)
            try:
                with path.open('r+b' if existing else 'xb') as stream:
                    if not existing:
                        created_probes.append(path)
                        stream.write(b'must be denied')
                probe['denied'] = False
                failures.append('write unexpectedly allowed: ' + relative)
            except PermissionError as error:
                probe.update(denied=True, winerror=getattr(error, 'winerror', None))
            except OSError as error:
                probe.update(denied=False, other_error=str(error)[:500])
                failures.append('write probe had wrong error: ' + relative)
            detail['write_probes'].append(probe)
        save()
        if failures:
            raise RuntimeError('read-only ACL preflight failed: ' + '; '.join(failures))
        built = project / 'readonly-result.exe'
        result = command('read-only-install', [ku, 'build', '--verbose', 'hello.ku', '-o', built])
        assert_build_backend(result, 'c', 'debug')
        command('read-only-install-result', [built], 'Hello Ku\n')
        built.unlink()
        detail['build_and_run_passed'] = True
        detail['backend'] = 'c'
        detail['rust_tools_unavailable'] = True
    except BaseException as error:
        detail['failure'] = type(error).__name__ + ': ' + str(error)[-2000:]
        try:
            detail['failure_acl_samples'] = acl_action('inspect')
        except BaseException as inspection:
            detail['inspection_error'] = str(inspection)[-1000:]
        save()
        raise
    finally:
        if backup.exists():
            try:
                detail['restoration'] = acl_action('restore')
            except BaseException as error:
                detail['restoration_error'] = str(error)[-2000:]
                save()
                raise
        for path in created_probes:
            path.unlink(missing_ok=True)
        save()


def native_loader_leaf_probe(install, root, env, report, report_path, label, source, expected):
    """Two fixed tiny diagnostic leaves; no load generator or product shortcut."""
    detail = {'status': 'running', 'scope': 'same SDK Release C11 O2; System-only PATH; no calendar APIs'}
    report.setdefault('loader_leaf_programs', {})[label] = detail
    system = windows_system_root(env)
    runtime_env = dict(env, PATH=os.pathsep.join((str(system / 'System32'), str(system))))
    with tempfile.TemporaryDirectory(prefix='ku-loader-' + label + '-', dir=root) as path:
        directory = Path(path)
        project = directory / 'build project 中文'
        project.mkdir()
        (project / 'main.ku').write_text(source, encoding='utf-8')
        program = project / (label + '.exe')
        unavailable = root / ('sdk-unavailable-for-' + label)
        primary = None
        moved = False
        try:
            built = invoke([install / 'ku.exe', 'build', '--backend', 'c', '--release', '--verbose',
                            'main.ku', '-o', program], project, env)
            detail['build_exit'] = built.returncode
            record_native_build_diagnostics(detail, built)
            assert_build_backend(built, 'c', 'release')
            emitted = reported_artifact(output(built), 'native c ok: ', project, directory)
            if b'#define KU_FEATURE_NATIVE_CALENDAR 1' in emitted.read_bytes():
                raise RuntimeError('unused calendar feature entered a diagnostic leaf')
            detail.update(generated_c_sha256=EXPORT.file_hash(emitted), binary_sha256=EXPORT.file_hash(program),
                          binary_bytes=program.stat().st_size)
            deployed = directory / (label + '.exe')
            shutil.move(program, deployed)
            shutil.rmtree(project)
            relocate_checkout_entry(install, unavailable)
            moved = True
            detail.update(source_removed_before_run=not project.exists(), sdk_unavailable_during_run=not install.exists())
            record_windows_gnu_pe_imports(detail, deployed, root, env, runtime_env)
            result = invoke([deployed], root, runtime_env, seconds=15)
            detail.update(run_exit=result.returncode, run_stdout=output(result)[:1024],
                          run_stderr_excerpt=result.stderr[:1024].decode('utf-8', errors='replace'))
            detail['functional_check_passed'] = result.returncode == 0 and output(result) == expected and not result.stderr
        except BaseException as error:
            primary = error
        finally:
            cleanup = OWNER.CleanupErrors()
            if moved:
                if unavailable.is_dir() and not install.exists():
                    cleanup.attempt('restore loader leaf SDK', lambda: relocate_checkout_entry(unavailable, install))
                else:
                    cleanup.add('loader leaf SDK restoration paths changed')
            detail['installation_restored'] = install.is_dir() and not unavailable.exists()
            if not detail['installation_restored']:
                cleanup.add('loader leaf SDK restoration unconfirmed')
            detail['status'] = 'passed' if primary is None and not cleanup.failed and detail.get('functional_check_passed') else 'failed'
            write_report(report_path, report)
            cleanup.raise_if_any(primary)


def run_native_time_array_probe(args, report):
    """Diagnose only a producer's verified GNU SDK; not installation acceptance."""
    if os.name != 'nt':
        raise ValueError('--native-time-array-probe-bundle requires Windows GNU')
    target = 'x86_64-pc-windows-gnu'
    bundle = args.native_time_array_probe_bundle.resolve()
    repo = args.repo.resolve()
    version = json.loads((repo / 'editors/vscode-ku/package.json').read_text(encoding='utf-8'))['version']
    manifest = EXPORT.RUNTIME_BUNDLE.verify(bundle, version, args.expected_sdk_commit, target)
    report.update(schema='ku-native-loader-probe-v1', target=target,
                  scope='early GNU producer copy; diagnostic only, not final archive/installation acceptance',
                  sdk_source_commit=manifest['source_commit'], bundle_id=manifest['bundle_id'],
                  publisher=manifest['rustc'], verification_run_id=os.environ.get('GITHUB_RUN_ID'))
    temporary = None
    try:
        with tempfile.TemporaryDirectory(prefix='ku-native-loader-probe-') as path:
            temporary = Path(path).resolve()
            install = temporary / 'probe SDK 中文'
            shutil.copytree(bundle, install)
            EXPORT.RUNTIME_BUNDLE.verify(install, version, args.expected_sdk_commit, target)
            env = dict(os.environ, RUSTUP_AUTO_INSTALL='0')
            native_loader_leaf_probe(install, temporary, env, report, args.output, 'hello',
                                     'fn main() { println("loader-hello-ok") }\n', 'loader-hello-ok\n')
            native_loader_leaf_probe(install, temporary, env, report, args.output, 'scalar-time',
                                     'import time from "std.time"\nfn main() { println(time.unix() > 0) }\n', 'true\n')
            installed_native_time_array_consumer(install, temporary, env, report, args.output, target, loader_probe=True)
            if any(leaf.get('status') != 'passed' for leaf in report['loader_leaf_programs'].values()):
                raise RuntimeError('one or more System-only GNU loader leaves failed; all results are retained')
        report['status'] = 'passed'
    finally:
        report['owned_probe_root_removed'] = temporary is not None and not temporary.exists()


def run_readonly_only(args, report):
    """Supplement the already-tested immutable Windows SDK, not all cases."""
    if os.name != 'nt':
        raise ValueError('--readonly-only is a Windows supplemental gate')
    repo = args.repo.resolve()
    metadata = json.loads(args.manifest.read_text(encoding='utf-8'))
    if metadata['target'] != 'x86_64-pc-windows-msvc' or EXPORT.file_hash(args.archive) != metadata['sha256'] or args.archive.stat().st_size != metadata['size']:
        raise ValueError('readonly SDK archive identity mismatch')
    if args.expected_sdk_commit and args.expected_sdk_commit != metadata['commit']:
        raise ValueError('readonly SDK source differs from selected publisher')
    env = dict(os.environ, RUSTUP_AUTO_INSTALL='0')
    env.pop('KU_RUNNER_RUSTC', None)
    env.pop('RUSTUP_TOOLCHAIN', None)
    verifier = checked(['git', 'rev-parse', 'HEAD'], repo, env).strip()
    report.update(schema='ku-runtime-consumer-v1', status='running', cases=[], source_commit=metadata['commit'],
        sdk_source_commit=metadata['commit'], verification_code_commit=verifier, archive_sha256=metadata['sha256'],
        installer_run_id=args.installer_run, verification_run_id=os.environ.get('GITHUB_RUN_ID'),
        scope='Windows readonly supplement only; prior ordinary cases and other platforms are separate runs')
    with tempfile.TemporaryDirectory(prefix='ku-readonly-sdk-') as temporary:
        root = Path(temporary).resolve()
        install = root / '中文 只读 SDK'
        install.mkdir()
        EXPORT.verify_archive(args.archive.resolve(), metadata['files'], install)
        manifest = EXPORT.RUNTIME_BUNDLE.verify(install, metadata['version'], metadata['commit'], metadata['target'])
        report.update(target=metadata['target'], publisher=manifest['rustc'], bundle_id=manifest['bundle_id'])
        project = root / 'project'
        project.mkdir()
        (project / 'hello.ku').write_bytes(EXPORT.canonical_example((repo / 'examples/hello.ku').read_bytes()))
        rustup = management_invocation('rustup')
        if not rustup:
            raise ValueError('readonly CI setup requires rustup')
        before = checked([rustup, 'default'], project, env)
        report['global_default_before'] = before
        ku = install / 'ku.exe'
        # Setup is explicit and outside the RX probe. No toolchain is cloned.
        ready = invoke([ku, 'doctor', '--backend', 'runner', '--json'], project, env)
        report['setup_toolchain_install_performed'] = ready.returncode != 0
        if ready.returncode:
            prepared = invoke([ku, 'doctor', '--install-runner-toolchain'], project, env, seconds=660)
            if prepared.returncode:
                raise RuntimeError('explicit readonly toolchain setup failed: ' + prepared.stderr.decode('utf-8', errors='replace')[-2000:])
            ready = invoke([ku, 'doctor', '--backend', 'runner', '--json'], project, env)
        if ready.returncode:
            raise RuntimeError('readonly runner setup not ready: ' + ready.stderr.decode('utf-8', errors='replace')[-2000:])
        report['setup_doctor'] = json.loads(output(ready))
        doctor = report['setup_doctor']
        selected = doctor['selected_compiler']
        system = doctor['system_rustc']['identity']
        if (selected['link_probe_passed'] is not True
                or any(selected['identity'][key] != manifest['rustc'][key] for key in ('release', 'commit_hash', 'commit_date', 'host'))
                or doctor['required_rustc'] != manifest['rustc']):
            raise RuntimeError('readonly setup did not verify the exact SDK compiler/required identity')
        if system['release'] != args.consumer_toolchain or system['release'] == manifest['rustc']['release']:
            raise RuntimeError('readonly supplement requires the declared unlike system compiler')
        report['consumer'] = system
        windows_readonly_installation(install, project, root, env, report, args.output, metadata['version'])
        if checked([rustup, 'default'], project, env) != before:
            raise RuntimeError('readonly setup changed the global Rust default')
        report.update(global_default_unchanged=True, toolchain_clone=False)
    report.update(status='passed', owned_temp_removed=True)


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
        native_env = native_without_rust(env, root / 'absent-rustup-for-native')

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

        def failure(name, selected_env=None, backend='runner'):
            destination = project / ("preserved.exe" if os.name == "nt" else "preserved")
            destination.write_bytes(b"preexisting output must survive\n")
            selection = [] if backend is None else ['--backend', backend]
            result = invoke([ku, "build", *selection, '--verbose', "hello.ku", "-o", destination], project, selected_env or env)
            record(name, result, "nonzero-without-bare-E0514")
            text = output(result) + result.stderr.decode(errors="replace")
            if result.returncode == 0 or "E0514" in text or destination.read_bytes() != b"preexisting output must survive\n":
                raise RuntimeError(name + " did not fail before compiler/output replacement: " + text[-4096:])
            if backend is None and ('KU_CC' not in text or 'rustc command:' in text):
                raise RuntimeError(name + ' did not clearly reject the unavailable C compiler without Runner fallback')

        def remove_executable(path):
            path = Path(path)
            if os.name == 'nt' and path.suffix.lower() != '.exe':
                path = path.with_suffix('.exe')
            if not path.resolve().is_relative_to(root):
                raise ValueError('generated executable cleanup escaped owned root')
            if path.is_file():
                path.unlink()

        def execute_and_remove(name, destination, wanted=None, selected_env=None):
            executable = Path(destination)
            if os.name == 'nt' and executable.suffix.lower() != '.exe':
                executable = executable.with_suffix('.exe')
            executed = invoke([executable], project, selected_env or env)
            record(name, executed)
            if executed.returncode or output(executed) != (expected if wanted is None else wanted):
                raise RuntimeError(name + ' produced incorrect executable output')
            report['cases'][-1]['binary_sha256'] = EXPORT.file_hash(executable)
            remove_executable(executable)

        def build_run(name, arguments, destination, verbose_profile=None, entry="hello.ku", wanted=None, backend='c'):
            selected_env = native_env if backend == 'c' else env
            arguments = list(arguments)
            if not any(flag in arguments for flag in ('--verbose', '-v')):
                arguments.append('--verbose')
            result = success(name, ['build', *arguments, '-o', destination, entry], selected_env=selected_env)
            assert_build_backend(result, backend, verbose_profile)
            report['cases'][-1]['observed_backend'] = backend
            if verbose_profile is not None:
                report['cases'][-1]['observed_build_profile'] = verbose_profile
            execute_and_remove(name + '-result', destination, wanted, selected_env)
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
                result = success('contract-output-' + flag, ['build', '--verbose', flag, destination, 'hello.ku'], selected_env=native_env)
                assert_build_backend(result, 'c', 'debug')
                executed = invoke([destination], project, native_env)
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
                selected_backend = 'c' if backend in ('c', 'native-c') else 'runner'
                build_run('contract-backend-' + backend, ['--backend', backend], project / ('contract-backend' + suffix), backend=selected_backend)
            # LLVM is an explicitly limited text prototype. The ordinary
            # Hello cases above remain unchanged; this empty-main IR fixture
            # is supported and its default C executable emits no stdout.
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
                    built = success('contract-project-build-' + label, ['build', '--verbose', *arguments], selected_env=native_env)
                    assert_build_backend(built, 'c', 'debug')
                    program = project / '.ku/build/debug' / ('hello' + suffix)
                    result = invoke([program], project, native_env)
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
        build_run('default-native-without-rust-before-runner-setup', [], project / 'default-native-before-runner', 'debug')
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
        installed_runner_embedded_entry_consumer(install, root, env, report, target)
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
                    built = success('cross-drive-original-build', ['build', '--backend', 'runner', '--verbose', 'hello.ku', '-o', 'cross-drive-result'])
                    assert_build_backend(built, 'runner', 'debug')
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
        original = success("original-build-project-and-env-unlike", ["build", '--backend', 'runner', '--verbose', "hello.ku"], selected_env=configured)
        assert_build_backend(original, 'runner', 'debug')
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
        built = success("ignore-newer-incompatible-rlib", ["build", '--backend', 'runner', '--verbose', "hello.ku", "-o", "newer-ignored"])
        assert_build_backend(built, 'runner', 'debug')
        execute_and_remove("ignore-newer-incompatible-rlib-result", project / "newer-ignored")
        wrong.unlink()
        runtime_path = install / "ku-runtime.json"
        deps_path = install / "ku-runtime-deps.json"
        original_manifest = runtime_path.read_bytes()
        with changed(runtime_path, b"{invalid json"):
            failure("invalid-manifest")
            build_run('default-native-does-not-read-runner-manifest', [], project / 'native-without-runner-manifest', 'debug')
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
        mixed["runtime_target"] = ("x86_64-pc-windows-msvc" if target == "x86_64-pc-windows-gnu"
                                   else "x86_64-pc-windows-gnu" if os.name == "nt"
                                   else "x86_64-pc-windows-msvc")
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
        failure('default-native-missing-compiler-no-fallback', dict(env, KU_CC=str(root / 'missing-c-compiler')), backend=None)
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
        built = success("standalone-explicit-compiler-no-rustup", ["build", '--backend', 'runner', '--verbose', "hello.ku", "-o", "standalone-result"], selected_env=direct)
        assert_build_backend(built, 'runner', 'debug')
        execute_and_remove("standalone-explicit-compiler-result", project / "standalone-result")
        # Common immutable env for both processes; no per-thread env mutation.
        OWNER.COMMAND_TIMEOUT_SECONDS = 300
        with environment(env), ThreadPoolExecutor(max_workers=2) as pool:
            futures = [pool.submit(OWNER.run_bounded, [str(ku), "build", '--backend', 'runner', '--verbose', "hello.ku", "-o", str(project / ("parallel-" + str(i)))], project, "parallel Runner consumer", check=False) for i in range(2)]
            for i, future in enumerate(futures):
                result = future.result()
                record("parallel-build-" + str(i), result)
                if result.returncode:
                    raise RuntimeError("parallel build failed")
                assert_build_backend(result, 'runner', 'debug')
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
                build_run('source-and-target-unavailable', [], project / 'without-source', 'debug')
                installed_native_tls_consumer(install, root, env, report, args.output, target, checkout_hidden=True)
                installed_native_compression_consumer(install, root, env, report, args.output, target, checkout_hidden=True)
                installed_native_time_array_consumer(install, root, env, report, args.output, target, checkout_hidden=True)
                installed_native_http_client_consumer(install, root, env, report, args.output, target, checkout_hidden=True)
                installed_native_http_client_consumer(install, root, env, report, args.output, target, checkout_hidden=True, task_client=True)
                installed_native_http_client_consumer(install, root, env, report, args.output, target, checkout_hidden=True, structured_task=True)
                installed_native_http_relay_consumer(install, root, env, report, args.output, target, checkout_hidden=True)
            finally:
                for old, destination in reversed(retired):
                    relocate_checkout_entry(destination, old)
        else:
            report["cases"].append({"name": "source-and-target-unavailable", "status": "not_requested_local;CI flag required"})
            installed_native_tls_consumer(install, root, env, report, args.output, target)
            installed_native_compression_consumer(install, root, env, report, args.output, target)
            installed_native_time_array_consumer(install, root, env, report, args.output, target)
            installed_native_http_client_consumer(install, root, env, report, args.output, target)
            installed_native_http_client_consumer(install, root, env, report, args.output, target, task_client=True)
            installed_native_http_client_consumer(install, root, env, report, args.output, target, structured_task=True)
            installed_native_http_relay_consumer(install, root, env, report, args.output, target)
        exercise_cli_contracts()
        # Exercise the native path separately; it must not consume runner rlibs.
        success("native-doctor", ["doctor", "--backend", "c"])
        native = project / ("hello-native.exe" if os.name == "nt" else "hello-native")
        compiled = success("native-build-independent", ["build", "--backend", "c", "--release", '--verbose', "hello.ku", "-o", native], selected_env=native_env)
        assert_build_backend(compiled, 'c', 'release')
        result = invoke([native], root, native_env)
        record("native-result", result)
        if result.returncode or output(result) != expected:
            raise RuntimeError("native guard output differs")
        remove_executable(native)
        # Enforce read-only installation, not only a no-write assertion.
        if os.name == "nt":
            windows_readonly_installation(install, project, root, env, report, args.output, metadata['version'])
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
                built = success("read-only-install", ["build", '--verbose', "hello.ku", "-o", "readonly-result"], selected_env=native_env)
                assert_build_backend(built, 'c', 'debug')
                execute_and_remove("read-only-install-result", project / "readonly-result", selected_env=native_env)
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
    parser.add_argument("--archive", type=Path)
    parser.add_argument("--manifest", type=Path)
    parser.add_argument("--native-time-array-probe-bundle", type=Path)
    parser.add_argument("--repo", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--consumer-toolchain", default="1.96.0")
    parser.add_argument("--hide-checkout", action="store_true")
    parser.add_argument("--readonly-only", action="store_true")
    parser.add_argument("--structured-only", action="store_true")
    parser.add_argument("--bootstrap-generations", action="store_true")
    parser.add_argument("--installer-run")
    parser.add_argument("--expected-sdk-commit")
    args = parser.parse_args()
    if args.native_time_array_probe_bundle:
        if args.archive or args.manifest or args.readonly_only or args.structured_only or args.hide_checkout or not args.expected_sdk_commit:
            parser.error('early native probe requires --expected-sdk-commit and cannot consume an archive or other mode')
    elif not args.archive or not args.manifest:
        parser.error('--archive and --manifest are required outside the early native probe')
    if args.structured_only and (args.readonly_only or args.hide_checkout or not args.expected_sdk_commit):
        parser.error('--structured-only requires --expected-sdk-commit and cannot run other verification modes')
    if args.bootstrap_generations and not args.structured_only:
        parser.error('--bootstrap-generations requires --structured-only for the source-free installer')
    args.output = args.output.resolve()
    args.output.parent.mkdir(parents=True, exist_ok=True)
    report = {"status": "running"}
    try:
        selected = (run_native_time_array_probe if args.native_time_array_probe_bundle else
                    run_structured_only if args.structured_only else
                    run_readonly_only if args.readonly_only else run)
        selected(args, report)
    except BaseException as error:
        report.update(status="failed", failure=type(error).__name__ + ": " + str(error)[-6000:])
        raise
    finally:
        write_report(args.output, report)


if __name__ == "__main__":
    main()
