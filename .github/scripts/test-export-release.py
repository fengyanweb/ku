#!/usr/bin/env python3
"""Bounded release archive and attribution regression tests (no network)."""

from __future__ import annotations

import copy
import contextlib
import gzip
import hashlib
import importlib.util
import io
import json
import os
import re
import shutil
import socket
import struct
import subprocess
from pathlib import Path
import sys
import tarfile
import tempfile
import time
from types import SimpleNamespace
import unittest
from unittest import mock

sys.dont_write_bytecode = True
SPEC = importlib.util.spec_from_file_location("ku_export", Path(__file__).with_name("export-release.py"))
assert SPEC and SPEC.loader
EXPORT = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(EXPORT)


def compression_pack_fixture(root, target, codec):
    """Native object headers suffice for packaging tests, never runtime proof."""
    builder = EXPORT.NATIVE_COMPRESSION
    leaf = root / 'native-compression/v1' / target / codec
    (leaf / 'lib').mkdir(parents=True)
    (leaf / 'include').mkdir()
    if target == 'aarch64-apple-darwin':
        body = struct.pack('<IIIIIIII', 0xfeedfacf, 0x100000c, 0, 1, 0, 0, 0, 0)
    elif target.endswith('linux-gnu'):
        body = bytearray(64)
        body[:7] = b'\x7fELF\x02\x01\x01'
        struct.pack_into('<HH', body, 16, 1, 183 if target.startswith('aarch64-') else 62)
        body = bytes(body)
    else:
        body = struct.pack('<HHIIIHH', 0x8664, 1, 0, 0, 0, 0, 0) + bytes(40)
    header = f'{"fixture.o/":<16}{0:<12}{0:<6}{0:<6}{100644:<8}{len(body):<10}`\n'.encode()
    library = leaf / 'lib' / builder.archive_name(codec, target)
    library.write_bytes(b'!<arch>\n' + header + body + (b'\n' if len(body) & 1 else b''))
    for name in builder.HEADER_NAMES[codec]:
        text = ('#define ZLIB_VERSION "1.4"\n' if codec == 'zlib' else
                '#define ZSTD_VERSION_MAJOR 1\n#define ZSTD_VERSION_MINOR 5\n#define ZSTD_VERSION_RELEASE 7\n')
        (leaf / 'include' / name).write_text(text, encoding='utf-8')
    (leaf / 'LICENSE').write_text('upstream fixture license bytes\n', encoding='utf-8')
    _, _, object_format, abi, crt = builder.TARGETS[target]
    source = builder.SOURCES[codec]
    manifest = {'format': 'ku-native-compression-pack-v1', 'codec': codec,
                'version': source['version'], 'target': target,
                'object_format': object_format, 'abi': abi, 'crt': crt,
                'archive': builder.record_file(library),
                'headers': [builder.record_file(leaf / 'include' / name) for name in builder.HEADER_NAMES[codec]],
                'license': dict(builder.record_file(leaf / 'LICENSE'), spdx=source['license']),
                'source': {'url': source['url'], 'sha256': source['sha256']},
                'runtime_dependency': 'none',
                'build': {'profile': 'Release', 'optimization': 'O2', 'multithread': False}}
    (leaf / 'manifest.json').write_text(json.dumps(manifest), encoding='utf-8')
    return leaf


def calendar_pack_fixture(root, target):
    """Minimal object-container fixture; does not claim executable ABI proof."""
    builder = EXPORT.NATIVE_CALENDAR
    leaf = root / 'native-calendar/v1' / target
    (leaf / 'lib').mkdir(parents=True)
    (leaf / 'include').mkdir()
    body = bytearray(64)
    body[:7] = b'\x7fELF\x02\x01\x01'
    struct.pack_into('<HH', body, 16, 1, 62)
    body = bytes(body) + builder.BUILD_ID.encode()
    ar_header = f'{"fixture.o/":<16}{0:<12}{0:<6}{0:<6}{100644:<8}{len(body):<10}`\n'.encode()
    library = leaf / 'lib' / builder.archive_name(target)
    library.write_bytes(b'!<arch>\n' + ar_header + body + (b'\n' if len(body) & 1 else b''))
    shutil.copyfile(builder.ROOT / 'crates/ku-native-calendar/include' / builder.HEADER, leaf / 'include' / builder.HEADER)
    shutil.copyfile(builder.ROOT / 'LICENSE', leaf / 'LICENSE.ku')
    (leaf / 'LICENSE.chrono').write_bytes(b'chrono dependency license fixture\n')
    manifest = {'schema_version': 1, 'abi_version': 1, 'build_id': builder.BUILD_ID,
                'crate_version': '0.1.0', 'chrono_version': '0.4.45',
                'rustc_version': '1.99.1', 'rustc_verbose': 'rustc 1.99.1\nrelease: 1.99.1\n',
                'target': target, 'source_sha256': builder.source_hash(),
                'archive': builder.record(library),
                'header': builder.record(leaf / 'include' / builder.HEADER),
                'native_static_libs': {'raw': '-lc -lm', 'tokens': ['-lc', '-lm']},
                'licenses': [builder.record(leaf / 'LICENSE.ku'), builder.record(leaf / 'LICENSE.chrono')]}
    (leaf / 'manifest.json').write_text(json.dumps(manifest), encoding='utf-8')
    return leaf


def http_client_pack_fixture(root, target):
    """Object-container fixture, never a claim of executable runtime proof."""
    builder = EXPORT.NATIVE_HTTP_CLIENT
    leaf = root / 'native-http/v1' / target
    (leaf/'lib').mkdir(parents=True)
    (leaf/'include').mkdir()
    body=bytearray(64);body[:7]=b'\x7fELF\x02\x01\x01'
    struct.pack_into('<HH',body,16,1,62)
    body=bytes(body)+builder.BUILD_ID.encode()
    header=f'{"fixture.o/":<16}{0:<12}{0:<6}{0:<6}{100644:<8}{len(body):<10}`\n'.encode()
    library=leaf/'lib'/builder.archive_name(target)
    library.write_bytes(b'!<arch>\n'+header+body+(b'\n' if len(body)&1 else b''))
    shutil.copyfile(builder.ROOT/'crates/ku-native-http-client/include'/builder.HEADER,leaf/'include'/builder.HEADER)
    shutil.copyfile(builder.ROOT/'LICENSE',leaf/'LICENSE.ku')
    (leaf/'LICENSE.dependencies').write_bytes(b'HTTP dependency license fixture\n')
    manifest={'schema_version':1,'abi_version':1,'build_id':builder.BUILD_ID,'crate_version':'0.1.0','ureq_version':'2.12.1',
        'rustc_version':'1.99.1','rustc_verbose':'rustc 1.99.1\nrelease: 1.99.1\n','target':target,'source_sha256':builder.source_hash(),
        'archive':builder.record(library),'header':builder.record(leaf/'include'/builder.HEADER),
        'native_static_libs':{'raw':'-lc -lm','tokens':['-lc','-lm']},
        'licenses':[builder.record(leaf/'LICENSE.ku'),builder.record(leaf/'LICENSE.dependencies')]}
    (leaf/'manifest.json').write_text(json.dumps(manifest),encoding='utf-8')
    return leaf


class CalendarCoffArchiveTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory(prefix='ku-calendar-coff-')
        self.addCleanup(temporary.cleanup)
        self.path = Path(temporary.name)/'calendar.lib'
        self.builder = EXPORT.NATIVE_CALENDAR

    @staticmethod
    def ar(objects):
        result = bytearray(b'!<arch>\n')
        for name, body in objects:
            header = f'{name + "/":<16}{0:<12}{0:<6}{0:<6}{100644:<8}{len(body):<10}`\n'.encode()
            result += header + body + (b'\n' if len(body) & 1 else b'')
        return result

    def coff(self, *, big=False, names=None, identity=None, section=b'.rdata'):
        names = sorted(self.builder.CALENDAR_API) if names is None else names
        identity = self.builder.BUILD_ID.encode() if identity is None else identity
        header, entry = (56, 20) if big else (20, 18)
        data = header + 40
        symbols = data + len(identity)
        strings, offsets = bytearray(b'\0'), []
        for name in names:
            offsets.append(len(strings) + 4)
            strings += name + b'\0'
        string_offset = symbols + len(names) * entry
        body = bytearray(string_offset + 4 + len(strings))
        if big:
            struct.pack_into('<HHHH', body, 0, 0, 0xffff, 2, 0x8664)
            body[12:28] = self.builder.BIGOBJ_CLASS
            struct.pack_into('<III', body, 44, 1, symbols, len(names))
        else:
            struct.pack_into('<HH', body, 0, 0x8664, 1)
            struct.pack_into('<II', body, 8, symbols, len(names))
        body[header:header+len(section)] = section
        struct.pack_into('<II', body, header+16, len(identity), data)
        struct.pack_into('<I', body, header+36, 0x40)
        body[data:data+len(identity)] = identity
        for index, offset in enumerate(offsets):
            start = symbols + index * entry
            struct.pack_into('<I', body, start+4, offset)
            struct.pack_into('<i' if big else '<h', body, start+12, 1)
            body[start+entry-2] = 2
        struct.pack_into('<I', body, string_offset, len(strings)+4)
        body[string_offset+4:] = strings
        return body

    @staticmethod
    def short_import(name=b'Sleep', dll=b'kernel32.dll', flags=4):
        payload = name + b'\0' + dll + b'\0'
        return bytearray(struct.pack('<HHHHIIHH', 0, 0xffff, 0, 0x8664, 0, len(payload), 0, flags)+payload)

    def validate(self, objects, target='x86_64-pc-windows-msvc'):
        self.path.write_bytes(self.ar(objects))
        return self.builder.validate_archive(self.path, target)

    def test_windows_gnu_selects_a_gnu_host_and_its_own_archive_contract(self):
        target = 'x86_64-pc-windows-gnu'
        self.assertEqual(self.builder.compiler_toolchain('1.99.1', target, 'nt'),
                         '1.99.1-x86_64-pc-windows-gnu')
        self.assertEqual(self.builder.compiler_toolchain('1.99.1', 'x86_64-pc-windows-msvc', 'nt'), '1.99.1')
        self.assertEqual(self.builder.compiler_toolchain('1.99.1', target, 'posix'), '1.99.1')
        self.assertEqual(self.builder.archive_name(target), 'libku_native_calendar.a')
        self.assertEqual(self.builder.archive_name('x86_64-pc-windows-msvc'), 'ku_native_calendar.lib')
        self.assertEqual(self.validate([('calendar.o', self.coff())], target), 1)
        self.assertEqual(self.builder.native_tokens('-lkernel32 -l:libpthread.a -lgcc_eh', target),
                         ['-lkernel32', '-l:libpthread.a', '-lgcc_eh'])
        for raw in ('kernel32.lib', '/defaultlib:msvcrt', '-l:../libpthread.a', '-l:other.a', '-Wl,--whole-archive'):
            with self.subTest(raw=raw), self.assertRaises(ValueError):
                self.builder.native_tokens(raw, target)
        for other in ('x86_64-pc-windows-msvc', 'x86_64-unknown-linux-gnu', 'aarch64-apple-darwin'):
            with self.subTest(target=other), self.assertRaises(ValueError):
                self.builder.native_tokens('-l:libpthread.a', other)

    def test_windows_gnu_requires_real_abi_definitions_and_identity(self):
        target = 'x86_64-pc-windows-gnu'
        imports = [('kernel32.dll', self.short_import(name)) for name in self.builder.CALENDAR_API]
        for objects in ([('data.o', self.coff(names=[])), *imports],
                        [('data.o', self.coff(names=[])), ('thunk.o', self.coff(section=b'.idata$5'))],
                        [('calendar.o', self.coff(identity=b'another-build')),
                         ('kernel32.dll', self.short_import(self.builder.BUILD_ID.encode()))]):
            with self.subTest(objects=len(objects)), self.assertRaisesRegex(ValueError, 'defined ABI|build identity'):
                self.validate(objects, target)

    def test_short_imports_and_real_bigobj_are_distinct(self):
        for big in (False, True):
            normal = self.coff(big=big)
            short = self.short_import()
            long = self.short_import(b'GetSystemTimePreciseAsFileTime')
            self.assertLess(len(short), 56)
            self.assertGreater(len(long), 56)
            self.assertEqual(self.validate([('calendar.o', normal), ('kernel32.dll', short),
                                           ('kernel32.dll', long),
                                           ('bcrypt.dll', self.short_import(b'ProcessPrng', b'bcryptprimitives.dll')),
                                           ('api.dll', self.short_import(b'WaitOnAddress', b'api-ms-win-core-synch-l1-2-0.dll'))]), 1)

    def test_malformed_imports_never_pass_as_bigobj(self):
        good = self.short_import(b'GetSystemTimePreciseAsFileTime')
        bad = []
        wrong = good.copy()
        struct.pack_into('<H', wrong, 6, 0xaa64)
        bad.append(wrong)
        wrong = good.copy()
        struct.pack_into('<I', wrong, 12, 1)
        bad.append(wrong)
        wrong = good.copy()
        struct.pack_into('<H', wrong, 4, 1)
        bad.append(wrong)
        bad.extend((good[:19], good[:-1], self.short_import(dll=b'../kernel32.dll'),
                    self.short_import(dll=b'private.dll'), self.short_import(name=b''),
                    self.short_import(dll=b'api-ms-win-.dll'), self.short_import(dll=b'api-ms-win-core-synch.dll'),
                    self.short_import(flags=0x8004), self.short_import(flags=3),
                    self.short_import(flags=4 << 2)))
        for import_body in bad:
            with self.subTest(body=bytes(import_body[:20])), self.assertRaises(ValueError):
                self.validate([('calendar.o', self.coff()), ('kernel32.dll', import_body)])

    def test_imports_cannot_supply_api_definitions_or_build_identity(self):
        imports = [('kernel32.dll', self.short_import(name)) for name in self.builder.CALENDAR_API]
        with self.assertRaisesRegex(ValueError, 'defined ABI'):
            self.validate([('data.o', self.coff(names=[])), *imports])
        with self.assertRaisesRegex(ValueError, 'defined ABI'):
            self.validate([('data.o', self.coff(names=[])), ('thunk.o', self.coff(section=b'.idata$5'))])
        with self.assertRaisesRegex(ValueError, 'build identity'):
            self.validate([('calendar.o', self.coff(identity=b'another-build')),
                           ('kernel32.dll', self.short_import(self.builder.BUILD_ID.encode()))])
        with self.assertRaisesRegex(ValueError, 'no complete native objects'):
            self.validate([('kernel32.dll', self.short_import(self.builder.BUILD_ID.encode()))])


class RuntimeConsumerReportTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        spec = importlib.util.spec_from_file_location('ku_runtime_consumer_report', Path(__file__).with_name('consume-runtime-bundle.py'))
        assert spec and spec.loader
        cls.consumer = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(cls.consumer)

    def setUp(self):
        temporary = tempfile.TemporaryDirectory(prefix='ku-consumer-report-test-')
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name).resolve()
        self.path = self.root / '中文 report.json'
        self.original = b'{"status":"last valid report"}\n'
        self.path.write_bytes(self.original)

    def test_rustup_management_symlink_keeps_invoked_name(self):
        target = self.root / 'rustup-init'
        alias = self.root / 'rustup'
        target.write_text('import pathlib,sys\nname=pathlib.Path(sys.argv[0]).name\nprint(name)\nsys.exit(0 if name == "rustup" else 23)\n', encoding='utf-8')
        try:
            alias.symlink_to(target)
        except OSError as error:
            self.skipTest('platform cannot create file symlink: ' + str(error))
        with mock.patch.object(self.consumer.shutil, 'which', return_value=str(alias)):
            invoked = self.consumer.management_invocation('rustup')
        self.assertEqual(invoked, os.path.abspath(alias))
        self.assertNotEqual(invoked, str(alias.resolve()))
        result = subprocess.run([sys.executable, invoked], capture_output=True, timeout=5)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout.strip(), b'rustup')

    def test_owned_checkout_move_and_restore_handles_cross_volume(self):
        import errno
        source_dir = self.root / 'src'
        source_dir.mkdir()
        source_file = source_dir / '中文.rs'
        source_file.write_bytes(b'original source bytes\n')
        cargo = self.root / 'Cargo.toml'
        cargo.write_bytes(b'[package]\n')
        backup_dir = self.root / 'retired-src'
        backup_cargo = self.root / 'retired-Cargo.toml'
        # Real copy/unlink fallback is exercised, not only an asserted helper
        # call. Path.rename would fail on the same EXDEV condition.
        with mock.patch.object(self.consumer.os, 'rename', side_effect=OSError(errno.EXDEV, 'cross-device move')):
            self.consumer.relocate_checkout_entry(source_dir, backup_dir)
            self.consumer.relocate_checkout_entry(cargo, backup_cargo)
            self.assertFalse(source_dir.exists())
            self.assertFalse(cargo.exists())
            self.assertEqual((backup_dir / source_file.name).read_bytes(), b'original source bytes\n')
            self.consumer.relocate_checkout_entry(backup_dir, source_dir)
            self.consumer.relocate_checkout_entry(backup_cargo, cargo)
        self.assertEqual(source_file.read_bytes(), b'original source bytes\n')
        self.assertEqual(cargo.read_bytes(), b'[package]\n')
        self.assertFalse(backup_dir.exists())
        self.assertFalse(backup_cargo.exists())

    def test_reported_artifact_uses_cli_cwd_quotes_and_actual_ownership(self):
        project = self.root / 'project with spaces'
        project.mkdir()
        artifact = project / 'output.ll'
        artifact.write_bytes(b'; real IR fixture\n')
        for printed in ('output.ll', '"output.ll"', str(artifact), json.dumps(str(artifact))):
            with self.subTest(printed=printed):
                path = self.consumer.reported_artifact('llvm ir ok: ' + printed + '\n', 'llvm ir ok: ', project, self.root)
                self.assertTrue(os.path.samefile(path, artifact))
        with tempfile.TemporaryDirectory(prefix='ku-unowned-ir-') as other:
            outside = Path(other) / 'outside.ll'
            outside.write_bytes(b'; outside')
            with self.assertRaisesRegex(RuntimeError, 'reported=.*cwd=.*root='):
                self.consumer.reported_artifact('llvm ir ok: ' + str(outside), 'llvm ir ok: ', project, self.root)
        with self.assertRaisesRegex(RuntimeError, 'expected one bounded artifact'):
            self.consumer.reported_artifact('not an artifact', 'llvm ir ok: ', project, self.root)

    def test_report_is_utf8_even_under_cp1252_text_default(self):
        value = {'path': '搬迁 中文 空格', 'failure': '库版本错误 😀'}
        with mock.patch('io.text_encoding', return_value='cp1252'):
            self.consumer.write_report(self.path, value)
        self.assertEqual(json.loads(self.path.read_bytes().decode('utf-8')), value)
        self.assertEqual(list(self.root.glob('*.writing')), [])

    def test_report_serialization_or_encoding_failure_preserves_original(self):
        for value, error in (({'not_json': object()}, TypeError), ({'invalid_unicode': '\ud800'}, UnicodeEncodeError), ({'not_finite': float('nan')}, ValueError)):
            with self.subTest(error=error.__name__), self.assertRaises(error):
                self.consumer.write_report(self.path, value)
            self.assertEqual(self.path.read_bytes(), self.original)
            self.assertEqual(list(self.root.glob('*.writing')), [])

    def test_report_replace_failure_cleans_private_file_and_preserves_original(self):
        with mock.patch.object(self.consumer.os, 'replace', side_effect=PermissionError('replace denied')), self.assertRaises(PermissionError):
            self.consumer.write_report(self.path, {'status': 'next complete report'})
        self.assertEqual(self.path.read_bytes(), self.original)
        self.assertEqual(list(self.root.glob('*.writing')), [])

    def test_report_write_failure_cleans_private_file_and_preserves_original(self):
        with mock.patch.object(self.consumer.os, 'fsync', side_effect=OSError('disk write failed')), self.assertRaises(OSError):
            self.consumer.write_report(self.path, {'status': 'next complete report'})
        self.assertEqual(self.path.read_bytes(), self.original)
        self.assertEqual(list(self.root.glob('*.writing')), [])

    def test_explicit_tls_certificate_tool_does_not_fall_back(self):
        for value in ('relative-openssl', str(self.root / 'missing-openssl')):
            with self.subTest(value=value), mock.patch.object(self.consumer.shutil, 'which') as which:
                with self.assertRaisesRegex(RuntimeError, 'KU_TEST_OPENSSL must name an existing absolute'):
                    self.consumer.tls_fixture_openssl({'KU_TEST_OPENSSL': value})
                which.assert_not_called()

    def test_compiler_readback_distinguishes_default_c_from_explicit_runner(self):
        for backend, marker in (('c', 'c compiler command:'), ('runner', 'rustc command:')):
            with self.subTest(backend=backend):
                result = subprocess.CompletedProcess([], 0, ('build profile: release\n' + marker + ' compiler argv\n').encode(), b'')
                self.consumer.assert_build_backend(result, backend, 'release')
                other = 'runner' if backend == 'c' else 'c'
                with self.assertRaisesRegex(RuntimeError, 'expected .* compiler command'):
                    self.consumer.assert_build_backend(result, other, 'release')
                with self.assertRaisesRegex(RuntimeError, 'expected .* profile'):
                    self.consumer.assert_build_backend(result, backend, 'debug')
        for text in ('build profile: release\n',
                     'build profile: release\nc compiler command: cc\nrustc command: rustc\n',
                     'build profile: release\nmsvc: cl (env from vcvars)\n'):
            with self.subTest(text=text), self.assertRaisesRegex(RuntimeError, 'compiler command'):
                self.consumer.assert_build_backend(subprocess.CompletedProcess([], 0, text.encode(), b''), 'c', 'release')

    def test_native_environment_hides_rust_without_removing_separate_c_tools(self):
        rust = self.root / 'Rust tools'
        native = self.root / 'Native C tools 中文'
        rust.mkdir()
        native.mkdir()
        rust_name = 'rustc.exe' if os.name == 'nt' else 'rustc'
        (rust / rust_name).write_bytes(b'fixture only')
        original = {'PATH': os.pathsep.join((str(rust), str(native))), 'KU_RUNNER_RUSTC': '/must/not/use/rustc', 'RUSTUP_TOOLCHAIN': 'unlike'}
        before = dict(original)
        with mock.patch.object(self.consumer.shutil, 'which', return_value=None):
            selected = self.consumer.native_without_rust(original, self.root / 'absent-rustup')
        self.assertEqual(original, before)
        self.assertEqual(selected['PATH'], str(native))
        self.assertNotIn('KU_RUNNER_RUSTC', selected)
        self.assertNotIn('RUSTUP_TOOLCHAIN', selected)
        self.assertEqual(selected['RUSTUP_AUTO_INSTALL'], '0')
        with mock.patch.object(self.consumer.shutil, 'which', return_value='unexpected rustc'), self.assertRaisesRegex(RuntimeError, 'could not isolate Rust'):
            self.consumer.native_without_rust(original, self.root / 'absent-rustup')

    def test_native_build_diagnostics_reject_crt_warnings_on_either_stream_even_at_exit_zero(self):
        for warning in ('warning LNK4098: defaultlib conflict', 'warning LNK4286: imported symbol',
                        'warning LNK4217: locally defined symbol imported', 'error LNK2019: unresolved external',
                        'error LNK1120: unresolved externals',
                        "error LNK2038: mismatch detected for 'RuntimeLibrary': MD does not match MT",
                        'warning: conflicting runtime libraries MSVCRT and LIBCMT'):
            for stream in ('stdout', 'stderr'):
                with self.subTest(warning=warning, stream=stream):
                    data = (b'x' * 5000 + b'\n' + warning.encode())
                    result = subprocess.CompletedProcess([], 0, data if stream == 'stdout' else b'', data if stream == 'stderr' else b'')
                    detail = {}
                    with self.assertRaisesRegex(RuntimeError, 'incompatible CRT/linker'):
                        self.consumer.record_native_build_diagnostics(detail, result)
                    self.assertFalse(detail['build_link_contract_passed'])
                    self.assertTrue(detail['build_link_contract_diagnostics'])
                    self.assertLessEqual(len(detail['build_stderr_excerpt'].encode('utf-8')), 4096)
        detail = {}
        warning = b'warning: unused local variable\n' + b'\xff' * 5000
        self.consumer.record_native_build_diagnostics(detail, subprocess.CompletedProcess([], 0, b'', warning))
        self.assertTrue(detail['build_link_contract_passed'])
        self.assertEqual(detail['build_stderr_bytes'], len(warning))
        self.assertTrue(detail['build_stderr_truncated'])
        self.assertTrue(detail['build_stderr_excerpt'].startswith('warning: unused local variable'))
        self.assertLessEqual(len(detail['build_stderr_excerpt'].encode('utf-8')), 4096)

    @unittest.skipIf(os.name == 'nt', 'POSIX launcher symlink regression')
    def test_installed_time_array_orchestration_invokes_the_sdk_through_a_real_symlink(self):
        # Exercise the consumer's entry/source/SDK lifetime protocol. Compiler
        # and executable process edges are fixtures, not native runtime proof.
        install = self.root/'SDK with spaces 中文'
        install.mkdir()
        ku = install/'ku'
        ku.write_bytes(b'installed compiler fixture')
        target = 'x86_64-unknown-linux-gnu'
        calendar_pack_fixture(install, target)
        report, calls = {}, []

        def invoke(command, cwd, env, seconds=300):
            calls.append(command)
            if len(command) > 1:
                entry = Path(command[0])
                self.assertTrue(entry.is_symlink())
                self.assertNotEqual(entry.parent, install)
                self.assertTrue(os.path.samefile(entry, ku))
                self.assertNotIn('KU_NATIVE_CALENDAR_PACK', env)
                self.assertEqual(command[1:6], ['build', '--backend', 'c', '--release', '--verbose'])
                source = cwd/'main.ku'
                self.assertTrue(source.is_file())
                emitted = cwd/'.ku/build/release/modules.c'
                emitted.parent.mkdir(parents=True)
                emitted.write_bytes(b'#define KU_FEATURE_NATIVE_CALENDAR 1\n__ku_calendar_call_v1\n__ku_array_first_\n__ku_array_concat_\n')
                Path(command[-1]).write_bytes(b'compiled program fixture')
                return subprocess.CompletedProcess(command, 0, ('build profile: release\nc compiler command: fixture\nnative c ok: '+str(emitted)+'\n').encode(), b'')
            self.assertFalse(install.exists())
            self.assertEqual(cwd.name, 'unrelated working directory')
            self.assertFalse(any(Path(command[0]).parent.parent.glob('module build project*')))
            return subprocess.CompletedProcess(command, 0, b'native-time-array-ok\n', b'')

        with mock.patch.object(self.consumer, 'invoke', side_effect=invoke):
            self.consumer.installed_native_time_array_consumer(
                install, self.root, {'KU_NATIVE_CALENDAR_PACK': '/must/not/use'},
                report, self.path, target)
        detail = report['installed_native_time_array']
        self.assertEqual(detail['status'], 'passed')
        self.assertEqual(detail['cli_entry_kind'], 'posix-symlink')
        self.assertTrue(detail['source_removed_before_run'])
        self.assertTrue(detail['installation_restored'])
        self.assertEqual(len(calls), 2)
        self.assertTrue(ku.is_file())
        self.assertFalse(list(self.root.glob('ku-installed-time-array-*')))

    def installed_tls_orchestration(self, failure=None):
        """Only the archive/lifetime protocol is mocked, not claimed as TLS proof.

        The actual consumer uses ssl and the installed CLI. This unit test has
        a real bounded socket peer but substitutes the compiler/client process
        edges and certificate context, making SDK/source visibility assertions
        independent of a C toolchain or a test certificate utility.
        """
        target = 'x86_64-pc-windows-msvc' if os.name == 'nt' else 'x86_64-unknown-linux-gnu'
        install = self.root / '搬迁 SDK with spaces'
        install.mkdir()
        ku = install / ('ku.exe' if os.name == 'nt' else 'ku')
        ku.write_bytes(b'installed package compiler bytes')
        pack = install / 'native-tls/v1' / target
        archive = 'ku_native_tls.lib' if os.name == 'nt' else 'libku_native_tls.a'
        for name in ('manifest.kutls', 'include/ku_native_tls.h', 'lib/' + archive):
            path = pack / name
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_bytes(('same archive TLS payload: ' + name).encode())
        before = {p.relative_to(install).as_posix(): p.read_bytes() for p in install.rglob('*') if p.is_file()}
        report, calls, endpoint = {}, [], {}

        class WrappedSocket:
            def __init__(self, value):
                self.value = value
            def __enter__(self):
                return self
            def __exit__(self, *ignored):
                self.close()
            def close(self):
                self.value.close()
            def recv(self, count):
                return self.value.recv(count)
            def sendall(self, value):
                self.value.sendall(value)
            def version(self):
                return 'TLSv1.3'

        context = SimpleNamespace(wrap_socket=lambda value, server_side: WrappedSocket(value))

        def fixture(directory, env):
            (directory / 'localhost-key.pem').write_text('private test fixture, must not be reported')
            return 'public test certificate\n', context

        def invoke(command, cwd, env, seconds=300):
            calls.append([str(p) for p in command])
            self.assertNotIn('KU_NATIVE_TLS_PACK', env)
            self.assertNotIn('KU_NATIVE_TLS_LINK_REQUIRED', env)
            if Path(command[0]) == ku:
                self.assertTrue(ku.is_file())
                self.assertEqual(command[1:6], ['build', '--backend', 'c', '--release', '--verbose'])
                source = (cwd / 'main.ku').read_text()
                endpoint['port'] = int(re.search(r'port: (\d+)', source)[1])
                self.assertIn('tls_server_name: "localhost"', source)
                self.assertIn('bytes.from_array([65, 0, 255])', source)
                if failure == 'build':
                    return subprocess.CompletedProcess(command, 31, b'fixture compiler failure', b'')
                artifact = cwd / '.ku/build/release/client.c'
                artifact.parent.mkdir(parents=True)
                artifact.write_bytes(b'/* fixture only */ void ku_tls_v1_client_new(void);\n')
                Path(command[-1]).write_bytes(b'fixture executable bytes')
                stderr = b''
                if failure == 'crt-warning':
                    stderr = b'LINK : warning LNK4098: defaultlib conflicts with other libs\n'
                elif failure == 'deployment-warning':
                    stderr = b"ld: warning: object file 'tls.a(ring.o)' was built for newer 'macOS' version (15.5) than being linked (15.0)\n"
                return subprocess.CompletedProcess(command, 0, ('build profile: release\nc compiler command: fixture argv\nnative c ok: ' + str(artifact) + '\n').encode(), stderr)
            self.assertFalse(install.exists())
            self.assertFalse(ku.exists())
            self.assertTrue((self.root / 'installed-sdk-unavailable-for-tls').is_dir())
            self.assertFalse(any(Path(command[0]).parent.parent.glob('build project*')))
            self.assertEqual(Path(command[0]).parent.name, '搬迁 TLS program')
            self.assertEqual(cwd.name, 'unrelated working directory')
            if failure == 'run':
                return subprocess.CompletedProcess(command, 32, b'', b'fixture runtime failure')
            with socket.create_connection(('127.0.0.1', endpoint['port']), timeout=2) as client:
                client.sendall(bytes((65, 0, 255)))
                reply = bytearray()
                while len(reply) < 3:
                    part = client.recv(3-len(reply))
                    self.assertTrue(part)
                    reply.extend(part)
            stdout = (str(len(reply)) + '\n' + ''.join(str(value) + '\n' for value in reply)).encode()
            return subprocess.CompletedProcess(command, 0, stdout, b'')

        with mock.patch.object(self.consumer, 'installed_tls_fixture', side_effect=fixture), mock.patch.object(self.consumer, 'invoke', side_effect=invoke):
            arguments = (install, self.root, {'KU_NATIVE_TLS_PACK': '/must-not-use/source/pack', 'KU_NATIVE_TLS_LINK_REQUIRED': '1'}, report, self.path, target)
            if failure:
                with self.assertRaisesRegex(RuntimeError, 'native TLS build failed|incorrect result or exit|incompatible CRT/linker/deployment'):
                    self.consumer.installed_native_tls_consumer(*arguments, checkout_hidden=True)
            else:
                self.consumer.installed_native_tls_consumer(*arguments, checkout_hidden=True)
        self.assertEqual({p.relative_to(install).as_posix(): p.read_bytes() for p in install.rglob('*') if p.is_file()}, before)
        self.assertEqual(list(self.root.glob('ku-installed-tls-*')), [])
        self.assertFalse((self.root / 'installed-sdk-unavailable-for-tls').exists())
        self.assertTrue(report['installed_native_tls']['peer_stopped'])
        self.assertTrue(report['installed_native_tls']['installation_restored'])
        self.assertNotIn('private test fixture, must not be reported', json.dumps(report))
        self.assertEqual(len(calls), 1 if failure in ('build', 'crt-warning', 'deployment-warning') else 2)
        return report['installed_native_tls']

    def test_installed_tls_uses_archive_pack_and_moved_source_free_binary(self):
        detail = self.installed_tls_orchestration()
        self.assertEqual(detail['status'], 'passed')
        self.assertTrue(detail['source_removed_before_run'])
        self.assertTrue(detail['sdk_unavailable_during_run'])
        self.assertTrue(detail['checkout_hidden_during_build'])
        self.assertTrue(detail['binary_echo'])
        self.assertEqual(detail['tls_version'], 'TLSv1.3')

    def test_installed_tls_failure_restores_archive_and_stops_owned_peer(self):
        for stage in ('build', 'run', 'crt-warning', 'deployment-warning'):
            with self.subTest(stage=stage):
                # Each failure owns a distinct SDK and peer lifecycle.
                with tempfile.TemporaryDirectory(prefix='tls-failure-', dir=self.root) as nested:
                    original = self.root
                    self.root = Path(nested)
                    try:
                        self.assertEqual(self.installed_tls_orchestration(stage)['status'], 'failed')
                    finally:
                        self.root = original

    def installed_compression_orchestration(self, failure=None):
        """Real file moves/removal with mocked process edges, not codec proof."""
        target = 'x86_64-pc-windows-msvc' if os.name == 'nt' else 'x86_64-unknown-linux-gnu'
        install = self.root / '搬迁 codec SDK with spaces'
        install.mkdir()
        ku = install / ('ku.exe' if os.name == 'nt' else 'ku')
        ku.write_bytes(b'installed package compiler bytes')
        for codec in ('zstd', 'zlib'):
            compression_pack_fixture(install, target, codec)
        before = {p.relative_to(install).as_posix(): p.read_bytes() for p in install.rglob('*') if p.is_file()}
        env = {name: '/must-not-use/development-pack' for name in ('KU_ZSTD_LIB', 'KU_ZSTD_INCLUDE', 'KU_ZLIB_LIB', 'KU_ZLIB_INCLUDE')}
        calls, report = [], {}

        def invoke(command, cwd, selected_env, seconds=300):
            calls.append([str(p) for p in command])
            for name in env:
                self.assertNotIn(name, selected_env)
            if Path(command[0]) == ku:
                self.assertTrue(ku.is_file())
                self.assertEqual(command[1:6], ['build', '--backend', 'c', '--release', '--verbose'])
                source = (cwd / 'main.ku').read_text(encoding='utf-8')
                for module in ('zstd', 'gzip', 'deflate'):
                    self.assertIn(module + '.compress(source, 4096)?', source)
                if failure == 'build':
                    return subprocess.CompletedProcess(command, 31, b'fixture compiler failure', b'')
                artifact = cwd / '.ku/build/release/codec.c'
                artifact.parent.mkdir(parents=True)
                artifact.write_bytes(b'#define KU_FEATURE_ZSTD 1\n#define KU_FEATURE_ZLIB 1\nku_zstd_compress ku_gzip_compress ku_deflate_compress\n')
                Path(command[-1]).write_bytes(b'fixture executable bytes')
                stderr = b''
                if failure == 'crt-warning':
                    stderr = b'LINK : warning LNK4217: local runtime symbol imported\n'
                elif failure == 'deployment-warning':
                    stderr = b"ld: warning: object file 'zstd.a(entropy.o)' was built for newer 'macOS' version (15.5) than being linked (15.0)\n"
                return subprocess.CompletedProcess(command, 0, ('build profile: release\nc compiler command: fixture argv\nnative c ok: ' + str(artifact) + '\n').encode(), stderr)
            self.assertFalse(install.exists())
            self.assertFalse(ku.exists())
            self.assertTrue((self.root / 'installed-sdk-unavailable-for-compression').is_dir())
            self.assertFalse(any(Path(command[0]).parent.parent.glob('codec build project*')))
            self.assertEqual(Path(command[0]).parent.name, '搬迁 codec program')
            self.assertEqual(cwd.name, 'unrelated working directory')
            self.assertEqual(seconds, 15)
            return subprocess.CompletedProcess(command, 32 if failure == 'run' else 0,
                b'' if failure == 'run' else b'installed-native-compression-roundtrip-ok\n', b'')

        with mock.patch.object(self.consumer, 'invoke', side_effect=invoke):
            arguments = (install, self.root, env, report, self.path, target)
            if failure:
                with self.assertRaisesRegex(RuntimeError, 'native compression build failed|incorrect result or exit|incompatible CRT/linker'):
                    self.consumer.installed_native_compression_consumer(*arguments, checkout_hidden=True)
            else:
                self.consumer.installed_native_compression_consumer(*arguments, checkout_hidden=True)
        self.assertEqual({p.relative_to(install).as_posix(): p.read_bytes() for p in install.rglob('*') if p.is_file()}, before)
        self.assertEqual(list(self.root.glob('ku-installed-compression-*')), [])
        self.assertFalse((self.root / 'installed-sdk-unavailable-for-compression').exists())
        self.assertTrue(report['installed_native_compression']['installation_restored'])
        self.assertEqual(len(calls), 1 if failure in ('build', 'crt-warning', 'deployment-warning') else 2)
        return report['installed_native_compression']

    def test_installed_compression_uses_moved_archive_without_source_or_sdk(self):
        detail = self.installed_compression_orchestration()
        self.assertEqual(detail['status'], 'passed')
        for key in ('source_removed_before_run', 'sdk_unavailable_during_run', 'checkout_hidden_during_build', 'binary_roundtrip', 'unicode_roundtrip', 'output_bound'):
            self.assertTrue(detail[key])
        self.assertEqual(set(detail['installed_packs']), {'zstd', 'zlib'})

    def test_installed_compression_failures_restore_original_sdk(self):
        for stage in ('build', 'run', 'crt-warning', 'deployment-warning'):
            with self.subTest(stage=stage), tempfile.TemporaryDirectory(prefix='codec-failure-', dir=self.root) as nested:
                original = self.root
                self.root = Path(nested)
                try:
                    self.assertEqual(self.installed_compression_orchestration(stage)['status'], 'failed')
                finally:
                    self.root = original

    def test_old_plain_bundle_does_not_acquire_optional_codec_dependency(self):
        install = self.root / 'plain SDK'
        install.mkdir()
        report = {}
        with mock.patch.object(self.consumer, 'invoke') as invoked:
            self.consumer.installed_native_compression_consumer(install, self.root, {}, report, self.path, 'x86_64-unknown-linux-gnu')
        invoked.assert_not_called()
        self.assertEqual(report['installed_native_compression']['status'], 'not_in_archive')


class ReleaseExportTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(prefix="ku-export-test-")
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name).resolve()
        self.repo = self.root / "repo"
        self.repo.mkdir()
        self.bundle = self.repo / "bundle"
        self.bundle.mkdir()
        self.output = self.root / "output"
        self.target = "x86_64-unknown-linux-gnu"
        for name in ("ku", "libku.rlib", "deps/libsample-0123456789abcdef.rlib", "ku-language-0.0.17.vsix",
                     f"native-tls/v1/{self.target}/manifest.kutls",
                     f"native-tls/v1/{self.target}/include/ku_native_tls.h",
                     f"native-tls/v1/{self.target}/lib/libku_native_tls.a"):
            self.put(self.bundle / name, f"fixture:{name}\n".encode())
        (self.bundle / "ku").chmod(0o755)
        self.make_runtime_manifest()
        self.package = self.root / "cache" / "sample-1.2.3"
        self.put(self.package / "Cargo.toml", b"fixture manifest\n")
        self.put(self.package / "LICENSE-MIT", b"original license bytes\r\nCopyright sample\r\n")
        self.put(self.package / "NOTICE", b"original NOTICE bytes\n")
        self.put(self.repo / "Cargo.toml", b"workspace manifest\n")
        self.put(self.repo / "LICENSE", b"original Ku Apache-2.0 license fixture\r\n")
        self.put(self.repo / "NOTICE", b"original Ku copyright notice fixture\n")
        self.data = {
            "workspace_members": ["ku"],
            "packages": [
                {"name": "ku", "version": "0.0.17", "id": "ku", "manifest_path": str(self.repo / "Cargo.toml"), "source": None, "license": "Apache-2.0"},
                {"name": "sample", "version": "1.2.3", "id": "sample", "manifest_path": str(self.package / "Cargo.toml"), "source": "registry+https://github.com/rust-lang/crates.io-index", "license": "MIT", "license_file": None},
            ],
            "resolve": {"nodes": [
                {"id": "ku", "deps": [{"pkg": "sample", "dep_kinds": [{"kind": None, "target": None}]}]},
                {"id": "sample", "deps": []},
            ]},
        }
        self.metadata = self.root / "metadata.json"
        self.write_metadata()
        self.sysroot = self.root / "rust"
        self.put(self.sysroot / "share/doc/rust/licenses/MIT.txt", b"original Rust license\n")
        self.put(self.sysroot / "share/doc/rust/COPYRIGHT-library.html", b"<html>original Rust attribution</html>")

    def make_runtime_manifest(self):
        for name in ('ku-runtime.json', 'ku-runtime-deps.json'):
            (self.bundle / name).unlink(missing_ok=True)
        text = 'release: 1.89.0\ncommit-hash: ' + 'b'*40 + '\ncommit-date: 2025-08-04\nhost: ' + self.target + '\n'
        EXPORT.RUNTIME_BUNDLE.create(self.bundle, '0.0.17', 'a'*40, self.target, text, '1.89.0')

    @staticmethod
    def put(path: Path, data: bytes):
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes(data)

    def write_metadata(self):
        self.metadata.write_text(json.dumps(self.data), encoding="utf-8")

    def run_export(self, **overrides):
        arguments = dict(bundle=self.bundle, output=self.output, repo=self.repo, version="0.0.17",
                         target=self.target, commit="a" * 40, metadata=self.metadata, rust_sysroot=self.sysroot)
        arguments.update(overrides)
        # Windows chmod cannot represent Unix execute bits. Simulate this input
        # target's source mode; the tar verifier still checks actual stored mode.
        if os.name == "nt" and "-windows-" not in arguments["target"]:
            original = EXPORT.plain

            def target_mode(path, directory=False):
                info = original(path, directory)
                if Path(path) == self.bundle / "ku":
                    fields = list(info)
                    fields[0] |= 0o111
                    return os.stat_result(fields)
                return info

            with mock.patch.object(EXPORT, "plain", side_effect=target_mode):
                return EXPORT.export_release(**arguments)
        return EXPORT.export_release(**arguments)

    def test_runtime_manifest_rejects_missing_mixed_corrupt_and_escaping_inputs(self):
        runtime = self.bundle / 'ku-runtime.json'
        original = runtime.read_bytes()
        runtime.unlink()
        with self.assertRaises(EXPORT.ExportError):
            self.run_export()
        runtime.write_bytes(original)
        dependency = self.bundle / 'deps/libsample-0123456789abcdef.rlib'
        saved = dependency.read_bytes()
        dependency.write_bytes(b'from another compilation')
        with self.assertRaisesRegex(EXPORT.ExportError, 'runtime bundle'):
            self.run_export()
        dependency.write_bytes(saved)
        manifest = json.loads(original)
        manifest['runtime_rlib'] = '../libku.rlib'
        runtime.write_text(json.dumps(manifest))
        with self.assertRaisesRegex(EXPORT.ExportError, 'runtime bundle'):
            self.run_export()
        runtime.write_bytes(original)
        self.assertEqual(EXPORT.RUNTIME_BUNDLE.verify(self.bundle)['source_commit'], 'a'*40)

    def test_runtime_identity_uses_actual_compiler_and_ignores_unlisted_mtime(self):
        before = EXPORT.RUNTIME_BUNDLE.verify(self.bundle)
        extra = self.bundle / 'deps/libku-ffffffffffffffff.rlib'
        extra.write_bytes(b'unrelated newer library')
        os.utime(extra, (time.time()+3600, time.time()+3600))
        self.assertEqual(EXPORT.RUNTIME_BUNDLE.verify(self.bundle), before)
        # Selection is bounded to the manifest, but publisher exports must not
        # include an unlisted dependency even when its filename looks valid.
        with self.assertRaisesRegex(EXPORT.ExportError, 'closure'):
            self.run_export()
        extra.unlink()
        bad_compiler = 'release: 1.96.0\ncommit-hash: ' + 'b'*40 + '\ncommit-date: 2025-08-04\nhost: ' + self.target + '\n'
        with self.assertRaisesRegex(ValueError, 'actual compiler'):
            EXPORT.RUNTIME_BUNDLE.rust_identity(bad_compiler, '1.89.0', self.target)

    def test_runtime_manifest_duplicate_keys_and_relative_paths_rejected(self):
        path = self.bundle / 'ku-runtime.json'
        path.write_text('{"schema":"ku-runtime-bundle-v1","schema":"other"}')
        with self.assertRaisesRegex(ValueError, 'duplicate'):
            EXPORT.RUNTIME_BUNDLE.verify(self.bundle)
        for name in ('../outside', '/absolute', 'C:/outside', 'deps/../libku.rlib', 'deps\\outside', 'deps//file'):
            with self.subTest(name=name), self.assertRaises(ValueError):
                EXPORT.RUNTIME_BUNDLE.relative_file(self.bundle, name)

    def optional_compression_root(self):
        root = self.root / 'private codec cache 中文'
        for codec in ('zstd', 'zlib'):
            compression_pack_fixture(root, self.target, codec)
        return root

    def test_explicit_optional_compression_exports_only_verified_payload(self):
        root = self.optional_compression_root()
        self.put(root / 'sources/upstream.tar.gz', b'not an SDK payload')
        self.put(root / 'build/debug.log', b'not an SDK payload')
        self.put(root / f'native-compression/v1/{self.target}/zstd/extra-private-file', b'not an SDK payload')
        result = self.run_export(native_compression_root=root)
        manifest = json.loads(Path(result['manifest_path']).read_text(encoding='utf-8'))
        exported = {name for name in manifest['files'] if name.startswith('native-compression/')}
        expected = set()
        for codec in ('zstd', 'zlib'):
            prefix = f'native-compression/v1/{self.target}/{codec}/'
            expected.update(prefix + name for name in ('manifest.json', 'LICENSE',
                'lib/' + EXPORT.NATIVE_COMPRESSION.archive_name(codec, self.target),
                *['include/' + name for name in EXPORT.NATIVE_COMPRESSION.HEADER_NAMES[codec]]))
        self.assertEqual(exported, expected)
        extracted = self.root / 'codec extracted'
        extracted.mkdir()
        EXPORT.verify_archive(Path(result['asset_path']), manifest['files'], extracted)
        for codec in ('zstd', 'zlib'):
            leaf = extracted / 'native-compression/v1' / self.target / codec
            EXPORT.NATIVE_COMPRESSION.validate_pack(leaf, codec, self.target)
        attribution = json.loads((extracted / 'THIRD_PARTY.json').read_text(encoding='utf-8'))
        self.assertEqual([row['name'] for row in attribution['native_compression']], ['zstd', 'zlib'])
        self.assertNotIn(str(root), json.dumps(attribution))
        self.assertFalse(any(name.startswith(('sources/', 'build/')) for name in manifest['files']))

    def test_optional_compression_refuses_missing_mixed_or_unlicensed_pack(self):
        root = self.optional_compression_root()
        leaf = root / 'native-compression/v1' / self.target / 'zlib'
        path = leaf / 'manifest.json'
        original = path.read_bytes()
        for change in ('wrong-target', 'wrong-license', 'wrong-build', 'duplicate-key', 'mixed-header'):
            with self.subTest(change=change):
                data = json.loads(original)
                header = leaf / 'include/zlib.h'
                saved = header.read_bytes()
                if change == 'wrong-target':
                    data['target'] = 'aarch64-apple-darwin'
                elif change == 'wrong-license':
                    data['license']['spdx'] = 'Apache-2.0'
                elif change == 'wrong-build':
                    data['build']['multithread'] = True
                elif change == 'mixed-header':
                    header.write_bytes(b'wrong installed header')
                path.write_text(json.dumps(data), encoding='utf-8')
                if change == 'duplicate-key':
                    path.write_text('{"codec":"different",' + original.decode()[1:], encoding='utf-8')
                with self.assertRaises(EXPORT.ExportError):
                    self.run_export(native_compression_root=root)
                path.write_bytes(original)
                header.write_bytes(saved)
                self.assertEqual(list(self.output.glob('*.tar.gz')), [])
        (leaf / 'LICENSE').unlink()
        with self.assertRaises(OSError):
            self.run_export(native_compression_root=root)

    def test_optional_compression_detects_mutation_during_snapshot_copy(self):
        root = self.optional_compression_root()
        original_add = EXPORT.Stager.add

        def changed_copy(stage, name, **arguments):
            if name.endswith('/include/zstd.h'):
                arguments['source'].write_bytes(b'changed between verification and copying')
            return original_add(stage, name, **arguments)

        with mock.patch.object(EXPORT.Stager, 'add', new=changed_copy), self.assertRaises(ValueError):
            self.run_export(native_compression_root=root)
        self.assertEqual(list(self.output.glob('*.tar.gz')), [])

    def test_optional_compression_rejects_linked_pack_file(self):
        root = self.optional_compression_root()
        leaf = root / 'native-compression/v1' / self.target / 'zstd'
        path = leaf / 'include/zstd.h'
        saved = self.root / 'external-header.h'
        path.rename(saved)
        try:
            path.symlink_to(saved)
        except OSError as error:
            self.skipTest('platform cannot create a file symlink: ' + str(error))
        with self.assertRaisesRegex(EXPORT.ExportError, 'symlink/reparse'):
            self.run_export(native_compression_root=root)

    def test_http_client_pack_exports_exact_payload_and_attribution(self):
        leaf=http_client_pack_fixture(self.bundle,self.target)
        expected=EXPORT.NATIVE_HTTP_CLIENT.validate_pack(leaf,self.target)
        result=self.run_export()
        manifest=json.loads(Path(result['manifest_path']).read_bytes())
        relative=f'native-http/v1/{self.target}'
        self.assertEqual({name for name in manifest['files'] if name.startswith('native-http/')},
            {relative+'/'+name for name in ('manifest.json','LICENSE.ku','LICENSE.dependencies','include/ku_native_http_client.h','lib/libku_native_http_client.a')})
        extracted=self.root/'http-extracted';extracted.mkdir()
        EXPORT.verify_archive(Path(result['asset_path']),manifest['files'],extracted)
        attribution=json.loads((extracted/'THIRD_PARTY.json').read_bytes())['native_http_client']
        self.assertEqual(attribution['source_sha256'],expected['source_sha256'])
        self.assertEqual(attribution['ureq_version'],'2.12.1')
        self.assertIn('native-http/v1/',(extracted/'README-INSTALL.md').read_text(encoding='utf-8'))

    def test_http_client_pack_rejects_nonplain_or_mismatched_inventory(self):
        leaf=http_client_pack_fixture(self.bundle,self.target)
        path=leaf/'manifest.json';saved=path.read_bytes()
        for key,value in [('abi_version',True),('source_sha256','a'*64),('target','aarch64-apple-darwin'),('rustc_version','1.89.0')]:
            data=json.loads(saved);data[key]=value;path.write_text(json.dumps(data))
            with self.subTest(key=key),self.assertRaises(EXPORT.ExportError):self.run_export()
            path.write_bytes(saved)
        extra=leaf/'private-cache';extra.mkdir()
        with self.assertRaises(EXPORT.ExportError):self.run_export()
        extra.rmdir()
        try:(leaf/'link').symlink_to(leaf/'include',target_is_directory=True)
        except OSError:self.skipTest('symlink unavailable')
        with self.assertRaises(EXPORT.ExportError):self.run_export()

    def test_calendar_pack_exports_exact_payload_and_attribution(self):
        leaf = calendar_pack_fixture(self.bundle, self.target)
        expected = EXPORT.NATIVE_CALENDAR.validate_pack(leaf, self.target)
        result = self.run_export()
        manifest = json.loads(Path(result['manifest_path']).read_bytes())
        relative = f'native-calendar/v1/{self.target}'
        self.assertEqual({name for name in manifest['files'] if name.startswith('native-calendar/')},
                         {relative + '/' + name for name in ('manifest.json', 'LICENSE.ku', 'LICENSE.chrono',
                          'include/ku_native_calendar.h', 'lib/libku_native_calendar.a')})
        extracted = self.root / 'calendar-extracted'
        extracted.mkdir()
        EXPORT.verify_archive(Path(result['asset_path']), manifest['files'], extracted)
        attribution = json.loads((extracted/'THIRD_PARTY.json').read_bytes())['native_calendar']
        self.assertEqual(attribution['source_sha256'], expected['source_sha256'])
        self.assertEqual(attribution['chrono_version'], '0.4.45')
        self.assertEqual(attribution['materials'], [relative+'/LICENSE.ku', relative+'/LICENSE.chrono'])
        self.assertIn('native-calendar/v1/', (extracted/'README-INSTALL.md').read_text(encoding='utf-8'))

    def test_calendar_pack_rejects_mixed_source_target_and_file_inventory(self):
        leaf = calendar_pack_fixture(self.bundle, self.target)
        path = leaf / 'manifest.json'
        saved = path.read_bytes()
        for key, value in [('source_sha256', 'a'*64), ('target', 'aarch64-apple-darwin'),
                           ('rustc_version', '1.89.0'), ('abi_version', 2)]:
            data = json.loads(saved)
            data[key] = value
            path.write_text(json.dumps(data))
            with self.subTest(key=key), self.assertRaises(EXPORT.ExportError):
                self.run_export()
            path.write_bytes(saved)
        extra = leaf/'private-cache'
        extra.write_bytes(b'not an installation payload')
        with self.assertRaises(EXPORT.ExportError):
            self.run_export()
        extra.unlink()
        (leaf/'LICENSE.chrono').unlink()
        with self.assertRaises(EXPORT.ExportError):
            self.run_export()
        self.assertEqual(list(self.output.glob('*.tar.gz')), [])

    def test_calendar_pack_default_validation_needs_no_source_directory(self):
        leaf = calendar_pack_fixture(self.bundle, self.target)
        with mock.patch.object(EXPORT.NATIVE_CALENDAR, 'source_hash', side_effect=AssertionError('source touched')):
            EXPORT.NATIVE_CALENDAR.validate_pack(leaf, self.target)
        library = leaf/'lib'/EXPORT.NATIVE_CALENDAR.archive_name(self.target)
        data = bytearray(library.read_bytes())
        struct.pack_into('<H', data, 8+60+18, 183)
        library.write_bytes(data)
        path = leaf/'manifest.json'
        manifest = json.loads(path.read_bytes())
        manifest['archive'] = EXPORT.NATIVE_CALENDAR.record(library)
        path.write_text(json.dumps(manifest))
        with self.assertRaisesRegex(ValueError, 'wrong-target'):
            EXPORT.NATIVE_CALENDAR.validate_pack(leaf, self.target)

    def test_roundtrip_hashes_original_licenses_and_file_set(self):
        result = self.run_export()
        archive = Path(result["asset_path"])
        manifest = json.loads(Path(result["manifest_path"]).read_text(encoding="utf-8"))
        self.assertEqual(archive.name, f"ku-v0.0.17-{self.target}.tar.gz")
        self.assertEqual(manifest["asset"], archive.name)
        self.assertEqual(manifest["commit"], "a" * 40)
        self.assertEqual(manifest["ku_license"], "Apache-2.0")
        self.assertEqual(manifest["size"], archive.stat().st_size)
        self.assertEqual(result["sha256"], hashlib.sha256(archive.read_bytes()).hexdigest())
        self.assertFalse(any(name.startswith('native-compression/') for name in manifest['files']))
        self.assertEqual(manifest["files"]["ku"]["mode"], 0o755)
        extracted = self.root / "extract"
        extracted.mkdir()
        EXPORT.verify_archive(archive, manifest["files"], extracted)
        license_path = extracted / "THIRD_PARTY/cargo/sample-1.2.3/LICENSE-MIT"
        self.assertEqual(license_path.read_bytes(), (self.package / "LICENSE-MIT").read_bytes())
        third_party = json.loads((extracted / "THIRD_PARTY.json").read_text(encoding="utf-8"))
        self.assertEqual([package["name"] for package in third_party["cargo"]], ["sample"])
        self.assertEqual(third_party["ku_license"], "Apache-2.0")
        self.assertEqual(third_party["ku_materials"], ["LICENSE", "NOTICE"])
        for name in ("LICENSE", "NOTICE"):
            self.assertEqual((extracted / name).read_bytes(), (self.repo / name).read_bytes())
        self.assertNotIn(str(self.root), json.dumps(third_party))
        sums = (extracted / "SHA256SUMS").read_text(encoding="utf-8").splitlines()
        self.assertEqual(len(sums), len(manifest["files"]) - 1)
        for line in sums:
            digest, name = line.split("  ", 1)
            self.assertEqual(digest, hashlib.sha256((extracted / name).read_bytes()).hexdigest())
        self.assertIn("Apache-2.0", (extracted / "README-INSTALL.md").read_text(encoding="utf-8"))

    def test_release_cli_requires_actual_archive_installation_check(self):
        arguments = ["export-release.py", "--bundle", str(self.bundle), "--output", str(self.output),
                     "--repo", str(self.repo), "--metadata", str(self.metadata),
                     "--rust-sysroot", str(self.sysroot), "--version", "0.0.17",
                     "--target", self.target, "--commit", "a" * 40]
        with mock.patch.object(sys, "argv", arguments), mock.patch.object(
                EXPORT, "export_release", return_value={}) as export, mock.patch("builtins.print"):
            self.assertEqual(EXPORT.main(), 0)
        self.assertIs(export.call_args.kwargs["verify_install"], True)

    def test_installation_uses_verified_extracted_archive_and_records_receipt(self):
        def verify(installed, repo, version, target):
            self.assertNotEqual(installed, self.bundle)
            self.assertEqual((installed / "ku").read_bytes(), (self.bundle / "ku").read_bytes())
            self.assertTrue((installed / "SHA256SUMS").is_file())
            return {"format": "test-receipt", "target": target}
        with mock.patch.object(EXPORT, "verify_installation", side_effect=verify) as check, mock.patch.object(
                EXPORT, "verify_default_native_installation", return_value={"status": "passed", "backend": "c"}) as native, mock.patch.object(
                EXPORT, "verify_runtime_installation", return_value={"status": "passed"}) as runtime:
            result = self.run_export(verify_install=True)
        self.assertEqual(check.call_count, 1)
        runtime.assert_called_once_with(check.call_args.args[0], self.repo, self.target)
        native.assert_called_once_with(check.call_args.args[0], self.repo, self.target)
        self.assertEqual(result["runtime_installation"], {"status": "passed"})
        self.assertEqual(result['default_native_installation'], {'status': 'passed', 'backend': 'c'})
        manifest = json.loads(Path(result["manifest_path"]).read_bytes())
        self.assertEqual(manifest["installation_check"], {"format": "test-receipt", "target": self.target})
        self.assertNotIn("runtime_installation", manifest)
        self.assertNotIn('default_native_installation', manifest)
        self.assertEqual(len(list(self.output.iterdir())), 2)

    def runtime_fixture(self):
        self.put(self.repo / "tests/acceptance/multiloop_load.py", b"not executed by unit tests\n")
        self.put(self.repo / "tests/fixtures/multiloop_service.ku", b"unmodified runtime fixture\n")
        return {"status": "passed", "cli_sha256": EXPORT.file_hash(self.bundle / "ku"),
                "fixture_sha256": EXPORT.file_hash(self.repo / "tests/fixtures/multiloop_service.ku"),
                "runs": [{"loops": 2, "exit_code": 0, "phases": [{"name": "short", "arrivals": 3,
                    "completed": 3, "counts": {"success": 3}, "client_shed": 0, "first_errors": []}],
                    "final_metrics": {
                        "loops": [dict(id=i, queued=0, running=0, resident=0, pending=0) for i in range(2)],
                        "tasks": [dict(id=i, ready=0, waiting=0, running=0, resident=0, timers=0) for i in range(2)],
                        "outcomes": [dict(id=i, queued_bytes=0) for i in range(2)]}}]}

    def runtime_process(self, report, directories, **overrides):
        def spawn(arguments, repo, working):
            self.assertEqual(arguments[:3], [sys.executable, "-B", str(self.repo / "tests/acceptance/multiloop_load.py")])
            self.assertEqual(arguments[3:13], ["--ku", str(self.bundle / "ku"), "--loops", "2",
                                             "--duration", "3", "--rps", "1", "--profile", "release"])
            destination = Path(arguments[-1])
            directories.append(destination.parent)
            self.assertEqual(repo, self.repo)
            self.assertEqual(working, destination.parent)
            destination.write_text(json.dumps(report), encoding="utf-8")
            return EXPORT.subprocess.CompletedProcess(arguments, overrides.get("returncode", 0),
                                                       overrides.get("stdout", b""), overrides.get("stderr", b""))
        return spawn

    def test_installed_runtime_smoke_checks_extracted_cli_and_cleans_temporary_tree(self):
        report, directories = self.runtime_fixture(), []
        with mock.patch.object(EXPORT, "run_runtime_check", side_effect=self.runtime_process(report, directories)):
            receipt = EXPORT.verify_runtime_installation(self.bundle, self.repo, self.target)
        self.assertEqual(receipt["requests"]["paths_in_order"], ["/ok", "/cpu", "/timer"])
        self.assertEqual(receipt["observed_loops"], 2)
        self.assertEqual(receipt["executable_sha256"], EXPORT.file_hash(self.bundle / "ku"))
        self.assertEqual(receipt["drain_exit_code"], 0)
        self.assertTrue(receipt["temporary_tree_removed"])
        self.assertTrue(receipt["process_tree_cleanup_confirmed"])
        self.assertTrue(directories and all(not root.exists() for root in directories))

    def test_runtime_smoke_rejects_wrong_bodies_partial_runs_and_live_records(self):
        baseline = self.runtime_fixture()
        mutations = [
            lambda value: value.update(cli_sha256="b" * 64),
            lambda value: value["runs"][0]["phases"][0].update(counts={"success": 2, "wrong_body": 1}),
            lambda value: value["runs"][0]["phases"][0].update(arrivals=2, completed=2),
            lambda value: value["runs"][0].update(exit_code=-9),
            lambda value: value["runs"][0]["final_metrics"]["tasks"][1].update(resident=1),
            lambda value: value["runs"][0]["final_metrics"]["outcomes"][0].update(queued_bytes=1),
            lambda value: value["runs"][0]["final_metrics"].update(loops=[]),
        ]
        for index, mutate in enumerate(mutations):
            report, directories = copy.deepcopy(baseline), []
            mutate(report)
            with self.subTest(index=index), mock.patch.object(EXPORT, "run_runtime_check",
                    side_effect=self.runtime_process(report, directories)):
                with self.assertRaises(EXPORT.RuntimeSmokeError) as caught:
                    EXPORT.verify_runtime_installation(self.bundle, self.repo, self.target)
                self.assertEqual(caught.exception.evidence["status"], "failed")
                self.assertIn("report_tail", caught.exception.evidence)
                self.assertTrue(caught.exception.evidence["temporary_tree_removed"])
                self.assertTrue(all(not root.exists() for root in directories))

    def test_runtime_smoke_owner_failure_is_not_reported_as_confirmed_cleanup(self):
        report, directories = self.runtime_fixture(), []
        spawn = self.runtime_process(report, directories)
        def failed(*arguments):
            spawn(*arguments)
            raise SystemExit("installed runtime smoke exceeded 180 seconds; cleanup unconfirmed")
        with mock.patch.object(EXPORT, "run_runtime_check", side_effect=failed):
            with self.assertRaisesRegex(EXPORT.RuntimeSmokeError, "180 seconds") as caught:
                EXPORT.verify_runtime_installation(self.bundle, self.repo, self.target)
        self.assertFalse(caught.exception.evidence["process_tree_cleanup_confirmed"])
        self.assertTrue(caught.exception.evidence["temporary_tree_removed"])
        self.assertTrue(all(not root.exists() for root in directories))

    def test_runtime_failure_does_not_publish_unchecked_archive(self):
        evidence = {"status": "failed", "temporary_tree_removed": True}
        with mock.patch.object(EXPORT, "verify_installation", return_value={}), mock.patch.object(
                EXPORT, "verify_default_native_installation", return_value={}), mock.patch.object(
                EXPORT, "verify_runtime_installation", side_effect=EXPORT.RuntimeSmokeError("runtime failed", evidence)):
            with self.assertRaisesRegex(EXPORT.RuntimeSmokeError, "runtime failed"):
                self.run_export(verify_install=True)
        self.assertFalse(any(self.output.iterdir()))

    def test_runtime_failure_logs_are_bounded_and_temporary_files_are_removed(self):
        report, directories = self.runtime_fixture(), []
        spawn = self.runtime_process(report, directories, returncode=1, stderr=b"failure evidence\n" * 1000)
        with mock.patch.object(EXPORT, "run_runtime_check", side_effect=spawn):
            with self.assertRaises(EXPORT.RuntimeSmokeError) as caught:
                EXPORT.verify_runtime_installation(self.bundle, self.repo, self.target)
        evidence = caught.exception.evidence
        self.assertEqual(evidence["driver_exit_code"], 1)
        self.assertEqual(len(evidence["stderr_tail"].encode()), 8192)
        self.assertTrue(evidence["temporary_tree_removed"])
        self.assertTrue(all(not root.exists() for root in directories))

    def test_runtime_wrapper_uses_driver_directory_for_sibling_imports(self):
        driver_directory = self.root / "driver helpers 目录"
        driver_directory.mkdir()
        working = self.root / "runtime-temp"
        working.mkdir()
        (driver_directory / "installed_runtime_sibling.py").write_text(
            "MARKER = 'trusted sibling'\n", encoding="utf-8")
        driver = driver_directory / "driver.py"
        driver.write_text(
            "import json, os, sys, tempfile\n"
            "from installed_runtime_sibling import MARKER\n"
            "print(json.dumps({'marker': MARKER, 'arguments': sys.argv[1:], "
            "'script_parent': os.path.realpath(sys.path[0]), "
            "'working_directory': os.path.realpath(os.getcwd()), "
            "'temporary_root': os.path.realpath(tempfile.gettempdir())}))\n",
            encoding="utf-8")
        direct = subprocess.run([sys.executable, "-B", str(driver), "argument with space"],
            cwd=self.root, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
            check=True, timeout=5)
        direct_report = json.loads(direct.stdout)
        result = EXPORT.run_runtime_check(
            [sys.executable, "-B", str(driver), "argument with space"], self.root, working)
        report = json.loads(result.stdout)
        self.assertEqual(report["marker"], direct_report["marker"])
        self.assertEqual(report["arguments"], direct_report["arguments"])
        self.assertEqual(report["script_parent"], str(driver_directory.resolve()))
        self.assertEqual(report["working_directory"], str(self.root))
        self.assertEqual(report["temporary_root"], str(working.resolve()))
        self.assertTrue(result.process_owner_cleanup["process_tree_cleanup_confirmed"])

    def test_runtime_owner_retires_descendant_even_after_launcher_exits(self):
        ready, escaped = self.root / "orphan-ready", self.root / "orphan-escaped"
        descendant = ("from pathlib import Path; import time; "
                      f"Path({str(ready)!r}).write_text('ready'); time.sleep(10); "
                      f"Path({str(escaped)!r}).write_text('escaped'); print('descendant-escaped',flush=True)")
        source = ("import subprocess,sys,time\nfrom pathlib import Path\n"
                  f"subprocess.Popen([sys.executable,'-B','-c',{descendant!r}])\n"
                  "deadline=time.monotonic()+2\n"
                  f"while not Path({str(ready)!r}).exists():\n"
                  "    if time.monotonic()>=deadline: raise RuntimeError('child did not start')\n"
                  "    time.sleep(.01)\n"
                  "print('parent-exited',flush=True)\n")
        driver = self.root / "orphan-driver.py"
        driver.write_text(source, encoding="utf-8")
        began = time.monotonic()
        result = EXPORT.run_runtime_check([sys.executable, "-B", str(driver)], self.root, self.root)
        self.assertEqual(result.stdout.replace(b"\r\n", b"\n"), b"parent-exited\n")
        self.assertLess(time.monotonic() - began, 5)
        self.assertTrue(ready.exists(), "the actual descendant never reached its held phase")
        self.assertFalse(escaped.exists())

    def test_runtime_result_is_single_cli_json_and_failure_only_uses_stderr(self):
        arguments = ["export-release.py", "--bundle", str(self.bundle), "--output", str(self.output),
                     "--repo", str(self.repo), "--metadata", str(self.metadata),
                     "--rust-sysroot", str(self.sysroot), "--version", "0.0.17",
                     "--target", self.target, "--commit", "a" * 40]
        result = {"asset_path": "asset", "runtime_installation": {"status": "passed"}}
        for failure in (False, True):
            stdout, stderr = io.StringIO(), io.StringIO()
            error = EXPORT.RuntimeSmokeError("failed smoke", {"status": "failed"})
            with self.subTest(failure=failure), mock.patch.object(sys, "argv", arguments), mock.patch.object(
                    EXPORT, "export_release", side_effect=error if failure else None, return_value=result), \
                    mock.patch.object(sys, "stdout", stdout), mock.patch.object(sys, "stderr", stderr):
                self.assertEqual(EXPORT.main(), 1 if failure else 0)
            if failure:
                self.assertEqual(stdout.getvalue(), "")
                self.assertEqual(json.loads(stderr.getvalue().splitlines()[0])["runtime_installation"]["status"], "failed")
            else:
                self.assertEqual(json.loads(stdout.getvalue()), result)
                self.assertEqual(stderr.getvalue(), "")

    def test_installed_commands_validate_version_examples_and_normal_exit(self):
        for name in ("hello.ku", "json.ku"):
            self.put(self.repo / "examples" / name, f"fixture {name}\r\n".encode())
        value = {"name": "中文😀", "count": 7, "active": True, "absent": None,
                 "embedded_nul": "left\0right", "nested": {"numbers": [[1, 2], [3]]}}
        outputs = ["ku 0.0.17\n", "check ok: hello.ku\n", "Hello Ku\n",
                   json.dumps(value) + "\nroundtrip ok\njson/parse_error\nerror at byte 7\n"]

        def command(executable, arguments, working):
            self.assertEqual(executable, self.bundle / "ku")
            for name in ("hello.ku", "json.ku"):
                self.assertEqual((working / name).read_bytes(), f"fixture {name}\n".encode())
            output = outputs.pop(0)
            return {"exit_code": 0, "stdout_bytes": len(output.encode()),
                    "stdout_sha256": hashlib.sha256(output.encode()).hexdigest(), "stderr_bytes": 0}, output

        with mock.patch.object(EXPORT.platform, "system", return_value="Linux"), mock.patch.object(
                EXPORT.platform, "machine", return_value="x86_64"), mock.patch.object(
                EXPORT, "install_command", side_effect=command) as run:
            receipt = EXPORT.verify_installation(self.bundle, self.repo, "0.0.17", self.target)
        self.assertEqual([row["name"] for row in receipt["checks"]],
                         ["version", "check-hello", "run-hello", "run-json"])
        self.assertEqual(run.call_count, 4)
        self.assertEqual(receipt["examples"], {
            name: hashlib.sha256(f"fixture {name}\n".encode()).hexdigest()
            for name in ("hello.ku", "json.ku")})
        self.assertEqual(receipt["executable_sha256"], hashlib.sha256((self.bundle / "ku").read_bytes()).hexdigest())
        with mock.patch.object(EXPORT.platform, "system", return_value="Linux"), mock.patch.object(
                EXPORT.platform, "machine", return_value="x86_64"), mock.patch.object(
                EXPORT, "install_command", return_value=({}, "ku 0.0.16\n")):
            with self.assertRaisesRegex(EXPORT.ExportError, "version does not match"):
                EXPORT.verify_installation(self.bundle, self.repo, "0.0.17", self.target)

    def test_default_native_installation_requires_real_c_command_and_source_free_run(self):
        self.put(self.repo / 'examples/hello.ku', b'fn main(){println("Hello Ku")}\r\n')
        calls = []

        def command(executable, arguments, working):
            calls.append((executable, arguments, working))
            if len(calls) == 1:
                self.assertEqual(executable, self.bundle / 'ku')
                self.assertEqual(arguments[:4], ['build', '--release', '--verbose', 'hello.ku'])
                self.assertNotIn('--backend', arguments)
                self.assertEqual((working / 'hello.ku').read_bytes(), b'fn main(){println("Hello Ku")}\n')
                Path(arguments[-1]).write_bytes(b'fixture native executable bytes')
                return {'exit_code': 0}, 'build profile: release\nc compiler command: cc argv\n'
            self.assertEqual(arguments, [])
            self.assertFalse(calls[0][2].exists())
            self.assertEqual(executable, Path(calls[0][1][-1]))
            self.assertNotEqual(working, calls[0][2])
            return {'exit_code': 0}, 'Hello Ku\n'

        with mock.patch.object(EXPORT, 'install_command', side_effect=command):
            receipt = EXPORT.verify_default_native_installation(self.bundle, self.repo, self.target)
        self.assertEqual(len(calls), 2)
        self.assertEqual(receipt['backend'], 'c')
        self.assertEqual(receipt['profile'], 'release')
        self.assertEqual(receipt['status'], 'passed')
        self.assertTrue(receipt['source_tree_removed'])
        self.assertTrue(receipt['temporary_tree_removed'])
        self.assertFalse(calls[1][2].exists())

    def test_default_native_installation_cannot_pass_with_runner_or_missing_native_artifact(self):
        self.put(self.repo / 'examples/hello.ku', b'fn main(){println("Hello Ku")}\n')
        for text in ('build profile: release\nrustc command: rustc argv\n',
                     'build profile: release\nc compiler command: cc argv\n'):
            def command(executable, arguments, working):
                if 'rustc command:' in text:
                    Path(arguments[-1]).write_bytes(b'existing wrong-backend executable')
                return {'exit_code': 0}, text
            with self.subTest(text=text), mock.patch.object(EXPORT, 'install_command', side_effect=command) as invoked:
                with self.assertRaisesRegex(EXPORT.ExportError, 'did not execute the Release C compiler'):
                    EXPORT.verify_default_native_installation(self.bundle, self.repo, self.target)
            self.assertEqual(invoked.call_count, 1)

    def test_example_canonicalization_changes_only_crlf_and_requires_utf8(self):
        source = "\ufeff中文😀\r\nkeep\rcontent  \n".encode("utf-8")
        self.assertEqual(EXPORT.canonical_example(source), "\ufeff中文😀\nkeep\rcontent  \n".encode("utf-8"))
        with self.assertRaises(UnicodeDecodeError):
            EXPORT.canonical_example(b"invalid\xff\r\n")

    def test_install_command_reaps_normal_and_expired_processes(self):
        receipt, output = EXPORT.install_command(Path(sys.executable), ["-c", "print('installed')"], self.root)
        self.assertEqual(receipt["exit_code"], 0)
        self.assertEqual(output, "installed\n")
        process = mock.Mock()
        process.poll.return_value = None
        with mock.patch.object(EXPORT.subprocess, "Popen", return_value=process), mock.patch.object(
                EXPORT.time, "monotonic", side_effect=(0, 31)):
            with self.assertRaisesRegex(EXPORT.ExportError, "exceeded 30 seconds"):
                EXPORT.install_command(self.bundle / "ku", ["run", "hello.ku"], self.root)
        process.kill.assert_called_once()
        process.wait.assert_called_once_with(timeout=10)

    def test_windows_bundle_and_asset_names(self):
        (self.bundle / "ku").rename(self.bundle / "ku.exe")
        previous = self.bundle / f"native-tls/v1/{self.target}"
        self.target = "x86_64-pc-windows-msvc"
        current = self.bundle / f"native-tls/v1/{self.target}"
        previous.rename(current)
        (current / "lib/libku_native_tls.a").rename(current / "lib/ku_native_tls.lib")
        self.put(self.bundle / "ku.pdb", b"debug symbols")
        self.put(self.bundle / "deps/sample_derive-0123456789abcdef.dll", b"Cargo emitted Windows procedural macro")
        self.make_runtime_manifest()
        result = self.run_export()
        self.assertIn(self.target, result["manifest_path"])

    def test_windows_gnu_bundle_has_exe_and_gnu_archive_with_matching_host(self):
        (self.bundle / "ku").rename(self.bundle / "ku.exe")
        previous = self.bundle / f"native-tls/v1/{self.target}"
        self.target = "x86_64-pc-windows-gnu"
        current = self.bundle / f"native-tls/v1/{self.target}"
        previous.rename(current)
        self.put(self.bundle / "deps/sample_derive-0123456789abcdef.dll", b"GNU-host Cargo procedural macro")
        self.make_runtime_manifest()
        result = self.run_export()
        self.assertIn(self.target, result["manifest_path"])
        receipt = json.loads(Path(result["manifest_path"]).read_text(encoding="utf-8"))
        self.assertIn("ku.exe", receipt["files"])
        self.assertIn(f"native-tls/v1/{self.target}/lib/libku_native_tls.a", receipt["files"])
        self.assertEqual(EXPORT.RUNTIME_BUNDLE.read_json(self.bundle / "ku-runtime.json")["rustc"]["host"], self.target)

    def test_target_specific_proc_macro_names(self):
        positive = {
            "x86_64-pc-windows-msvc": "deps/sample_derive-0123456789abcdef.dll",
            "x86_64-pc-windows-gnu": "deps/sample_derive-0123456789abcdef.dll",
            "x86_64-unknown-linux-gnu": "deps/libsample_derive-0123456789abcdef.so",
            "aarch64-apple-darwin": "deps/libsample_derive-0123456789abcdef.dylib",
        }
        for target, name in positive.items():
            with self.subTest(target=target):
                self.assertTrue(EXPORT.dependency_name(name, target))
                self.assertTrue(EXPORT.dependency_name("deps/libsample-0123456789abcdef.rlib", target))
                for other in set(positive.values()) - {name}:
                    self.assertFalse(EXPORT.dependency_name(other, target))
                for invalid in ("deps/kernel32.dll", "deps/sample.dll", "deps/sample-abc.dll",
                                "deps/libsample.so.1", "deps/libsample.rlib", "deps/libsample-ABCDEF0123456789.rlib"):
                    self.assertFalse(EXPORT.dependency_name(invalid, target))
        self.put(self.bundle / positive[self.target], b"Cargo emitted Linux procedural macro")
        self.make_runtime_manifest()
        self.run_export()

    def test_wrong_platform_or_arbitrary_dynamic_library_is_not_exported(self):
        for index, name in enumerate(("deps/kernel32.dll", "deps/sample-0123456789abcdef.dll",
                                      "deps/libsample-0123456789abcdef.dylib")):
            with self.subTest(name=name):
                path = self.bundle / name
                self.put(path, b"not a permitted Linux dependency")
                with self.assertRaisesRegex(EXPORT.ExportError, "unexpected file"):
                    self.run_export(output=self.root / f"bad-output-{index}")
                path.unlink()

    def test_reproducible_archive_and_no_overwrite(self):
        first = self.run_export()
        second = self.run_export(output=self.root / "output2")
        self.assertEqual(first["sha256"], second["sha256"])
        before = Path(first["asset_path"]).read_bytes()
        with self.assertRaisesRegex(EXPORT.ExportError, "refusing overwrite"):
            self.run_export()
        self.assertEqual(before, Path(first["asset_path"]).read_bytes())

    def test_invalid_identity_rejected(self):
        for overrides in ({"version": "v0.0.17"}, {"version": "0.00.17"}, {"version": "0.0.17/evil"},
                          {"target": "../linux"}, {"target": "x86_64-linux"},
                          {"commit": "a" * 39}, {"commit": "A" * 40}, {"commit": "a" * 40 + "\n"}):
            with self.subTest(overrides=overrides), self.assertRaises(EXPORT.ExportError):
                self.run_export(**overrides)

    def test_cross_platform_traversal_and_device_names_rejected(self):
        for name in ("../x", "/x", "a/../../x", "C:/x", "a\\x", "a//b", "./a", "a/",
                     "a\nx", "a\x00x", "a:stream", "NUL.txt", "a/COM1", "a.", "a ", "a?x", "a|x"):
            with self.subTest(name=name), self.assertRaises(EXPORT.ExportError):
                EXPORT.checked_relative(name)

    def test_missing_bundle_dependency_and_extra_file_rejected(self):
        dependency = self.bundle / "deps/libsample-0123456789abcdef.rlib"
        dependency.unlink()
        with self.assertRaisesRegex(EXPORT.ExportError, "missing required"):
            self.run_export()
        self.put(dependency, b"restored")
        self.put(self.bundle / "secrets.txt", b"must not be published")
        with self.assertRaisesRegex(EXPORT.ExportError, "unexpected file"):
            self.run_export()

    def test_size_and_count_budgets(self):
        with mock.patch.object(EXPORT, "MAX_BYTES", 1), self.assertRaisesRegex(EXPORT.ExportError, "size exceeds"):
            self.run_export()
        with mock.patch.object(EXPORT, "MAX_FILES", 2), self.assertRaisesRegex(EXPORT.ExportError, "count exceeds"):
            self.run_export()
        with mock.patch.object(EXPORT, "MAX_LICENSE", 1), self.assertRaisesRegex(EXPORT.ExportError, "size limit"):
            self.run_export()

    def test_missing_license_and_copyright_do_not_silently_pass(self):
        for name in ("LICENSE", "NOTICE"):
            original = (self.repo / name).read_bytes()
            (self.repo / name).unlink()
            # Preserve read_bounded's existing I/O contract; main() reports
            # OSError as a failed export with the missing material's path.
            with self.assertRaises(FileNotFoundError) as missing:
                self.run_export()
            self.assertEqual(Path(missing.exception.filename), self.repo / name)
            self.put(self.repo / name, original)
        self.data["packages"][0]["license"] = None
        self.write_metadata()
        with self.assertRaisesRegex(EXPORT.ExportError, "must declare Apache-2.0"):
            self.run_export()
        self.data["packages"][0]["license"] = "Apache-2.0"
        self.write_metadata()
        (self.package / "LICENSE-MIT").unlink()
        with self.assertRaisesRegex(EXPORT.ExportError, "missing original license"):
            self.run_export()
        self.put(self.package / "LICENSE-MIT", b"original")
        (self.sysroot / "share/doc/rust/COPYRIGHT-library.html").unlink()
        with self.assertRaisesRegex(EXPORT.ExportError, "copyright attribution"):
            self.run_export()

    def test_explicit_license_file_is_copied_but_cannot_escape_crate(self):
        (self.package / "LICENSE-MIT").unlink()
        self.put(self.package / "legal/terms.txt", b"explicit original terms")
        self.data["packages"][1]["license_file"] = "legal/terms.txt"
        self.write_metadata()
        result = self.run_export()
        manifest = json.loads(Path(result["manifest_path"]).read_text(encoding="utf-8"))
        self.assertIn("THIRD_PARTY/cargo/sample-1.2.3/legal/terms.txt", manifest["files"])
        self.data["packages"][1]["license_file"] = "../outside.txt"
        self.put(self.package.parent / "outside.txt", b"outside")
        self.write_metadata()
        with self.assertRaisesRegex(EXPORT.ExportError, "escapes crate"):
            self.run_export(output=self.root / "output2")

    def test_production_graph_excludes_dev_and_unreachable_dependencies(self):
        dev = copy.deepcopy(self.data["packages"][1])
        dev.update(name="dev-only", id="dev-only", manifest_path="not-read-missing-manifest")
        self.data["packages"].append(dev)
        self.data["resolve"]["nodes"][0]["deps"].append({"pkg": "dev-only", "dep_kinds": [{"kind": "dev"}]})
        self.data["resolve"]["nodes"].append({"id": "dev-only", "deps": []})
        self.write_metadata()
        self.run_export()

    def test_production_graph_rejects_missing_nodes_and_unknown_kinds(self):
        self.data["resolve"]["nodes"].pop()
        with self.assertRaisesRegex(EXPORT.ExportError, "incomplete"):
            EXPORT.production_packages(self.data)
        self.data["resolve"]["nodes"][0]["deps"][0]["dep_kinds"][0]["kind"] = "unknown"
        with self.assertRaisesRegex(EXPORT.ExportError, "unknown dependency"):
            EXPORT.production_packages(self.data)
        self.data["resolve"] = None
        with self.assertRaisesRegex(EXPORT.ExportError, "complete resolve"):
            EXPORT.production_packages(self.data)

    def test_symlinks_rejected_in_bundle_and_license_material(self):
        link = self.bundle / "link"
        try:
            link.symlink_to(self.package / "LICENSE-MIT")
        except (OSError, NotImplementedError):
            self.skipTest("symlink creation unavailable on this host")
        with self.assertRaisesRegex(EXPORT.ExportError, "symlink/reparse"):
            self.run_export()
        link.unlink()
        (self.package / "LICENSE-MIT").unlink()
        (self.package / "LICENSE-MIT").symlink_to(self.package / "NOTICE")
        with self.assertRaisesRegex(EXPORT.ExportError, "symlink/reparse"):
            self.run_export()

    def test_output_inside_bundle_rejected(self):
        with self.assertRaisesRegex(EXPORT.ExportError, "outside the input"):
            self.run_export(output=self.bundle / "output")

    def make_archive(self, members):
        archive = self.root / "malformed.tar.gz"
        with tarfile.open(archive, "w:gz") as output:
            for name, value, typecode in members:
                member = tarfile.TarInfo(name)
                member.size = len(value)
                member.mode = 0o644
                member.type = typecode
                if typecode == tarfile.SYMTYPE:
                    member.linkname = "../outside"
                output.addfile(member, io.BytesIO(value))
        return archive

    def test_archive_rejects_hash_extra_missing_duplicate_traversal_and_symlink(self):
        expected = {"payload": {"size": 4, "sha256": hashlib.sha256(b"good").hexdigest(), "mode": 0o644}}
        cases = [
            ([("payload", b"evil", tarfile.REGTYPE)], "hash mismatch"),
            ([("payload", b"good", tarfile.REGTYPE), ("extra", b"", tarfile.REGTYPE)], "extra file"),
            ([], "missing expected"),
            ([("payload", b"good", tarfile.REGTYPE)] * 2, "duplicate"),
            ([("../outside", b"good", tarfile.REGTYPE)], "unsafe archive path"),
            ([("payload", b"", tarfile.SYMTYPE)], "non-plain"),
            ([("payload", b"longer", tarfile.REGTYPE)], "size mismatch"),
        ]
        for index, (members, message) in enumerate(cases):
            with self.subTest(message=message):
                archive = self.make_archive(members)
                destination = self.root / f"bad-extract-{index}"
                destination.mkdir()
                with self.assertRaisesRegex(EXPORT.ExportError, message):
                    EXPORT.verify_archive(archive, expected, destination)
        self.assertFalse((self.root / "outside").exists())

    def test_extension_headers_rejected_before_unbounded_metadata_read(self):
        expected = {"payload": {"size": 4, "sha256": hashlib.sha256(b"good").hexdigest(), "mode": 0o644}}
        for index, typecode in enumerate((tarfile.XHDTYPE, tarfile.XGLTYPE, tarfile.GNUTYPE_LONGNAME,
                                          tarfile.GNUTYPE_LONGLINK, tarfile.GNUTYPE_SPARSE)):
            with self.subTest(typecode=typecode):
                header = tarfile.TarInfo("payload")
                header.type = typecode
                header.size = 2 * 1024 * 1024 * 1024
                archive = self.root / f"extension-{index}.tar.gz"
                archive.write_bytes(gzip.compress(header.tobuf(format=tarfile.USTAR_FORMAT)))
                destination = self.root / f"extension-extract-{index}"
                destination.mkdir()
                with self.assertRaisesRegex(EXPORT.ExportError, "non-plain USTAR"):
                    EXPORT.verify_archive(archive, expected, destination)

    def test_public_header_entrypoint_owns_read_size_before_any_extension_processor(self):
        class OneHeader(io.BytesIO):
            def __init__(self, data):
                super().__init__(data)
                self.requests = []

            def read(self, size=-1):
                self.requests.append(size)
                if size != tarfile.BLOCKSIZE or self.tell() != 0:
                    raise AssertionError("extension payload was read before rejection")
                return super().read(size)

        for typecode in (tarfile.XHDTYPE, tarfile.XGLTYPE, tarfile.GNUTYPE_LONGNAME,
                         tarfile.GNUTYPE_LONGLINK, tarfile.GNUTYPE_SPARSE):
            with self.subTest(typecode=typecode):
                member = tarfile.TarInfo("payload")
                member.type, member.size = typecode, 2 * 1024 * 1024 * 1024
                stream = OneHeader(member.tobuf(format=tarfile.USTAR_FORMAT))
                archive = SimpleNamespace(fileobj=stream, encoding="utf-8", errors="strict", offset=0)
                with mock.patch.object(tarfile.TarInfo, "_proc_member", side_effect=AssertionError("unbounded extension dispatch")):
                    with self.assertRaisesRegex(EXPORT.ExportError, "non-plain USTAR"):
                        EXPORT.PlainTarInfo.fromtarfile(archive)
                self.assertEqual(stream.requests, [512])
                self.assertEqual(archive.offset, 0)

    def test_archive_reader_does_not_inherit_version_specific_header_dispatch(self):
        # Newer CPython releases bypass the public frombuf hook in their base
        # fromtarfile implementation. Neither that implementation nor any of its
        # private extension dispatchers may be part of our verification path.
        expected = {"payload": {"size": 4, "sha256": hashlib.sha256(b"good").hexdigest(), "mode": 0o644}}
        archive = self.make_archive([("payload", b"good", tarfile.REGTYPE)])
        destination = self.root / "own-header-entrypoint"
        destination.mkdir()
        with mock.patch.object(tarfile.TarInfo, "fromtarfile", side_effect=AssertionError("inherited header dispatch")), \
             mock.patch.object(tarfile.TarInfo, "_proc_member", side_effect=AssertionError("private extension dispatch")):
            EXPORT.verify_archive(archive, expected, destination)
        self.assertEqual((destination / "payload").read_bytes(), b"good")

    def test_gzip_crc_and_nonzero_or_excessive_trailers_rejected(self):
        expected = {"payload": {"size": 4, "sha256": hashlib.sha256(b"good").hexdigest(), "mode": 0o644}}
        archive = self.make_archive([("payload", b"good", tarfile.REGTYPE)])
        original = archive.read_bytes()
        raw = gzip.decompress(original)
        cases = [
            (gzip.compress(raw + b"hidden-data"), "trailing data"),
            (gzip.compress(raw + bytes(EXPORT.MAX_TAR_TRAILER + 1)), "trailing data"),
            (original[:-8] + bytes([original[-8] ^ 1]) + original[-7:], "CRC check failed"),
        ]
        for index, (value, message) in enumerate(cases):
            with self.subTest(message=message):
                archive.write_bytes(value)
                destination = self.root / f"trailer-extract-{index}"
                destination.mkdir()
                with self.assertRaisesRegex((EXPORT.ExportError, gzip.BadGzipFile), message):
                    EXPORT.verify_archive(archive, expected, destination)



