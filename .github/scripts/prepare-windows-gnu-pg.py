#!/usr/bin/env python3
"""Prepare one private GNU libpq import library and its observed DLL closure.

Uses the runner's existing PostgreSQL DLL and selected MinGW tools. Installs
nothing, modifies neither provider, and never connects to a database. The caller
owns the exact --output directory and removes it after its installed-SDK check.
"""
from __future__ import annotations

import argparse
from contextlib import contextmanager
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import re
import shutil
import struct
import sys
import tempfile

sys.dont_write_bytecode = True
SPEC = importlib.util.spec_from_file_location('gnu_pg_owner', Path(__file__).with_name('verify-native-three-os.py'))
OWNER = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(OWNER)
MAX_DLL = 64 * 1024 * 1024
MAX_CLOSURE_BYTES = 192 * 1024 * 1024
DLL_NAME = re.compile(r'[A-Za-z0-9_+.-]+\.dll', re.I)
SYMBOL_NAME = re.compile(r'[A-Za-z_][A-Za-z_0-9]*')
REQUIRED_API = {'PQlibVersion', 'PQconnectStart', 'PQconnectStartParams', 'PQconnectPoll', 'PQstatus',
                'PQfinish', 'PQmakeEmptyPGresult', 'PQclear', 'PQescapeBytea', 'PQfreemem',
                'PQsendQueryParams', 'PQconsumeInput', 'PQgetResult', 'PQsetSingleRowMode',
                'PQftype', 'PQfformat', 'PQgetvalue', 'PQgetisnull', 'PQgetlength'}


