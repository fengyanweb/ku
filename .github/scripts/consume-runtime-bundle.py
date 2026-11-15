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
import re
import shutil
import socket
import ssl
import stat
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
    """Preserve bounded stderr and reject known CRT/link-contract warnings."""
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
    detail['build_link_contract_diagnostics'] = codes
    detail['build_link_contract_passed'] = result.returncode == 0 and not codes
    if codes:
        raise RuntimeError('native build reported incompatible CRT/linker diagnostics: ' + ', '.join(codes))


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
            finally:
                for old, destination in reversed(retired):
                    relocate_checkout_entry(destination, old)
        else:
            report["cases"].append({"name": "source-and-target-unavailable", "status": "not_requested_local;CI flag required"})
            installed_native_tls_consumer(install, root, env, report, args.output, target)
            installed_native_compression_consumer(install, root, env, report, args.output, target)
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
    parser.add_argument("--archive", type=Path, required=True)
    parser.add_argument("--manifest", type=Path, required=True)
    parser.add_argument("--repo", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--consumer-toolchain", default="1.96.0")
    parser.add_argument("--hide-checkout", action="store_true")
    parser.add_argument("--readonly-only", action="store_true")
    parser.add_argument("--installer-run")
    parser.add_argument("--expected-sdk-commit")
    args = parser.parse_args()
    args.output = args.output.resolve()
    args.output.parent.mkdir(parents=True, exist_ok=True)
    report = {"status": "running"}
    try:
        (run_readonly_only if args.readonly_only else run)(args, report)
    except BaseException as error:
        report.update(status="failed", failure=type(error).__name__ + ": " + str(error)[-6000:])
        raise
    finally:
        write_report(args.output, report)


if __name__ == "__main__":
    main()
