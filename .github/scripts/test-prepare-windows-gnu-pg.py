#!/usr/bin/env python3
"""Offline PE/import-library inputs; fabricated images never run as programs."""
import importlib.util
from pathlib import Path
import struct
import tempfile
import unittest
from unittest import mock

SPEC = importlib.util.spec_from_file_location('gnu_pg_prepare_test', Path(__file__).with_name('prepare-windows-gnu-pg.py'))
PG = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(PG)


def image(exports=None, imports=(), delayed=()):
    exports = sorted(PG.REQUIRED_API if exports is None else exports)
    data = bytearray(0x1800)
    data[:2] = b'MZ'
    struct.pack_into('<I', data, 60, 0x80)
    data[0x80:0x84] = b'PE\0\0'
    struct.pack_into('<HHIIIHH', data, 0x84, 0x8664, 2, 0, 0, 0, 0xf0, 0x2022)
    optional = 0x98
    struct.pack_into('<H', data, optional, 0x20b)
    struct.pack_into('<Q', data, optional + 24, 0x180000000)
    struct.pack_into('<I', data, optional + 108, 16)
    sections = optional + 0xf0
    for index, (address, raw, size, flags) in enumerate(((0x1000, 0x400, 0x200, 0x60000020), (0x2000, 0x600, 0x1200, 0x40000040))):
        offset = sections + index * 40
        struct.pack_into('<IIII', data, offset + 8, size, address, size, raw)
        struct.pack_into('<I', data, offset + 36, flags)

    def offset(rva):
        return rva - 0x2000 + 0x600

    if exports:
        struct.pack_into('<II', data, optional + 112, 0x2000, 0x500)
        struct.pack_into('<IIIIII', data, offset(0x2000) + 16, 1, len(exports), len(exports), 0x2040, 0x2100, 0x2200)
        cursor = 0x2300
        for index, name in enumerate(exports):
            struct.pack_into('<I', data, offset(0x2040) + index * 4, 0x1000)
            struct.pack_into('<I', data, offset(0x2100) + index * 4, cursor)
            struct.pack_into('<H', data, offset(0x2200) + index * 2, index)
            text = name.encode() + b'\0'
            data[offset(cursor):offset(cursor) + len(text)] = text
            cursor += len(text)
    for index, names, address, width in ((1, imports, 0x2500, 20), (13, delayed, 0x2800, 32)):
        if not names:
            continue
        struct.pack_into('<II', data, optional + 112 + index * 8, address, (len(names) + 1) * width)
        cursor = 0x2600 if index == 1 else 0x2900
        for number, name in enumerate(names):
            base = offset(address + number * width)
            if index == 13:
                struct.pack_into('<II', data, base, 1, cursor)
            else:
                struct.pack_into('<I', data, base + 12, cursor)
            text = name.encode() + b'\0'
            data[offset(cursor):offset(cursor) + len(text)] = text
            cursor += len(text)
    return data


class WindowsGnuPgInputTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(prefix='ku-gnu-pg-input-test-')
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name).resolve()

    def dll(self, name, content):
        path = self.root / name
        path.write_bytes(content)
        return path

    def test_actual_export_ordinals_drive_the_definition_without_msvc_import_reuse(self):
        path = self.dll('libpq.dll', image())
        exports = PG.PeImage(path).exports()
        self.assertEqual({row['name'] for row in exports}, PG.REQUIRED_API)
        self.assertEqual({row['ordinal'] for row in exports}, set(range(1, len(PG.REQUIRED_API) + 1)))
        self.assertFalse(any(row['data'] or row['forwarder'] for row in exports))

    def test_x86_and_truncated_headers_are_rejected(self):
        data = image()
        struct.pack_into('<H', data, 0x84, 0x14c)
        with self.assertRaisesRegex(ValueError, 'x86_64'):
            PG.PeImage(self.dll('x86.dll', data))
        for size in (64, 127, 256, 400):
            with self.subTest(size=size), self.assertRaises((ValueError, UnicodeError)):
                PG.PeImage(self.dll('truncated.dll', image()[:size]))

    def test_missing_library_release_function_never_generates_a_partial_abi(self):
        with self.assertRaisesRegex(ValueError, 'required opaque C ABI'):
            PG.PeImage(self.dll('libpq.dll', image(PG.REQUIRED_API - {'PQfreemem'}))).exports()

    def test_normal_and_delay_imports_are_both_in_the_closure(self):
        parsed = PG.PeImage(self.dll('libpq.dll', image(imports=('libcrypto.dll',), delayed=('libssl.dll',))))
        self.assertEqual(parsed.imports(), ['libcrypto.dll', 'libssl.dll'])

    def test_import_directory_names_cannot_escape_the_selected_provider(self):
        with self.assertRaisesRegex(ValueError, 'plain DLL basename'):
            PG.PeImage(self.dll('libpq.dll', image(imports=('../outside.dll',)))).imports()

    def test_private_dependency_closure_is_exact_and_cycles_are_bounded(self):
        provider, system, runtime = (self.root / name for name in ('provider', 'System32', 'runtime'))
        for directory in (provider, system, runtime):
            directory.mkdir()
        (provider / 'libpq.dll').write_bytes(image(imports=('crypto.dll', 'KERNEL32.dll', 'api-ms-win-test-l1-1-0.dll')))
        (provider / 'crypto.dll').write_bytes(image(exports=[], imports=('libpq.dll',)))
        (provider / 'unrelated.dll').write_bytes(b'must not be copied')
        (system / 'KERNEL32.dll').write_bytes(b'observed system fixture')
        rows, observed = PG.dependency_closure(provider, system, runtime)
        self.assertEqual({row['name'] for row in rows}, {'libpq.dll', 'crypto.dll'})
        self.assertEqual({path.name for path in runtime.iterdir()}, {'libpq.dll', 'crypto.dll'})
        self.assertEqual({row['name'] for row in observed}, {'KERNEL32.dll', 'api-ms-win-test-l1-1-0.dll'})
        self.assertFalse((runtime / 'unrelated.dll').exists())

    def test_missing_dependency_is_not_filled_from_any_path_entry(self):
        provider, system, runtime = (self.root / name for name in ('provider', 'System32', 'runtime'))
        for directory in (provider, system, runtime):
            directory.mkdir()
        (provider / 'libpq.dll').write_bytes(image(imports=('not-present.dll',)))
        with self.assertRaisesRegex(ValueError, 'absent from the selected PostgreSQL'):
            PG.dependency_closure(provider, system, runtime)

    def test_closure_byte_budget_is_enforced_before_copying(self):
        provider, system, runtime = (self.root / name for name in ('provider', 'System32', 'runtime'))
        for directory in (provider, system, runtime):
            directory.mkdir()
        (provider / 'libpq.dll').write_bytes(image())
        with mock.patch.object(PG, 'MAX_CLOSURE_BYTES', 10), self.assertRaisesRegex(ValueError, 'byte budget'):
            PG.dependency_closure(provider, system, runtime)
        self.assertEqual(list(runtime.iterdir()), [])


if __name__ == '__main__':
    unittest.main()