def digest(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


class PeImage:
    """Only the bounded PE32+ fields needed for imports and named exports."""
    def __init__(self, path):
        if path.is_symlink() or not path.is_file() or not 64 <= path.stat().st_size <= MAX_DLL:
            raise ValueError('provider DLL must be a bounded plain existing file: ' + str(path))
        with path.open('rb') as stream:
            self.data = stream.read(MAX_DLL + 1)
        if len(self.data) > MAX_DLL:
            raise ValueError('provider DLL exceeds its read bound')
        if self.data[:2] != b'MZ':
            raise ValueError('provider is not a PE DLL')
        pe = self.u32(60)
        if not 64 <= pe <= 1024 * 1024 or self.data[pe:pe + 4] != b'PE\0\0':
            raise ValueError('invalid PE header')
        machine, count = self.unpack('<HH', pe + 4)
        optional_size = self.u16(pe + 20)
        self.optional = pe + 24
        if machine != 0x8664 or not 1 <= count <= 96 or optional_size < 112 or self.u16(self.optional) != 0x20b or not self.u16(pe + 22) & 0x2000:
            raise ValueError('provider is not x86_64 PE32+')
        self.slice(self.optional, optional_size)
        self.directories = self.u32(self.optional + 108)
        if not 0 <= self.directories <= (optional_size - 112) // 8:
            raise ValueError('PE data-directory count exceeds its optional header')
        self.image_base = self.unpack('<Q', self.optional + 24)[0]
        self.sections = []
        for index in range(count):
            section = self.optional + optional_size + index * 40
            virtual_size, address, raw_size, raw_offset = self.unpack('<IIII', section + 8)
            flags = self.u32(section + 36)
            self.slice(raw_offset, raw_size)
            self.sections.append((address, virtual_size, raw_offset, raw_size, flags))

    def slice(self, offset, count):
        if offset < 0 or count < 0 or offset > len(self.data) - count:
            raise ValueError('PE range exceeds its file')
        return self.data[offset:offset + count]

    def unpack(self, fmt, offset):
        return struct.unpack(fmt, self.slice(offset, struct.calcsize(fmt)))

    def u16(self, offset):
        return self.unpack('<H', offset)[0]

    def u32(self, offset):
        return self.unpack('<I', offset)[0]

    def rva(self, address, count=1):
        for base, _, raw, size, _ in self.sections:
            if address >= base and address - base <= size - count:
                return raw + address - base
        raise ValueError('PE RVA is not backed by file bytes')

    def text(self, address, maximum=512):
        start = self.rva(address)
        end = self.data.find(b'\0', start, min(len(self.data), start + maximum + 1))
        if end < 0:
            raise ValueError('PE name exceeds its bound')
        return self.data[start:end].decode('ascii')

    def directory(self, index):
        return self.unpack('<II', self.optional + 112 + index * 8) if index < self.directories else (0, 0)

    def imports(self):
        result = []
        for index, width, name_offset in ((1, 20, 12), (13, 32, 4)):
            address, size = self.directory(index)
            if not address and not size:
                continue
            if not address or not width <= size <= 65536:
                raise ValueError('PE import table size is invalid')
            for entry in range(min(size // width, 256)):
                offset = self.rva(address + entry * width, width)
                row = self.slice(offset, width)
                if not any(row):
                    break
                name = self.u32(offset + name_offset)
                if index == 13:
                    attributes = self.u32(offset)
                    if attributes & ~1:
                        raise ValueError('unsupported PE delay-import attributes')
                    if not attributes & 1:
                        name -= self.image_base
                name = self.text(name)
                if not DLL_NAME.fullmatch(name):
                    raise ValueError('PE import is not a plain DLL basename')
                if name.casefold() not in {value.casefold() for value in result}:
                    result.append(name)
            else:
                raise ValueError('PE import table is unterminated or exceeds its entry bound')
        return result

    def exports(self):
        address, size = self.directory(0)
        if not address or not 40 <= size <= 1024 * 1024:
            raise ValueError('libpq has no bounded export table')
        offset = self.rva(address, 40)
        base, functions, names, table, pointers, ordinals = self.unpack('<IIIIII', offset + 16)
        if not 0 < names <= 4096 or not names <= functions <= 16384:
            raise ValueError('libpq export count exceeds its bound')
        result = []
        for index in range(names):
            name = self.text(self.u32(self.rva(pointers + index * 4, 4)))
            ordinal = self.u16(self.rva(ordinals + index * 2, 2))
            if not SYMBOL_NAME.fullmatch(name) or ordinal >= functions or base + ordinal > 65535:
                raise ValueError('libpq export cannot be represented by a bounded C import library')
            target = self.u32(self.rva(table + ordinal * 4, 4))
            if not target:
                raise ValueError('named libpq export has no target')
            forwarder = self.text(target) if address <= target < address + size else None
            executable = any(start <= target < start + max(virtual, raw_size) and flags & 0x20000000
                             for start, virtual, _, raw_size, flags in self.sections)
            result.append({'name': name, 'ordinal': base + ordinal, 'data': not executable and forwarder is None,
                           'forwarder': forwarder})
        if len({row['name'] for row in result}) != len(result) or not REQUIRED_API <= {row['name'] for row in result if not row['data']}:
            raise ValueError('libpq export names are duplicated or omit its required opaque C ABI')
        return sorted(result, key=lambda row: row['name'])


@contextmanager
def environment(values):
    previous = dict(os.environ)
    os.environ.clear()
    os.environ.update(values)
    try:
        yield
    finally:
        os.environ.clear()
        os.environ.update(previous)


def execute(command, cwd, env, seconds=30):
    previous = OWNER.COMMAND_TIMEOUT_SECONDS, OWNER.MAX_CAPTURE_BYTES
    OWNER.COMMAND_TIMEOUT_SECONDS, OWNER.MAX_CAPTURE_BYTES = seconds, 65536
    try:
        with environment(env):
            return OWNER.run_bounded([str(value) for value in command], cwd, 'private GNU libpq preparation')
    finally:
        OWNER.COMMAND_TIMEOUT_SECONDS, OWNER.MAX_CAPTURE_BYTES = previous


def dependency_closure(provider, system32, runtime):
    pending, copied, observed, total = ['libpq.dll'], {}, [], 0
    while pending:
        name = pending.pop(0)
        if name.casefold() in copied:
            continue
        if len(copied) >= 32:
            raise ValueError('private libpq DLL closure exceeds 32 files')
        source = provider / name
        if source.is_symlink() or source.resolve().parent != provider:
            raise ValueError('private DLL source escaped the selected PostgreSQL bin directory')
        image = PeImage(source)
        total += len(image.data)
        if total > MAX_CLOSURE_BYTES:
            raise ValueError('private DLL closure exceeds its byte budget')
        destination = runtime / name
        destination.write_bytes(image.data)
        row = {'name': name, 'source': str(source), 'sha256': digest(destination), 'size': len(image.data),
               'imports': image.imports()}
        copied[name.casefold()] = row
        for dependency in row['imports']:
            # API sets are Windows loader contracts, not PATH DLL files.
            if dependency.lower().startswith(('api-ms-win-', 'ext-ms-win-')):
                observed.append({'name': dependency, 'kind': 'Windows API-set contract'})
            elif (provider / dependency).is_file():
                pending.append(dependency)
            elif (system32 / dependency).is_file():
                observed.append({'name': dependency, 'kind': 'observed Windows System32 file',
                                 'path': str(system32 / dependency), 'sha256': digest(system32 / dependency)})
            else:
                raise ValueError('DLL is absent from the selected PostgreSQL install and System32: ' + dependency)
    return list(copied.values()), observed


CANARY = r'''
#include <stddef.h>
#include <stdio.h>
#include <windows.h>
typedef struct pg_conn PGconn;
typedef struct pg_result PGresult;
extern int PQlibVersion(void);
extern PGconn* PQconnectStart(const char*);
extern int PQstatus(const PGconn*);
extern void PQfinish(PGconn*);
extern PGresult* PQmakeEmptyPGresult(PGconn*, int);
extern void PQclear(PGresult*);
extern unsigned char* PQescapeBytea(const unsigned char*, size_t, size_t*);
extern void PQfreemem(void*);
_Static_assert(sizeof(void*) == 8 && sizeof(size_t) == 8 && sizeof(int) == 4, "x64 opaque C ABI");
int main(void) {
  PGconn* connection = PQconnectStart("ku_private_invalid_connection_option=1");
  if (!connection) return 2;
  if (PQstatus(connection) != 1) { PQfinish(connection); return 6; }
  PGresult* result = PQmakeEmptyPGresult(connection, 2);
  if (!result) { PQfinish(connection); return 3; }
  size_t length = 0;
  unsigned char* escaped = PQescapeBytea((const unsigned char*)"x\0", 2, &length);
  if (!escaped || length < 2 || length > 1024) {
    if (escaped) PQfreemem(escaped);
    PQclear(result); PQfinish(connection); return 4;
  }
  PQfreemem(escaped); PQclear(result); PQfinish(connection);
  printf("libpq-opaque-abi-ok:%d:8:8:4\n", PQlibVersion());
  char path[32768]; DWORD size = GetModuleFileNameA(GetModuleHandleA("libpq.dll"), path, sizeof(path));
  if (!size || size >= sizeof(path)) return 5;
  printf("libpq-loaded-path:%s\n", path);
  return 0;
}
'''


def prepare(compiler, postgres_root, output):
    if os.name != 'nt':
        raise ValueError('GNU libpq preparation requires an actual Windows runner')
    compiler, postgres_root = compiler.resolve(strict=True), postgres_root.resolve(strict=True)
    provider = (postgres_root / 'bin').resolve(strict=True)
    system = Path(os.environ['SystemRoot']).resolve(strict=True)
    if output.exists() or output.is_symlink():
        raise ValueError('refusing to replace an existing private libpq output')
    if not str(output).isascii():
        raise ValueError('GNU preparation tools require an explicitly owned ASCII temporary output root')
    output.mkdir(mode=0o700)
    runtime = output / 'runtime'
    runtime.mkdir()
    tool = next((compiler.parent / name for name in ('dlltool.exe', 'x86_64-w64-mingw32-dlltool.exe')
                 if (compiler.parent / name).is_file()), None)
    if not tool:
        raise ValueError('selected MinGW directory has no existing dlltool')
    env = dict(os.environ)
    machine = execute([compiler, '-dumpmachine'], output, env, 15).stdout.decode().strip()
    if machine != 'x86_64-w64-mingw32':
        raise ValueError('selected compiler is not the supported x64 MinGW target')
    image = PeImage(provider / 'libpq.dll')
    exports = image.exports()
    if any(row['forwarder'] for row in exports):
        raise ValueError('libpq forwarded exports require an explicit dependency contract; refusing an incomplete closure')
    dlls, system_dependencies = dependency_closure(provider, system / 'System32', runtime)
    provider_sha256 = hashlib.sha256(image.data).hexdigest()
    if next(row['sha256'] for row in dlls if row['name'].casefold() == 'libpq.dll') != provider_sha256:
        raise ValueError('libpq changed between export inspection and private copy')
    for name in ('empty-pgpass.conf', 'empty-pg-service.conf'):
        (output / name).write_bytes(b'')
    (output / 'empty-pg-config').mkdir()
    definition = output / 'libpq.exports.def'
    definition.write_text('LIBRARY libpq.dll\nEXPORTS\n' + ''.join(
        f"  {row['name']} @{row['ordinal']}" + (' DATA' if row['data'] else '') + '\n' for row in exports), encoding='ascii')
    archive = output / 'libpq.dll.a'
    with tempfile.TemporaryDirectory(prefix='tools-', dir=output) as temporary:
        work = Path(temporary)
        child_env = dict(env, TMP=str(work), TEMP=str(work), TMPDIR=str(work))
        execute([tool, '-m', 'i386:x86-64', '-D', 'libpq.dll', '-d', definition.name, '-l', archive.name], output, child_env)
        (work / 'abi.c').write_text(CANARY, encoding='ascii')
        execute([compiler, '-std=c11', '-O2', 'abi.c', archive, '-static', '-o', 'abi.exe'], work, child_env)
        runtime_env = {key: value for key, value in env.items() if not key.upper().startswith('PG')}
        runtime_env.update(PATH=os.pathsep.join((str(runtime), str(system / 'System32'), str(system))),
                           PGPASSFILE=str(output / 'empty-pgpass.conf'),
                           PGSERVICEFILE=str(output / 'empty-pg-service.conf'), PGSYSCONFDIR=str(output / 'empty-pg-config'))
        result = execute([work / 'abi.exe'], work, runtime_env, 15)
        lines = result.stdout.decode('ascii').replace('\r\n', '\n').strip().splitlines()
        if (len(lines) != 2 or not re.fullmatch(r'libpq-opaque-abi-ok:[1-9][0-9]*:8:8:4', lines[0])
                or not lines[1].startswith('libpq-loaded-path:')
                or Path(lines[1].split(':', 1)[1]).resolve() != runtime / 'libpq.dll'):
            raise ValueError('libpq opaque C allocation/free ABI canary returned an unexpected result')
        receipt = lines[0]
    manifest = {'format': 'ku-private-windows-gnu-pg-v1', 'target': 'x86_64-pc-windows-gnu',
                'provider': str(postgres_root), 'tools': str(provider), 'compiler': str(compiler), 'machine': machine,
                'dlltool': str(tool), 'import_library': {'name': archive.name, 'size': archive.stat().st_size,
                    'sha256': digest(archive), 'provider_sha256': provider_sha256},
                'exports': exports, 'dlls': dlls, 'system_dependencies': system_dependencies,
                'opaque_abi': receipt, 'tool_work_removed': not work.exists(),
                'loaded_libpq': lines[1].split(':', 1)[1],
                'connection_defaults': {'PGPASSFILE': 'empty-pgpass.conf', 'PGSERVICEFILE': 'empty-pg-service.conf',
                                        'PGSYSCONFDIR': 'empty-pg-config'},
                'runtime_search_path': 'private DLL directory + Windows System32/SystemRoot only',
                'scope': 'installed-SDK function validation; not a new official release target or performance result'}
    (output / 'manifest.json').write_text(json.dumps(manifest, ensure_ascii=False, indent=2) + '\n', encoding='utf-8')
    return manifest


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--compiler', required=True, type=Path)
    parser.add_argument('--postgres-root', required=True, type=Path)
    parser.add_argument('--output', required=True, type=Path)
    args = parser.parse_args()
    manifest = prepare(args.compiler, args.postgres_root, args.output.resolve())
    print(json.dumps({key: manifest[key] for key in ('format', 'target', 'machine', 'opaque_abi', 'tool_work_removed')}, ensure_ascii=False))


if __name__ == '__main__':
    main()