class InstalledPostgresDirectoryTests(unittest.TestCase):
    def setUp(self):
        spec = importlib.util.spec_from_file_location("ku_installed_pg_directory_test", Path(__file__).with_name("verify-installed-pg.py"))
        self.gate = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(self.gate)
        self.repo = Path(__file__).resolve().parents[2]

    def exercise_windows_setup(self, root, report, powershell_result):
        gate = self.gate
        driver = self.repo / "tests" / "acceptance" / "http_pg_lifecycle.py"
        original_path = sys.path[:]
        events = []
        load = gate.load_trusted
        def trusted_load(path, name):
            self.assertEqual(path, driver)
            self.assertEqual(sys.path[0], str(driver.parent.resolve()))
            module = load(path, name)
            self.assertEqual(Path(module.process_sample.__code__.co_filename).resolve(),
                             driver.with_name("multiloop_load.py").resolve())
            return module
        def acl(command, **kwargs):
            self.assertEqual(command[0], "powershell.exe")
            self.assertEqual(kwargs["env"]["KU_PG_FIXTURE_ROOT"], str(root.resolve()))
            self.assertFalse((root / "fixture-work").exists())
            self.assertNotIn("pg_fixture_invoked", report)
            events.append("outer-root-acl")
            return powershell_result
        def runtime(arguments, repo, working):
            self.assertEqual(sys.path, original_path)
            self.assertEqual(working, root / "fixture-work")
            self.assertEqual(report["windows_pg_directory_acl_root"], str(root))
            self.assertTrue(working.is_dir())
            self.assertEqual(arguments[arguments.index("--lane") + 1], "both")
            self.assertTrue(report["pg_fixture_invoked"])
            self.assertTrue(report["windows_pg_directory_acl"]["acl"]["current_user_modify_inheritable"])
            events.append("original-contained-fixture")
            return subprocess.CompletedProcess(arguments, 0)
        exporter = SimpleNamespace(run_runtime_check=mock.Mock(side_effect=runtime))
        try:
            with mock.patch.object(gate, "os", SimpleNamespace(name="nt")), \
                 mock.patch.object(gate.sys, "platform", "win32"), \
                 mock.patch.object(gate, "load_trusted", side_effect=trusted_load), \
                 mock.patch.object(gate.subprocess, "run", side_effect=acl) as powershell:
                gate.run_pg_lifecycles(exporter, None, self.repo, root / "installed", root,
                                       root / "lifecycle.json", report)
        finally:
            self.assertEqual(sys.path, original_path)
            if powershell_result.returncode or not root.name.startswith("ku-http-pg-acceptance-") or root.parent != Path(tempfile.gettempdir()).resolve():
                exporter.run_runtime_check.assert_not_called()
                self.assertFalse((root / "fixture-work").exists())
            if not root.name.startswith("ku-http-pg-acceptance-") or root.parent != Path(tempfile.gettempdir()).resolve():
                powershell.assert_not_called()
        return events, powershell

    def test_windows_owned_root_acl_precedes_working_directory_and_original_fixture(self):
        acl = {"parent_sddl_before": "parent", "parent_sddl_after": "parent",
               "current_user_modify_inheritable": True}
        success = SimpleNamespace(returncode=0, stdout=json.dumps(acl), stderr="")
        report = {}
        with mock.patch.object(self.gate, "os", SimpleNamespace(name="nt")), \
             self.gate.owned_temporary_root(report) as root:
            self.addCleanup(shutil.rmtree, root, ignore_errors=True)
            self.assertTrue(root.name.startswith("ku-http-pg-acceptance-"))
            self.assertEqual(root.parent, Path(tempfile.gettempdir()).resolve())
            events, powershell = self.exercise_windows_setup(root, report, success)
            self.assertEqual(events, ["outer-root-acl", "original-contained-fixture"])
            powershell.assert_called_once()
            report["fixture_cleanup_confirmed"] = True
        self.assertTrue(report["temporary_cleanup_confirmed"])
        self.assertFalse(root.exists())

    def test_windows_outer_acl_failure_never_starts_fixture_and_root_is_cleaned(self):
        failure = SimpleNamespace(returncode=1, stdout="{}", stderr="ACL setup denied")
        report = {}
        with self.assertRaises(RuntimeError):
            with mock.patch.object(self.gate, "os", SimpleNamespace(name="nt")), \
                 self.gate.owned_temporary_root(report) as root:
                self.addCleanup(shutil.rmtree, root, ignore_errors=True)
                self.exercise_windows_setup(root, report, failure)
        self.assertNotIn("pg_fixture_invoked", report)
        self.assertEqual(report["windows_pg_directory_acl"]["exit_code"], 1)
        self.assertTrue(report["temporary_cleanup_confirmed"])
        self.assertFalse(root.exists())

    def test_existing_fixture_guard_still_refuses_other_prefix_and_nested_root(self):
        success = SimpleNamespace(returncode=0, stdout="{}", stderr="")
        with tempfile.TemporaryDirectory(prefix="ku-installed-pg-") as temporary:
            outer = Path(temporary).resolve()
            nested = outer / "ku-http-pg-acceptance-nested"
            nested.mkdir()
            for root in (outer, nested):
                with self.subTest(root=root), self.assertRaises(ValueError):
                    self.exercise_windows_setup(root, {}, success)

    def test_posix_owned_root_keeps_original_prefix(self):
        report = {}
        with mock.patch.object(self.gate, "os", SimpleNamespace(name="posix")), \
             self.gate.owned_temporary_root(report) as root:
            self.assertTrue(root.name.startswith("ku-installed-pg-"))
        self.assertTrue(report["temporary_cleanup_confirmed"])


