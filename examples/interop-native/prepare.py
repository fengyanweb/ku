#!/usr/bin/env python3
"""Build a portable author/consumer fixture from pinned upstream sources."""

import argparse
import hashlib
import io
from pathlib import Path
import shutil
from urllib.request import urlopen
import zipfile


SQLITE = (
    "https://sqlite.org/2025/sqlite-amalgamation-3500400.zip",
    "1d3049dd0f830a025a53105fc79fd2ab9431aea99e137809d064d8ee8356b032",
    2_845_716,
)
TINYXML_URL = "https://raw.githubusercontent.com/leethomason/tinyxml2/9148bdf719e997d1f474be6bcc7943881046dba1/"
TINYXML = {
    "tinyxml2.h": ("0f3c6e5a91b6d65caca3cc7239926455f21e488cff80ac3d27c74f5262fd2602", 74_466),
    "tinyxml2.cpp": ("95f64e4cb06c9e147d5b12e8860c88898944858dfbc38fed92ebf918f1436554", 78_553),
    "LICENSE.txt": ("9332252e9b9e46db8285d4a3f0bf25f139bf1dca6781b956d57f2302efca6432", 808),
}

SQLITE_DESCRIPTOR = """kind = "native"
language = "c"
module = "database"
header = "sqlite_adapter.h"
source.1 = "sqlite3.c"
source.2 = "sqlite_adapter.c"
system.1 = "dl"
system.2 = "m"
system.3 = "pthread"
export.Version = "sqlite3_libversion_number"
export.Query = "example_query"
borrow.Query = "cstr"
return.Query = "owned_cstr:example_release"
"""

SQLITE_MODULE = """fn Version(): int! {
    return __ku_interop_int("self", "Version")
}

fn Query(&sql: str, &input: str): str! {
    return __ku_interop_str("self", "Query", sql, input)
}
"""

TINYXML_DESCRIPTOR = """kind = "native"
language = "cpp"
module = "document"
header = "interop_document.h"
source.1 = "tinyxml2.cpp"
class = "ExampleDocument"
constructor = "New"
destructor = "Close"
method.Parse = "Parse(cstr):int"
method.RootName = "RootElement()->Name():cstr"
method.RaiseExampleError = "RaiseExampleError():null"
"""

TINYXML_MODULE = """fn New(): int! {
    return __ku_interop_int("self", "New")
}

fn Close(document: int): null! {
    return __ku_interop_null("self", "Close", document)
}

fn Parse(document: int, &text: str): int! {
    return __ku_interop_int("self", "Parse", document, text)
}

fn RootName(document: int): str! {
    return __ku_interop_str("self", "RootName", document)
}

fn RaiseExampleError(document: int): null! {
    return __ku_interop_null("self", "RaiseExampleError", document)
}
"""


def fetch(url, digest, size):
    with urlopen(url, timeout=60) as response:
        data = response.read(size + 1)
    if len(data) != size or hashlib.sha256(data).hexdigest() != digest:
        raise ValueError(f"Upstream archive size or SHA-256 does not match {url}")
    return data


def package(destination, name, version, module, descriptor, source):
    root = destination / "vendor" / name
    files = root / "src"
    files.mkdir(parents=True)
    (root / "ku.mod").write_text(
        f'name = "{name}"\nversion = "{version}"\nroot = "src"\ninterop.version = "1"\n',
        encoding="utf-8",
    )
    (files / "ku.interop").write_text(descriptor, encoding="utf-8")
    (files / f"{module}.ku").write_text(source, encoding="utf-8")
    return files


def prepare(destination):
    vendor = Path(__file__).resolve().parent
    sqlite_zip = fetch(*SQLITE)
    tinyxml = {
        name: fetch(TINYXML_URL + name, digest, size)
        for name, (digest, size) in TINYXML.items()
    }

    destination.mkdir()
    source = destination / "src"
    source.mkdir()
    shutil.copyfile(vendor / "main.ku", source / "main.ku")
    (destination / "ku.mod").write_text(
        'name = "interop_consumer"\nversion = "0.1.0"\nroot = "src"\nmain = "main.ku"\n',
        encoding="utf-8",
    )

    sqlite = package(destination, "sqlite_example", "3.50.4", "database", SQLITE_DESCRIPTOR, SQLITE_MODULE)
    with zipfile.ZipFile(io.BytesIO(sqlite_zip)) as archive:
        for name in ("sqlite3.h", "sqlite3.c"):
            (sqlite / name).write_bytes(archive.read(f"sqlite-amalgamation-3500400/{name}"))
    for name in ("sqlite_adapter.h", "sqlite_adapter.c"):
        shutil.copyfile(vendor / name, sqlite / name)
    (sqlite.parent / "LICENSE").write_text(
        "SQLite is in the public domain. See https://www.sqlite.org/copyright.html.\n",
        encoding="utf-8",
    )

    xml = package(destination, "tinyxml_example", "11.0.0", "document", TINYXML_DESCRIPTOR, TINYXML_MODULE)
    for name, content in tinyxml.items():
        path = xml.parent / "LICENSE" if name == "LICENSE.txt" else xml / name
        path.write_bytes(content)
    shutil.copyfile(vendor / "interop_document.h", xml / "interop_document.h")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("destination", type=Path, help="a new directory for the example")
    args = parser.parse_args()
    prepare(args.destination)
