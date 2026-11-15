#!/usr/bin/env python3
"""Compression-pack ownership, provenance and target regressions; no builds."""
import copy
from contextlib import redirect_stdout
import hashlib
import importlib.util
import io
import json
import os
from pathlib import Path
import shutil
import struct
import subprocess
import sys
import tarfile
import tempfile
import unittest
from unittest import mock
from types import SimpleNamespace

sys.dont_write_bytecode = True
SPEC = importlib.util.spec_from_file_location('native_compression_builder', Path(__file__).with_name('build-native-compression.py'))
assert SPEC and SPEC.loader
B = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(B)


def object_bytes(target):
    if target == 'aarch64-apple-darwin':
        return struct.pack('<IIIIIIII', 0xfeedfacf, 0x100000c, 0, 1, 0, 0, 0, 0)
    if target.endswith('linux-gnu'):
        data = bytearray(64)
        data[:7] = b'\x7fELF\x02\x01\x01'
        struct.pack_into('<HH', data, 16, 1, 183 if target.startswith('aarch64-') else 62)
        return bytes(data)
    return struct.pack('<HHIIIHH', 0x8664, 1, 0, 0, 0, 0, 0) + bytes(40)


def static_archive(body):
    header = f'{"fixture.o/":<16}{0:<12}{0:<6}{0:<6}{100644:<8}{len(body):<10}`\n'.encode()
    assert len(header) == 60
    return b'!<arch>\n' + header + body + (b'\n' if len(body) & 1 else b'')


class CompressionPackTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory(prefix='ku-compression-contract-')
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name).resolve()

    def tarball(self, files):
        path = self.root/'source.tar.gz'
        with tarfile.open(path, 'w:gz') as output:
            for name, contents in files.items():
                member = tarfile.TarInfo(name)
                member.size = len(contents)
                output.addfile(member, io.BytesIO(contents))
        return path

    def pinned_fixture(self, path):
        spec = copy.deepcopy(B.SOURCES['zlib'])
        spec['sha256'] = hashlib.sha256(path.read_bytes()).hexdigest()
        return mock.patch.dict(B.SOURCES, {'zlib': spec})

    def test_bad_source_hash_is_rejected_before_extracting(self):
        source = self.root/'wrong.tar.gz'
        source.write_bytes(b'not the publisher archive')
        output = self.root/'extract'
        output.mkdir()
        with self.assertRaisesRegex(ValueError, 'pinned official release'):
            B.extract_source(source, output, 'zlib')
        self.assertEqual(list(output.iterdir()), [])

    def test_verified_archive_still_cannot_escape_its_owned_tree(self):
        source = self.tarball({'zlib-1.4/../../escape': b'must not escape'})
        output = self.root/'extract'
        output.mkdir()
        with self.pinned_fixture(source), self.assertRaisesRegex(ValueError, 'unexpected path'):
            B.extract_source(source, output, 'zlib')
        self.assertFalse((self.root/'escape').exists())

    def test_static_archive_rejects_wrong_target_and_thin_archive(self):
        path = self.root/'library.a'
        for target in B.TARGETS:
            with self.subTest(target=target):
                path.write_bytes(static_archive(object_bytes(target)))
                self.assertEqual(B.validate_archive(path, target), 1)
                wrong = next(t for t in B.TARGETS if t != target)
                with self.assertRaisesRegex(ValueError, 'wrong-target'):
                    B.validate_archive(path, wrong)
        path.write_bytes(b'!<thin>\n' + b'not a self-contained library')
        with self.assertRaisesRegex(ValueError, 'normal static archive'):
            B.validate_archive(path, 'x86_64-unknown-linux-gnu')

    def test_native_host_gate_keeps_linux_arm64_without_fake_cross_build(self):
        with mock.patch.object(B.platform, 'system', return_value='Linux'), mock.patch.object(B.platform, 'machine', return_value='aarch64'):
            B.host_target('aarch64-unknown-linux-gnu')
            with self.assertRaisesRegex(ValueError, 'matching native host'):
                B.host_target('x86_64-unknown-linux-gnu')

    def test_fetch_is_explicit_and_default_build_does_not_access_network(self):
        arguments = ['build-native-compression.py', '--root', str(self.root), '--target', 'x86_64-unknown-linux-gnu', '--library', 'zlib']
        with mock.patch.object(sys, 'argv', arguments), mock.patch.object(B, 'host_target'), mock.patch.object(B, 'build_one') as build, mock.patch.object(B, 'fetch_source') as fetch:
            B.main()
            fetch.assert_not_called()
            self.assertIsNone(build.call_args.args[3])
        source = self.root / 'explicit fetched archive.tar.gz'
        with mock.patch.object(sys, 'argv', [*arguments, '--fetch']), mock.patch.object(B, 'host_target'), mock.patch.object(B, 'build_one') as build, mock.patch.object(B, 'fetch_source', return_value=source) as fetch:
            B.main()
            fetch.assert_called_once_with(self.root, 'zlib')
            self.assertEqual(build.call_args.args[3], source)

    def fetch_response(self, payload, *, url=None, length=None, failure=False,
                       content_type='application/gzip', encoding=''):
        class Response(io.BytesIO):
            status = 200
            headers = {'Content-Length': str(len(payload) if length is None else length),
                       'Content-Type': content_type, 'Content-Encoding': encoding}
            def geturl(self):
                return url or B.SOURCES['zlib']['url']
            def read(self, size=-1):
                if failure:
                    raise TimeoutError('fixture connection interrupted')
                return super().read(size)
        return Response(payload)

    def test_explicit_fetch_verifies_and_atomically_reuses_official_source(self):
        payload = b'fixture official tarball bytes'
        spec = copy.deepcopy(B.SOURCES['zlib'])
        spec['sha256'] = hashlib.sha256(payload).hexdigest()
        opener = SimpleNamespace(open=mock.Mock(side_effect=lambda request, timeout: self.fetch_response(payload)))
        with mock.patch.dict(B.SOURCES, {'zlib': spec}), mock.patch.object(B.urllib.request, 'build_opener', return_value=opener):
            path = B.fetch_source(self.root, 'zlib')
            self.assertEqual(path.read_bytes(), payload)
            self.assertEqual(path.name, 'zlib-1.4.tar.gz')
            self.assertEqual(opener.open.call_args.kwargs['timeout'], 30)
            self.assertEqual(opener.open.call_args.args[0].full_url, spec['url'])
            self.assertEqual(B.fetch_source(self.root, 'zlib'), path)
            self.assertEqual(opener.open.call_count, 1)
        self.assertFalse(any(self.root.rglob('*.partial')))

    def test_zlib_official_fallback_keeps_same_pin_and_reports_both_payload_identities(self):
        good = b'\x1f\x8bverified official fixture archive'
        bad = b'<!DOCTYPE html>private-response-body-must-not-be-logged'
        spec = copy.deepcopy(B.SOURCES['zlib'])
        self.assertEqual(spec['sha256'], '02dd159783677a749c04adbdd02b55b8a2bcc7e054f51b76b549199a916ad584')
        spec['sha256'] = hashlib.sha256(good).hexdigest()
        urls = []

        def opened(request, timeout):
            urls.append(request.full_url)
            self.assertEqual(timeout, 30)
            alternate = request.full_url == 'https://github.com/madler/zlib/releases/download/v1.4/zlib-1.4.tar.gz'
            return self.fetch_response(good if alternate else bad, url=request.full_url,
                                       content_type='application/gzip' if alternate else 'text/html',
                                       encoding='' if alternate else 'gzip')

        log = io.StringIO()
        with mock.patch.dict(B.SOURCES, {'zlib': spec}), mock.patch.object(B.urllib.request, 'build_opener', return_value=SimpleNamespace(open=opened)), redirect_stdout(log):
            path = B.fetch_source(self.root, 'zlib')
        self.assertEqual(urls, [spec['url'], 'https://github.com/madler/zlib/releases/download/v1.4/zlib-1.4.tar.gz'])
        self.assertEqual(path.read_bytes(), good)
        lines = log.getvalue().splitlines()
        failed = json.loads(lines[0].removeprefix('Compression source attempt failed: '))
        self.assertEqual(failed['failure'], 'sha256_mismatch')
        self.assertEqual(failed['size'], len(bad))
        self.assertEqual(failed['actual_sha256'], hashlib.sha256(bad).hexdigest())
        self.assertEqual(failed['expected_sha256'], spec['sha256'])
        self.assertEqual(failed['content_type'], 'text/html')
        self.assertEqual(failed['content_encoding'], 'gzip')
        self.assertEqual(failed['magic16_hex'], bad[:16].hex())
        self.assertEqual(failed['sha256_scope'], 'complete_response')
        succeeded = json.loads(lines[1].removeprefix('Fetched verified official compression source: '))
        self.assertEqual(succeeded['source_url'], urls[1])
        self.assertEqual(succeeded['actual_sha256'], spec['sha256'])
        self.assertNotIn('private-response-body-must-not-be-logged', log.getvalue())
        self.assertEqual(B.SOURCES['zlib']['url'], spec['url'])
        self.assertFalse(any(self.root.rglob('*.partial')))

    def test_zlib_fallback_is_bounded_and_does_not_log_transport_exception_text(self):
        url_error = B.urllib.error.URLError('https://example.invalid/private?token=do-not-log')
        opener = SimpleNamespace(open=mock.Mock(side_effect=url_error))
        log = io.StringIO()
        with mock.patch.object(B.urllib.request, 'build_opener', return_value=opener), redirect_stdout(log), self.assertRaises(ValueError) as raised:
            B.fetch_source(self.root, 'zlib')
        self.assertEqual(opener.open.call_count, 2)
        self.assertNotIn('do-not-log', log.getvalue() + str(raised.exception))
        rows = [json.loads(line.removeprefix('Compression source attempt failed: ')) for line in log.getvalue().splitlines()]
        self.assertEqual([row['source_url'] for row in rows], [B.SOURCES['zlib']['url'], 'https://github.com/madler/zlib/releases/download/v1.4/zlib-1.4.tar.gz'])
        self.assertTrue(all(row['size'] == 0 and row['magic16_hex'] == '' and row['sha256_scope'] == 'received_prefix' for row in rows))
        self.assertEqual(list((self.root / 'sources').iterdir()), [])

    def test_failed_fetch_never_leaves_partial_or_replaces_bad_cached_source(self):
        payload = b'fixture source'
        for reason in ('bad-sha', 'oversize', 'bad-host', 'interrupted'):
            with self.subTest(reason=reason), tempfile.TemporaryDirectory(prefix='fetch-', dir=self.root) as directory:
                cache = Path(directory)
                def response(request, timeout):
                    return self.fetch_response(payload,
                        url='https://unrelated.invalid/source.tar.gz' if reason == 'bad-host' else request.full_url,
                        length=B.MAX_SOURCE_ARCHIVE + 1 if reason == 'oversize' else None,
                        failure=reason == 'interrupted')
                opener = SimpleNamespace(open=mock.Mock(side_effect=response))
                with mock.patch.object(B.urllib.request, 'build_opener', return_value=opener), self.assertRaises((ValueError, TimeoutError)):
                    B.fetch_source(cache, 'zlib')
                self.assertEqual(opener.open.call_count, 2)
                self.assertEqual(list((cache / 'sources').iterdir()), [])
        cached = self.root / 'sources/zlib-1.4.tar.gz'
        cached.parent.mkdir()
        cached.write_bytes(b'existing wrong source identity')
        with mock.patch.object(B.urllib.request, 'build_opener') as network, self.assertRaisesRegex(ValueError, 'refusing overwrite'):
            B.fetch_source(self.root, 'zlib')
        network.assert_not_called()
        self.assertEqual(cached.read_bytes(), b'existing wrong source identity')

    def test_pack_publish_and_reuse_preserve_identity_and_reject_mutation(self):
        target = ('x86_64-pc-windows-msvc' if os.name == 'nt' else
                  'aarch64-apple-darwin' if sys.platform == 'darwin' else 'x86_64-unknown-linux-gnu')
        source = self.tarball({'zlib-1.4/LICENSE': b'original upstream license bytes\n',
                              'zlib-1.4/zlib.h': b'#define ZLIB_VERSION "1.4"\n',
                              'zlib-1.4/zconf.h': b'/* fixture generated ABI header */\n',
                              'zlib-1.4/configure': b'# fixture only; never executed\n'})
        cache = self.root/'private cache 中文'
        cache.mkdir()
        commands, state = [], {}

        def run(command, cwd, env, timeout=600):
            # Compiler/build processes are intentionally mocked here. Actual
            # native link/API tests belong to the product integration gate.
            command = [str(value) for value in command]
            commands.append(command)
            if '--version' in command:
                return subprocess.CompletedProcess(command, 0, b'fixture compiler identity\n', b'')
            if os.name == 'nt':
                if '-S' in command:
                    state['source'] = Path(command[command.index('-S')+1])
                    build = Path(command[command.index('-B')+1])
                    state['build'] = build
                    build.mkdir()
                    (build/'zconf.h').write_bytes(b'/* fixture generated ABI header */\n')
                    info = build/'CMakeFiles/3.31/CMakeCCompiler.cmake'
                    info.parent.mkdir(parents=True)
                    info.write_text('set(CMAKE_C_COMPILER "'+Path(sys.executable).as_posix()+'")\nset(CMAKE_C_COMPILER_ID "MSVC")\nset(CMAKE_C_COMPILER_VERSION "fixture")\n')
                    self.assertIn('-DCMAKE_MSVC_RUNTIME_LIBRARY=MultiThreadedDLL', command)
                else:
                    self.assertIn('--parallel', command)
                    self.assertEqual(command[-1], '1')
                    lib = state['build']/'Release/libzs.lib'
                    lib.parent.mkdir()
                    lib.write_bytes(static_archive(object_bytes(target)))
            elif 'configure' in command:
                self.assertIn('--static', command)
                state['prefix'] = Path(next(word.split('=', 1)[1] for word in command if word.startswith('--prefix=')))
                self.assertFalse(state['prefix'].is_relative_to(Path('/usr/local')))
                self.assertEqual(env['CFLAGS'], '-O2 -fPIC')
            elif command[-1] == 'libz.a':
                self.assertIn('-j1', command)
                (cwd/'libz.a').write_bytes(static_archive(object_bytes(target)))
            else:
                self.assertEqual(command[-2:], ['-j1', 'install'])
                prefix = state['prefix']
                (prefix/'lib').mkdir(parents=True)
                (prefix/'include').mkdir()
                shutil.copyfile(cwd/'libz.a', prefix/'lib/libz.a')
                for name in ('zlib.h', 'zconf.h'):
                    shutil.copyfile(cwd/name, prefix/'include'/name)
            return subprocess.CompletedProcess(command, 0, b'', b'')

        with self.pinned_fixture(source), mock.patch.object(B, 'run', side_effect=run):
            manifest = B.build_one(cache, target, 'zlib', source, cc=sys.executable, cmake=sys.executable)
            leaf = cache/'native-compression/v1'/target/'zlib'
            self.assertTrue(leaf.is_dir())
            self.assertEqual((leaf/'LICENSE').read_bytes(), b'original upstream license bytes\n')
            self.assertEqual(manifest['target'], target)
            self.assertEqual(manifest['build']['optimization'], 'O2')
            self.assertFalse(manifest['build']['multithread'])
            before = {p.relative_to(leaf).as_posix(): p.read_bytes() for p in leaf.rglob('*') if p.is_file()}
            with mock.patch.object(B, 'run') as repeated:
                self.assertEqual(B.build_one(cache, target, 'zlib', None), manifest)
                repeated.assert_not_called()
            self.assertEqual({p.relative_to(leaf).as_posix(): p.read_bytes() for p in leaf.rglob('*') if p.is_file()}, before)
            (leaf/'include/zlib.h').write_bytes(b'changed cached header')
            with self.assertRaisesRegex(ValueError, 'changed or mixed'):
                B.validate_pack(leaf, 'zlib', target)


if __name__ == '__main__':
    unittest.main()