@unittest.skipIf(os.name == "nt", "POSIX foreground postgres owner; Windows uses the existing Job")
class InstalledPostgresOwnerTests(unittest.TestCase):
    def setUp(self):
        spec = importlib.util.spec_from_file_location("ku_installed_pg_owner_test", Path(__file__).with_name("verify-installed-pg.py"))
        self.gate = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(self.gate)
        self.temporary = tempfile.TemporaryDirectory(prefix="ku-pg-owner-unit-")
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name).resolve() / "private-cluster"
        self.root.mkdir(mode=0o700)
        (self.root / "data").mkdir()
        self.events = []
        self.process = mock.Mock(pid=190101, returncode=None)
        self.process.poll.side_effect = AssertionError("early poll would reap the signal anchor")
        self.process.communicate.side_effect = AssertionError("early communicate would reap the signal anchor")
        self.process.wait.side_effect = lambda **kwargs: self.events.append("final-reap") or 0
        self.snapshots = [[(190101, os.getpid(), 190101, "Z")]]
        self.owner = self.gate.PosixPrivatePostgres(self.root, Path("/inert/postgresql"), "sync", self.tool)
        self.owner.root_identity = (self.root.stat().st_dev, self.root.stat().st_ino)
        self.owner.data_identity = ((self.root / "data").stat().st_dev, (self.root / "data").stat().st_ino)
        self.owner.tool_identities = {}
        self.owner.process, self.owner.data, self.owner.port = self.process, self.root / "data", 19192
        self.owner.log = mock.Mock()
        self.gate.HELD_POSTGRES_OWNERS.append(self.owner)
        self.addCleanup(self.gate.HELD_POSTGRES_OWNERS.clear)
        self.pid_file = self.root / "data/postmaster.pid"
        self.pid_file.write_text(f"190101\n{self.root / 'data'}\n1700000000\n19192\n\n\n\nready\n")
        self.stack = contextlib.ExitStack()
        self.addCleanup(self.stack.close)
        self.stack.enter_context(mock.patch.object(self.gate.signal, "getsignal", return_value=self.gate.signal.SIG_DFL))
        self.stack.enter_context(mock.patch.object(self.gate.os, "getpgid", return_value=190101))
        self.stack.enter_context(mock.patch.object(self.gate.os, "getsid", return_value=190101))
        self.kill = self.stack.enter_context(mock.patch.object(self.gate.os, "killpg", side_effect=lambda pid, sig: self.events.append(("owned-signal", pid, sig))))
        self.stack.enter_context(mock.patch.object(self.gate.time, "sleep"))

    def tool(self, command, timeout):
        self.assertGreater(timeout, 0)
        if Path(command[0]).name == "pg_ctl":
            self.events.append("pg_ctl-stop")
            self.assertEqual(command[1:3], ["-D", str(self.owner.data)])
            self.assertEqual(command[-3:], ["stop", "-m", "immediate"])
            return subprocess.CompletedProcess(command, 0, b"", b"")
        self.assertEqual(command, ["ps", "-axo", "pid=,ppid=,pgid=,uid=,state="])
        self.events.append("independent-snapshot")
        rows = self.snapshots[0]
        if len(self.snapshots) > 1:
            self.snapshots.pop(0)
        content = "".join(f"{pid} {ppid} {pgid} {os.getuid()} {state}\n" for pid, ppid, pgid, state in rows).encode()
        return subprocess.CompletedProcess(command, 0, content, b"")

    def test_foreground_start_holds_child_without_polling_and_disables_unused_unix_socket(self):
        tools = self.root.parent / "tools"
        tools.mkdir()
        for name in ("postgres", "pg_ctl"):
            (tools / name).write_bytes(("inert " + name).encode())
        original = self.owner
        self.gate.HELD_POSTGRES_OWNERS.remove(original)
        self.owner = self.gate.PosixPrivatePostgres(self.root.parent / "launch", tools, "sync", self.tool)
        def run_tool(command, timeout):
            if Path(command[0]).name == "initdb":
                self.owner.data.mkdir()
                return subprocess.CompletedProcess(command, 0, b"", b"")
            return self.tool(command, timeout)
        self.owner.run_tool = run_tool
        def launch(command, **kwargs):
            self.assertTrue(kwargs["start_new_session"])
            self.assertEqual(kwargs["cwd"], self.owner.root)
            self.assertEqual(command[command.index("-k") + 1], "")
            (self.owner.data / "postmaster.pid").write_text(f"190101\n{self.owner.data}\n1700000000\n19192\n\n\n\nready\n")
            return self.process
        listener = mock.MagicMock()
        listener.__enter__.return_value.getsockname.return_value = ("127.0.0.1", 19192)
        with mock.patch.object(self.gate.socket, "socket", return_value=listener), \
             mock.patch.object(self.gate.subprocess, "Popen", side_effect=launch):
            configured = self.owner.start()
        spec = json.loads(configured.read_bytes())
        self.assertEqual(spec["owner_pid"], os.getpid())
        self.assertEqual(spec["lane"], "sync")
        self.assertEqual(len(self.owner.receipt["postgres_sha256"]), 64)
        self.process.poll.assert_not_called()
        self.process.wait.assert_not_called()
        self.process.communicate.assert_not_called()
        self.owner.cleanup()

    def test_darwin_zombie_esrch_uses_direct_parent_group_and_never_signals(self):
        with mock.patch.object(self.gate.os, "getpgid", side_effect=ProcessLookupError), \
             mock.patch.object(self.gate.os, "getsid", side_effect=ProcessLookupError):
            self.owner.cleanup()
        self.assertEqual(self.events, ["independent-snapshot", "final-reap"])
        self.assertTrue(self.owner.receipt["cleanup_confirmed"])
        self.assertIsNone(self.owner.receipt["snapshots"][0][0]["session"])
        self.process.poll.assert_not_called()
        self.process.communicate.assert_not_called()
        self.kill.assert_not_called()

    def test_unqueryable_non_zombie_never_authorizes_signal_or_reap(self):
        self.snapshots = [[(190101, os.getpid(), 190101, "S")]]
        with mock.patch.object(self.gate.os, "getpgid", side_effect=ProcessLookupError), \
             mock.patch.object(self.gate.os, "getsid", side_effect=ProcessLookupError), \
             self.assertRaises(TimeoutError):
            self.owner.cleanup()
        self.process.wait.assert_not_called()
        self.kill.assert_not_called()
        self.assertNotIn("pg_ctl-stop", self.events)
        self.assertFalse(self.owner.receipt["cleanup_confirmed"])

    def test_real_retained_instant_child_is_reaped_only_after_ps_zombie_proof(self):
        # Exercise the actual host API without PostgreSQL, signals, or compilers.
        # Darwin returns ESRCH from getpgid/getsid for this still-unreaped child.
        self.stack.close()
        child = subprocess.Popen([sys.executable, "-B", "-c", "pass"],
            start_new_session=True, stdin=subprocess.DEVNULL,
            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        self.owner.process = child
        self.pid_file.unlink()
        def actual_ps(command, timeout):
            self.assertEqual(command, ["ps", "-axo", "pid=,ppid=,pgid=,uid=,state="])
            return subprocess.run(command, check=True, stdout=subprocess.PIPE,
                                  stderr=subprocess.PIPE, timeout=timeout)
        self.owner.run_tool = actual_ps
        try:
            deadline = time.monotonic() + 3
            while not self.owner.snapshot(1):
                self.assertLess(time.monotonic(), deadline)
                time.sleep(.01)
            self.assertIsNone(child.returncode)
            self.owner.cleanup()
            self.assertTrue(self.owner.receipt["cleanup_confirmed"])
            self.assertEqual(child.returncode, 0)
            self.assertNotIn("owned_group_signals", self.owner.receipt)
        finally:
            child.wait(timeout=3)

    def test_changed_data_inode_is_not_signaled_or_reaped(self):
        self.owner.data.rename(self.root / "old-data")
        self.owner.data.mkdir()
        with self.assertRaises(ValueError): self.owner.cleanup()
        self.kill.assert_not_called()
        self.process.wait.assert_not_called()

    def test_pg_ctl_then_independent_empty_group_then_one_final_reap(self):
        self.snapshots = [[(190101, os.getpid(), 190101, "S")], [(190101, os.getpid(), 190101, "Z")]]
        self.owner.cleanup()
        self.assertEqual(self.events, ["independent-snapshot", "pg_ctl-stop", "independent-snapshot", "final-reap"])
        self.assertTrue(self.owner.receipt["cleanup_confirmed"])
        self.assertTrue(self.owner.signal_closed)
        self.assertNotIn(self.owner, self.gate.HELD_POSTGRES_OWNERS)
        self.process.poll.assert_not_called()
        self.process.communicate.assert_not_called()
        self.kill.assert_not_called()

    def test_child_group_shutdown_uses_only_unreaped_anchor_and_never_signals_after_reap(self):
        self.snapshots = [[(190101, os.getpid(), 190101, "S"), (190102, 190101, 190101, "S")], [(190101, os.getpid(), 190101, "S"), (190102, 190101, 190101, "S")], [(190101, os.getpid(), 190101, "Z")]]
        self.owner.cleanup()
        self.kill.assert_called_once_with(190101, self.gate.signal.SIGQUIT)
        self.assertEqual(self.events[-1], "final-reap")
        self.assertTrue(self.owner.receipt["cleanup_confirmed"])
        self.process.poll.assert_not_called()
        self.process.communicate.assert_not_called()

    def test_changed_pid_record_session_or_direct_parent_is_never_signaled_or_reaped(self):
        original = self.pid_file.read_text()
        with self.subTest(case="pid-record"):
            self.pid_file.write_text(original.replace("190101\n", "190999\n", 1))
            with self.assertRaises(ValueError): self.owner.cleanup()
        self.pid_file.write_text(original)
        with self.subTest(case="session"), mock.patch.object(self.gate.os, "getsid", return_value=190999):
            with self.assertRaises(ValueError): self.owner.cleanup()
        self.snapshots = [[(190101, 190999, 190101, "Z")]]
        with self.subTest(case="parent"), self.assertRaises(ValueError): self.owner.cleanup()
        self.kill.assert_not_called()
        self.process.wait.assert_not_called()
        self.assertIn(self.owner, self.gate.HELD_POSTGRES_OWNERS)
        self.assertFalse(self.owner.receipt["cleanup_confirmed"])

    def test_sigchld_ignored_reaped_child_and_missing_anchor_fail_closed(self):
        with mock.patch.object(self.gate.signal, "getsignal", return_value=self.gate.signal.SIG_IGN), self.assertRaises(ValueError): self.owner.cleanup()
        self.process.returncode = 0
        with self.assertRaises(ValueError): self.owner.cleanup()
        self.process.returncode = None
        self.snapshots = [[]]
        with mock.patch.object(self.gate.os, "getpgid", side_effect=ProcessLookupError), self.assertRaises(ValueError): self.owner.cleanup()
        self.kill.assert_not_called()
        self.process.wait.assert_not_called()

    def test_persistent_members_are_failure_not_directory_deletion_or_success(self):
        self.snapshots = [[(190101, os.getpid(), 190101, "S"), (190102, 190101, 190101, "S")]]
        with self.assertRaises(TimeoutError): self.owner.cleanup()
        self.assertEqual(self.kill.call_args_list, [mock.call(190101, self.gate.signal.SIGQUIT), mock.call(190101, self.gate.signal.SIGKILL)])
        self.process.wait.assert_not_called()
        self.assertTrue(self.owner.root.is_dir())
        self.assertFalse(self.owner.receipt["cleanup_confirmed"])

    def test_failed_pg_ctl_still_uses_exact_owned_group_and_confirms_reclaim(self):
        self.snapshots = [[(190101, os.getpid(), 190101, "S")], [(190101, os.getpid(), 190101, "Z")]]
        original = self.owner.run_tool
        def fail_ctl(command, timeout):
            if Path(command[0]).name == "pg_ctl": raise TimeoutError("mock tool timeout")
            return original(command, timeout)
        self.owner.run_tool = fail_ctl
        self.owner.cleanup()
        self.assertEqual(self.owner.receipt["pg_ctl_stop"], "TimeoutError")
        self.assertTrue(self.owner.receipt["cleanup_confirmed"])
        self.process.wait.assert_called_once()

    def test_live_member_in_other_group_same_session_does_not_authorize_foreign_group_signal(self):
        self.snapshots = [[(190101, os.getpid(), 190101, "Z"), (190102, 190101, 190200, "S")]]
        with self.assertRaises(TimeoutError): self.owner.cleanup()
        self.kill.assert_not_called()
        self.process.wait.assert_not_called()

    def test_reap_exception_keeps_root_and_does_not_signal_again(self):
        self.process.wait.side_effect = TimeoutError("mock final reap failure")
        with self.assertRaises(TimeoutError): self.owner.cleanup()
        self.assertTrue(self.owner.signal_closed)
        self.kill.assert_not_called()
        self.assertFalse(self.owner.receipt["cleanup_confirmed"])
        self.assertTrue(self.owner.root.is_dir())

    def test_unconfirmed_context_retains_root_but_confirmed_failed_check_can_clean(self):
        report = {"status": "failed", "pg_fixture_invoked": True, "fixture_cleanup_confirmed": False}
        with self.gate.owned_temporary_root(report) as retained: pass
        self.addCleanup(shutil.rmtree, retained)
        self.assertTrue(retained.is_dir())
        self.assertTrue(report["cleanup_unconfirmed"])
        report = {"status": "failed", "pg_fixture_invoked": True, "fixture_cleanup_confirmed": True}
        with self.gate.owned_temporary_root(report) as cleaned: pass
        self.assertFalse(cleaned.exists())
        self.assertTrue(report["temporary_cleanup_confirmed"])

    def exercise_lanes(self, mode):
        gate = self.gate
        base = self.root.parent / ("lane-controller-" + mode)
        base.mkdir()
        registry, events = {}, []
        class InertOwner:
            def __init__(self, root, tools, lane, tool):
                self.root, self.lane = root, lane
                self.process = SimpleNamespace(pid=190101 if lane == "sync" else 190102)
                self.receipt = {"cleanup_confirmed": False, "postmaster_exit_code": 0}
                registry[lane] = self
            def start(self, execution_deadline):
                self.execution_deadline = execution_deadline
                events.append("start-" + self.lane)
                return base / (self.lane + "-owner.json")
            def cleanup(self):
                events.append("cleanup-" + self.lane)
                if mode == "cleanup-fails" and self.lane == "sync":
                    raise TimeoutError("mock private group remains")
                self.receipt["cleanup_confirmed"] = True
        def runtime(arguments, repo, working, timeout_seconds):
            self.assertGreater(timeout_seconds, 0)
            self.assertLessEqual(timeout_seconds, 180)
            lane = arguments[arguments.index("--lane") + 1]
            self.assertEqual(arguments[arguments.index("--loops") + 1], "4")
            self.assertEqual(arguments[arguments.index("--profile") + 1], "debug")
            self.assertIn("--database-owner-config", arguments)
            events.append("driver-" + lane)
            raw = {"database_process_owner": "outer installed-archive verifier",
                   "database_owner_pid": registry[lane].process.pid}
            path = Path(arguments[arguments.index("--output") + 1])
            path.write_text(json.dumps({"required_lanes": [lane], "lanes": {
                lane: {"status": "passed", "report": raw}}}))
            if mode in ("driver-fails", "unconfirmed-driver") and lane == "sync":
                failure = TimeoutError("mock original owner timeout")
                failure.process_owner_cleanup = {"process_tree_cleanup_confirmed": mode == "driver-fails"}
                raise failure
            completed = subprocess.CompletedProcess(arguments, 0)
            completed.process_owner_cleanup = {"process_tree_cleanup_confirmed": True}
            return completed
        runner = SimpleNamespace(MAX_CAPTURE_BYTES=0,
            run_bounded=lambda args, repo, label: subprocess.CompletedProcess(args, 0, b"/inert/postgresql\n", b""))
        exporter = SimpleNamespace(plain=lambda path: None, run_runtime_check=runtime)
        prepare = SimpleNamespace(read_file=lambda path, limit: path.read_bytes(), json_object=json.loads, MAX_JSON=1048576)
        report = {}
        with mock.patch.object(gate, "PosixPrivatePostgres", InertOwner), \
             mock.patch.object(gate, "load_trusted", return_value=runner), \
             mock.patch.object(gate.shutil, "which", return_value="/inert/pg_config"):
            if mode == "normal":
                gate.run_pg_lifecycles(exporter, prepare, base, base, base, base / "lifecycle.json", report)
            else:
                with self.assertRaises((TimeoutError, ValueError)):
                    gate.run_pg_lifecycles(exporter, prepare, base, base, base, base / "lifecycle.json", report)
        if set(registry) == {"sync", "async"}:
            self.assertEqual(registry["sync"].execution_deadline, registry["async"].execution_deadline)
        return events, report, json.loads((base / "lifecycle.json").read_bytes())

    def test_runtime_cap_can_only_be_shortened_and_expired_start_never_launches(self):
        for value in (0, -1, 181, float("inf"), float("nan"), True):
            with self.subTest(value=value), self.assertRaises(EXPORT.ExportError):
                EXPORT.run_runtime_check([], self.root, self.root, timeout_seconds=value)
        with mock.patch.object(self.gate.time, "monotonic", return_value=100), \
             mock.patch.object(self.gate.subprocess, "Popen") as launch, self.assertRaises(TimeoutError):
            self.owner.start(execution_deadline=100)
        launch.assert_not_called()

    def test_independent_lanes_preserve_order_parameters_and_cleanup_before_next(self):
        events, report, data = self.exercise_lanes("normal")
        self.assertEqual(events, ["start-sync", "driver-sync", "cleanup-sync", "start-async", "driver-async", "cleanup-async"])
        self.assertTrue(report["fixture_cleanup_confirmed"])
        self.assertEqual(data["status"], "passed")

    def test_failed_lane_still_runs_other_lane_only_after_confirmed_cleanup(self):
        events, report, data = self.exercise_lanes("driver-fails")
        self.assertEqual(events, ["start-sync", "driver-sync", "cleanup-sync", "start-async", "driver-async", "cleanup-async"])
        self.assertTrue(report["fixture_cleanup_confirmed"])
        self.assertEqual(data["status"], "failed")
        self.assertEqual(data["lanes"]["sync"]["status"], "failed")
        self.assertEqual(data["lanes"]["async"]["status"], "passed")

    def test_unconfirmed_runtime_or_database_cleanup_blocks_other_lane(self):
        for mode in ("cleanup-fails", "unconfirmed-driver"):
            with self.subTest(mode=mode):
                events, report, data = self.exercise_lanes(mode)
                self.assertEqual(events, ["start-sync", "driver-sync", "cleanup-sync"])
                self.assertFalse(report["fixture_cleanup_confirmed"])
                self.assertEqual(data["status"], "failed")


class ProductionRCScopeTests(unittest.TestCase):
    """A sample RC receipt must never silently authorize real deployment."""

    @classmethod
    def setUpClass(cls):
        path = Path(__file__).resolve().parents[2] / "deploy/production_rc.py"
        spec = importlib.util.spec_from_file_location("ku_rc_regression", path)
        cls.rc = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(cls.rc)

    def gate(self):
        return {"rc_checks": {key: True for key in ("identity", "install", *self.rc.KINDS)},
                "checks": {key: None for key in (*self.rc.REAL_CHECKS, "canary_actually_deployed")},
                "overall": "in_progress", "new_evidence": [], "scope": {}}

    def test_sample_completion_keeps_real_business_unknown(self):
        gate = self.rc.derive_gate(self.gate())
        self.assertEqual(gate["overall"], "rc_prepared")
        self.assertIsNone(gate["checks"]["business_slo_passed"])
        self.assertIsNone(gate["checks"]["canary_actually_deployed"])
        gate["rc_checks"]["observability"] = None
        self.assertEqual(self.rc.derive_gate(gate)["overall"], "in_progress")

    def test_status_does_not_rewind_an_actual_running_canary(self):
        gate = self.gate()
        gate["checks"]["canary_actually_deployed"] = True
        self.assertEqual(self.rc.derive_gate(gate)["overall"], "canary_observing")
        self.assertIsNone(gate["checks"]["business_slo_passed"])

    def test_interrupted_live_action_cannot_become_prepared_by_resume(self):
        gate = self.gate(); gate["live_action_pending"] = {"action": "canary"}
        self.assertEqual(self.rc.derive_gate(gate)["overall"], "blocked")
        self.assertIn("never replays", gate["next_action"])

    def test_receipt_for_another_candidate_does_not_advance_gate(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary); receipt = root / "receipt.json"
            manifest = {key: "a" * (40 if key == "candidate_commit" else 64)
                        for key in ("candidate_commit", "cli_sha256", "application_sha256",
                                    "config_template_sha256", "archive_sha256")}
            data = {"schema": "ku-production-rc-receipt-v1", "kind": "business",
                    "binding": {**manifest, "application_sha256": "b" * 64}, "outcome": "passed", "observed": {"requests": 1}}
            receipt.write_bytes(self.rc.encoded(data)); gate = self.gate(); gate["rc_checks"]["business"] = None
            args = SimpleNamespace(state=root, receipt=receipt, command="verify", profile=None)
            with mock.patch.object(self.rc, "state", return_value=(root, manifest, gate)), self.assertRaises(self.rc.RCError):
                self.rc.attach(args)
            self.assertIsNone(gate["rc_checks"]["business"])
            self.assertFalse((root / "evidence").exists())

    def test_failed_frozen_candidate_cannot_be_replaced_with_a_good_window(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary); evidence = root / "actual.json"; evidence.write_text('{"result":"passed"}')
            manifest = {key: "a" * (40 if key == "candidate_commit" else 64)
                        for key in ("candidate_commit", "cli_sha256", "application_sha256",
                                    "config_template_sha256", "archive_sha256")}
            receipt = root / "receipt.json"
            receipt.write_bytes(self.rc.encoded({"schema": "ku-production-rc-receipt-v1", "kind": "business",
                "binding": manifest, "outcome": "passed", "observed": {"requests": 1},
                "evidence": [{"path": str(evidence), "sha256": self.rc.digest(evidence)}]}))
            gate = self.gate(); gate["rc_checks"]["business"] = False
            args = SimpleNamespace(state=root, receipt=receipt, command="verify", profile=None)
            with mock.patch.object(self.rc, "state", return_value=(root, manifest, gate)), self.assertRaises(self.rc.RCError):
                self.rc.attach(args)
            self.assertIs(gate["rc_checks"]["business"], False)

    def receipt_fixture(self, root, kind, outcome):
        evidence = root / "actual.json"; evidence.write_text('{"observed_requests":1}')
        manifest = {key: "a" * (40 if key == "candidate_commit" else 64)
                    for key in ("candidate_commit", "cli_sha256", "application_sha256",
                                "config_template_sha256", "archive_sha256")}
        data = {"schema": "ku-production-rc-receipt-v1", "kind": kind, "binding": manifest,
            "outcome": outcome, "observed": {"requests": 1},
            "evidence": [{"path": str(evidence), "sha256": self.rc.digest(evidence)}]}
        receipt = root / "receipt.json"; receipt.write_bytes(self.rc.encoded(data))
        return manifest, data, SimpleNamespace(state=root, receipt=receipt, command="verify", profile=None)

    def test_unknown_receipt_cannot_erase_a_prior_candidate_failure(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            manifest, data, args = self.receipt_fixture(root, "business", "unknown")
            gate = self.gate(); gate["rc_checks"]["business"] = False
            with mock.patch.object(self.rc, "state", return_value=(root, manifest, gate)):
                self.assertEqual(self.rc.attach(args)["overall"], "blocked")
                self.assertIs(gate["rc_checks"]["business"], False)
                data["outcome"] = "passed"; args.receipt.write_bytes(self.rc.encoded(data))
                with self.assertRaises(self.rc.RCError): self.rc.attach(args)

    def test_invalid_target_receipt_does_not_leave_an_orphan_file(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            manifest, data, args = self.receipt_fixture(root, "target_preflight", "unknown")
            with mock.patch.object(self.rc, "state", return_value=(root, manifest, self.gate())), self.assertRaises(self.rc.RCError):
                self.rc.attach(args)
            self.assertFalse((root / "evidence").exists())

    def test_target_outcome_consistency_and_unknown_preserve_prior_failure(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            manifest, data, args = self.receipt_fixture(root, "target_preflight", "unknown")
            args.profile = root / "profile.json"
            args.profile.write_bytes(self.rc.encoded({"application": "fixture", "deployment_target": {"name": "fixture"},
                "business_contract_sha256": "b" * 64, "config_sha256": "c" * 64,
                "rollback_object": {"kind": "same_cli_application"}, "candidate_binding": manifest}))
            data["profile_sha256"] = self.rc.digest(args.profile)
            data["checks"] = {key: True for key in self.rc.REAL_CHECKS}
            args.receipt.write_bytes(self.rc.encoded(data)); gate = self.gate()
            with mock.patch.object(self.rc, "state", return_value=(root, manifest, gate)):
                with self.assertRaises(self.rc.RCError): self.rc.attach(args)
                self.assertFalse((root / "evidence").exists())
                gate["checks"]["target_preflight_passed"] = False
                data["checks"] = {key: None for key in self.rc.REAL_CHECKS}
                args.receipt.write_bytes(self.rc.encoded(data))
                result = self.rc.attach(args)
                self.assertIs(result["checks"]["target_preflight_passed"], False)
                self.assertNotEqual(result["overall"], "ready_for_canary")

    def test_credential_template_and_ambiguous_json_are_rejected(self):
        with self.assertRaises(self.rc.RCError):
            self.rc.no_secrets({"env": {"PGPASSWORD": "unused"}, "conninfo": "password=unused"})
        with self.assertRaises(self.rc.RCError):
            self.rc.no_secrets({"env": {"KU_SERVICE_CONTROL_TOKEN": "unused"}})
        self.rc.no_secrets({"control_token_file": "/private/owned/service.token"})
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary) / "input.json"
            for value in ('{"passed":true,"passed":false}', '{"load":NaN}'):
                path.write_text(value)
                with self.subTest(value=value), self.assertRaises(self.rc.RCError):
                    self.rc.load(path)

    def test_missing_real_target_refuses_canary_before_any_process(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary); path = root / "profile.json"; path.write_text('{"application":null}')
            args = SimpleNamespace(state=root, profile=path, command="canary", config=root / "missing", execution_approval="")
            with mock.patch.object(self.rc, "state", return_value=(root, {}, self.gate())), \
                    mock.patch.object(self.rc.subprocess, "run") as launch, self.assertRaises(self.rc.RCError):
                self.rc.live_action(args)
            launch.assert_not_called()
        with self.assertRaises(self.rc.RCError):
            self.rc.observer_verified({}, {}, {})

    def test_canary_only_accepts_exact_regular_slot_copy(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary).resolve(); source = root / "app/service"; source.parent.mkdir()
            source.write_bytes(b"inert candidate data"); expected = self.rc.digest(source)
            slot = root / "runtime/slots/a/service"; slot.parent.mkdir(parents=True); slot.write_bytes(source.read_bytes())
            self.assertEqual(self.rc.slot_program(root, slot, "a", expected), slot)
            outside = root / "other"; outside.write_bytes(source.read_bytes())
            with self.assertRaises(self.rc.RCError):
                self.rc.slot_program(root, outside, "a", expected)
            slot.unlink(); slot.symlink_to(source)
            with self.assertRaises(self.rc.RCError):
                self.rc.slot_program(root, slot, "a", expected)
            cli = root / "bin/ku"; cli.parent.mkdir(); cli.write_bytes(b"inert compiler fixture")
            manifest = {"cli_sha256": self.rc.digest(cli)}
            self.assertEqual(self.rc.configured_cli({"ku": str(cli)}, root, manifest), cli)
            outside.write_bytes(cli.read_bytes())
            with self.assertRaises(self.rc.RCError):
                self.rc.configured_cli({"ku": str(outside)}, root, manifest)
            cli.write_bytes(b"changed installed CLI")
            with self.assertRaises(self.rc.RCError):
                self.rc.configured_cli({"ku": str(cli)}, root, manifest)


class SystemdRCRenderTests(unittest.TestCase):
    """Render inert fixtures only; no unit or external program is started."""

    @classmethod
    def setUpClass(cls):
        cls.repo = Path(__file__).resolve().parents[2]
        path = cls.repo / "deploy/service/render-systemd.py"
        spec = importlib.util.spec_from_file_location("ku_rc_render_regression", path)
        cls.renderer = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(cls.renderer)

    def fixture(self, temporary):
        root = Path(temporary).resolve() / "installation with spaces"
        root.mkdir()
        config = json.loads((self.repo / "deploy/service/cluster.example.json").read_text())
        ku = root / "bin/ku"; ku.parent.mkdir(); ku.write_bytes(b"inert, never executed\n"); ku.chmod(0o755)
        manager = root / "tools/manage.py"; manager.parent.mkdir()
        manager.write_bytes((self.repo / "deploy/service/manage.py").read_bytes())
        token = root / "runtime/control.token"; token.parent.mkdir(); token.write_text("a" * 64); token.chmod(0o600)
        config.update(ku=str(ku), control_token_file=str(token))
        for slot in config["slots"]:
            program = root / "runtime/slots" / slot["name"] / "service"
            program.parent.mkdir(parents=True); program.write_bytes(b"inert native fixture\n"); program.chmod(0o755)
            slot["program"] = str(program)
            slot["manager_start"] = ["systemctl", "start", f"ku-rc-fixture-{slot['name']}.service"]
            slot["manager_stop"] = ["systemctl", "stop", f"ku-rc-fixture-{slot['name']}.service"]
        path = root / "runtime/cluster.json"; path.write_text(json.dumps(config))
        args = SimpleNamespace(config=path, user="fixture", output=root / "units", unit_prefix="ku-rc-fixture",
            manage=None, template=self.repo / "deploy/production-rc/ku-rc@.service", memory_max_bytes=268435456,
            cpu_quota_percent=200, tasks_max=64, nofile=8192, timeout_stop_seconds=16)
        account = SimpleNamespace(pw_uid=os.getuid(), pw_gid=os.getgid())
        return args, config, account

    def render(self, args, account):
        renderer = self.renderer
        with mock.patch.object(renderer.platform, "system", return_value="Linux"), \
                mock.patch.object(renderer.pwd, "getpwnam", return_value=account), \
                mock.patch.object(renderer.grp, "getgrgid", return_value=SimpleNamespace(gr_name="fixture")), \
                mock.patch.object(renderer.os, "getgrouplist", return_value=[account.pw_gid]), \
                mock.patch.object(renderer.os, "cpu_count", return_value=2):
            return renderer.render(args)

    def test_argument_rejects_directive_or_specifier_injection(self):
        self.assertEqual(self.renderer.argument("/installation with spaces/manage.py"), '"/installation with spaces/manage.py"')
        for value in ("a\nExecStart=/bin/anything", "a%h", "${HOME}", 'a"b', "a\\b", "a\x00b"):
            with self.subTest(value=value), self.assertRaises(self.renderer.RenderError):
                self.renderer.argument(value)

    def test_stock_template_keeps_supervisor_drain_and_bounded_restart(self):
        text = (self.repo / "deploy/service/ku-service@.service").read_text()
        for line in ("KillMode=mixed", "Restart=on-abnormal", "RestartSec=5s", "TimeoutStopSec=16s"):
            self.assertIn(line + "\n", text)

    @unittest.skipUnless(os.name == "posix" and os.getuid() != 0, "fixture uses current non-root Unix ownership")
    def test_render_is_bound_fresh_and_preserves_token_privacy(self):
        with tempfile.TemporaryDirectory(prefix="ku-systemd-render-") as temporary:
            args, config, account = self.fixture(temporary)
            result = self.render(args, account)
            self.assertEqual(result["status"], "rendered_not_installed")
            self.assertEqual(result["config_sha256"], self.renderer.digest(args.config))
            self.assertTrue(all(value is False for value in result["actions_performed"].values()))
            self.assertFalse(result["business_deadlines_modified"])
            for unit in result["units"]:
                text = (args.output / unit["filename"]).read_text()
                self.assertNotRegex(text, self.renderer.MARKER)
                self.assertIn("KillMode=mixed\n", text)
                self.assertIn("TimeoutStopSec=16s\n", text)
                self.assertIn('ExecStart="/usr/bin/python3" "' + str(Path(config["ku"]).parent.parent / "tools/manage.py") + '"', text)
                self.assertNotIn("a" * 64, text)
            self.assertNotIn("a" * 64, (args.output / "render-receipt.json").read_text())
            with self.assertRaises(self.renderer.RenderError):
                self.render(args, account)

    @unittest.skipUnless(os.name == "posix" and os.getuid() != 0, "fixture uses current non-root Unix ownership")
    def test_renderer_refuses_wrong_owner_drain_or_external_hard_domain(self):
        for changed in ("root", "token_mode", "drain", "external_domain", "worker-memory-bytes", "total-memory-bytes", "cgroup-root", "wrong_unit", "missing_manager", "arbitrary_sudo"):
            with self.subTest(changed=changed), tempfile.TemporaryDirectory(prefix="ku-systemd-render-") as temporary:
                args, config, account = self.fixture(temporary)
                if changed == "root": account.pw_uid = 0
                elif changed == "token_mode": Path(config["control_token_file"]).chmod(0o644)
                elif changed == "drain": config["service_options"]["drain_timeout_ms"] = 12000
                elif changed == "external_domain": config["slots"][0]["memory_limit_bytes"] = 1
                elif changed == "wrong_unit": config["slots"][0]["manager_start"] = ["systemctl", "start", "unrelated.service"]
                elif changed == "missing_manager": del config["slots"][0]["manager_stop"]
                elif changed == "arbitrary_sudo": config["slots"][0]["manager_start"] = ["sudo", "--preserve-env", "/usr/bin/systemctl", "start", "ku-rc-fixture-a.service"]
                else: config["service_options"][changed] = 1
                args.config.write_text(json.dumps(config))
                with self.assertRaises(self.renderer.RenderError):
                    self.render(args, account)
                self.assertFalse(args.output.exists())

    @unittest.skipUnless(os.name == "posix" and os.getuid() != 0, "fixture uses current non-root Unix ownership")
    def test_exact_noninteractive_sudo_systemctl_is_supported(self):
        with tempfile.TemporaryDirectory(prefix="ku-systemd-render-") as temporary:
            args, config, account = self.fixture(temporary)
            for slot in config["slots"]:
                for operation in ("start", "stop"):
                    slot["manager_" + operation] = ["sudo", "-n", "/usr/bin/systemctl", operation, f"ku-rc-fixture-{slot['name']}.service"]
            args.config.write_text(json.dumps(config))
            self.assertEqual(self.render(args, account)["status"], "rendered_not_installed")


if __name__ == "__main__":
    unittest.main()
