#!/usr/bin/env python3
"""Bounded release archive and attribution regression tests (no network)."""

from __future__ import annotations

import copy
import gzip
import hashlib
import importlib.util
import io
import json
import os
from pathlib import Path
import sys
import tarfile
import tempfile
import unittest
from unittest import mock

sys.dont_write_bytecode = True
SPEC = importlib.util.spec_from_file_location("ku_export", Path(__file__).with_name("export-release.py"))
assert SPEC and SPEC.loader
EXPORT = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(EXPORT)


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
        self.package = self.root / "cache" / "sample-1.2.3"
        self.put(self.package / "Cargo.toml", b"fixture manifest\n")
        self.put(self.package / "LICENSE-MIT", b"original license bytes\r\nCopyright sample\r\n")
        self.put(self.package / "NOTICE", b"original NOTICE bytes\n")
        self.put(self.repo / "Cargo.toml", b"workspace manifest\n")
        self.data = {
            "workspace_members": ["ku"],
            "packages": [
                {"name": "ku", "version": "0.0.17", "id": "ku", "manifest_path": str(self.repo / "Cargo.toml"), "source": None, "license": None},
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
        if os.name == "nt" and arguments["target"] != "x86_64-pc-windows-msvc":
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

    def test_roundtrip_hashes_original_licenses_and_file_set(self):
        result = self.run_export()
        archive = Path(result["asset_path"])
        manifest = json.loads(Path(result["manifest_path"]).read_text(encoding="utf-8"))
        self.assertEqual(archive.name, f"ku-v0.0.17-{self.target}.tar.gz")
        self.assertEqual(manifest["asset"], archive.name)
        self.assertEqual(manifest["commit"], "a" * 40)
        self.assertEqual(manifest["size"], archive.stat().st_size)
        self.assertEqual(result["sha256"], hashlib.sha256(archive.read_bytes()).hexdigest())
        self.assertEqual(manifest["files"]["ku"]["mode"], 0o755)
        extracted = self.root / "extract"
        extracted.mkdir()
        EXPORT.verify_archive(archive, manifest["files"], extracted)
        license_path = extracted / "THIRD_PARTY/cargo/sample-1.2.3/LICENSE-MIT"
        self.assertEqual(license_path.read_bytes(), (self.package / "LICENSE-MIT").read_bytes())
        third_party = json.loads((extracted / "THIRD_PARTY.json").read_text(encoding="utf-8"))
        self.assertEqual([package["name"] for package in third_party["cargo"]], ["sample"])
        self.assertNotIn(str(self.root), json.dumps(third_party))
        sums = (extracted / "SHA256SUMS").read_text(encoding="utf-8").splitlines()
        self.assertEqual(len(sums), len(manifest["files"]) - 1)
        for line in sums:
            digest, name = line.split("  ", 1)
            self.assertEqual(digest, hashlib.sha256((extracted / name).read_bytes()).hexdigest())
        self.assertIn("UNLICENSED", (extracted / "README-INSTALL.md").read_text(encoding="utf-8"))

    def test_windows_bundle_and_asset_names(self):
        (self.bundle / "ku").rename(self.bundle / "ku.exe")
        previous = self.bundle / f"native-tls/v1/{self.target}"
        self.target = "x86_64-pc-windows-msvc"
        current = self.bundle / f"native-tls/v1/{self.target}"
        previous.rename(current)
        (current / "lib/libku_native_tls.a").rename(current / "lib/ku_native_tls.lib")
        self.put(self.bundle / "ku.pdb", b"debug symbols")
        self.put(self.bundle / "deps/sample_derive-0123456789abcdef.dll", b"Cargo emitted Windows procedural macro")
        result = self.run_export()
        self.assertIn(self.target, result["manifest_path"])

    def test_target_specific_proc_macro_names(self):
        positive = {
            "x86_64-pc-windows-msvc": "deps/sample_derive-0123456789abcdef.dll",
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


if __name__ == "__main__":
    unittest.main()
