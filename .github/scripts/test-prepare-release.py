#!/usr/bin/env python3
"""Offline final-release contracts; fixtures are data, never executable targets."""

import hashlib
import importlib.util
import io
import json
import os
from pathlib import Path
from types import SimpleNamespace
import sys
import tarfile
import tempfile
import unittest
from unittest import mock

sys.dont_write_bytecode = True
SPEC = importlib.util.spec_from_file_location(
    "ku_prepare_release", Path(__file__).with_name("prepare-release.py"))
PREPARE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(PREPARE)

VERSION = "0.0.17"
TAG = f"v{VERSION}"
COMMIT = "a" * 40
ENV = {"GITHUB_SERVER_URL": "https://github.com", "GITHUB_REPOSITORY": "example/ku",
       "GITHUB_RUN_ID": "123456"}


def encoded(value):
    return (json.dumps(value, sort_keys=True) + "\n").encode()


class PrepareReleaseTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(prefix="ku-prepare-test-")
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name).resolve()
        self.repo = self.root / "repo"
        self.assets = self.root / "assets"
        self.repo.mkdir()
        self.assets.mkdir()
        (self.repo / "docs").mkdir()
        (self.repo / "docs" / f"v{VERSION}.md").write_text("# Ku 0.0.17\n\n实验版本。\n", encoding="utf-8")
        (self.repo / "Cargo.toml").write_text(f'[package]\nname = "ku"\nversion = "{VERSION}"\n', encoding="utf-8")
        (self.repo / "Cargo.lock").write_text(f'[[package]]\nname = "ku"\nversion = "{VERSION}"\n', encoding="utf-8")
        extension = self.repo / "editors" / "vscode-ku"
        extension.mkdir(parents=True)
        (extension / "package.json").write_bytes(encoded({"version": VERSION, "repository": {"url": "https://github.com/example/ku.git"}}))
        (extension / "package-lock.json").write_bytes(encoded({"version": VERSION, "packages": {"": {"version": VERSION}}}))
        for target in PREPARE.TARGETS:
            self.archive(target)
        self.environment = mock.patch.dict(os.environ, ENV)
        self.environment.start()
        self.addCleanup(self.environment.stop)

    def paths(self, target=None):
        target = target or PREPARE.TARGETS[0]
        asset = self.assets / f"ku-{TAG}-{target}.tar.gz"
        return asset, self.assets / f"{asset.name}.manifest.json"

    def archive(self, target, release=None, extra=None):
        asset, manifest = self.paths(target)
        payloads = {"RELEASE.json": encoded(release or {"version": VERSION, "target": target, "commit": COMMIT}),
                    "ku": b"fake target data, not executed\n", "deps/libprobe.rlib": b"fake library"}
        if extra:
            payloads.update(extra)
        specs = {}
        with tarfile.open(asset, "w:gz") as archive:
            for name, content in payloads.items():
                member = tarfile.TarInfo(name)
                member.size, member.mode = len(content), 0o644
                archive.addfile(member, io.BytesIO(content))
                specs[name] = {"size": len(content), "mode": 0o644,
                               "sha256": hashlib.sha256(content).hexdigest()}
        manifest.write_bytes(encoded({"version": VERSION, "target": target, "commit": COMMIT,
                                      "asset": asset.name, "sha256": hashlib.sha256(asset.read_bytes()).hexdigest(),
                                      "size": asset.stat().st_size, "files": specs}))

    def change_manifest(self, update):
        _, path = self.paths()
        manifest = json.loads(path.read_bytes())
        update(manifest)
        path.write_bytes(encoded(manifest))

    def prepare(self, **changes):
        args = {"assets": self.assets, "tag": TAG, "commit": COMMIT, "repo": self.repo}
        args.update(changes)
        return PREPARE.prepare_release(**args)

    def rejected(self, **changes):
        with self.assertRaises((PREPARE.PrepareError, PREPARE.EXPORTER.ExportError, OSError,
                                ValueError, KeyError, TypeError, tarfile.TarError, EOFError)):
            self.prepare(**changes)
        self.assertFalse((self.assets / "SHA256SUMS").exists())
        self.assertFalse((self.assets / "RELEASE-NOTES.md").exists())

    def test_three_assets_generate_exact_checksums_and_original_notes_with_ci(self):
        original = (self.repo / "docs" / f"v{VERSION}.md").read_bytes()
        result = self.prepare()
        self.assertEqual(result["targets"], list(PREPARE.TARGETS))
        lines = (self.assets / "SHA256SUMS").read_text().splitlines()
        self.assertEqual(len(lines), 6)
        for line in lines:
            digest, name = line.split("  ")
            self.assertEqual(digest, hashlib.sha256((self.assets / name).read_bytes()).hexdigest())
        notes = (self.assets / "RELEASE-NOTES.md").read_text(encoding="utf-8")
        self.assertIn("实验版本", notes)
        self.assertIn(COMMIT, notes)
        self.assertIn("https://github.com/example/ku/actions/runs/123456", notes)
        self.assertEqual((self.repo / "docs" / f"v{VERSION}.md").read_bytes(), original)
        with self.assertRaises(PREPARE.PrepareError):
            self.prepare()

    def test_missing_archive_and_manifest_are_rejected(self):
        for index in (0, 1):
            with self.subTest(index=index):
                path = self.paths()[index]
                original = path.read_bytes()
                path.unlink()
                self.rejected()
                path.write_bytes(original)

    def test_extra_hidden_file_directory_and_preexisting_report_are_rejected(self):
        for name in ("unexpected.txt", ".hidden", "SHA256SUMS", "RELEASE-NOTES.md"):
            with self.subTest(name=name):
                path = self.assets / name
                path.write_bytes(b"must remain unchanged")
                with self.assertRaises(PREPARE.PrepareError):
                    self.prepare()
                self.assertEqual(path.read_bytes(), b"must remain unchanged")
                path.unlink()
        directory = self.assets / "extra"
        directory.mkdir()
        self.rejected()

    def test_manifest_identity_fields_must_match(self):
        for key, value in (("version", "0.0.16"), ("target", "other"), ("commit", "b" * 40),
                           ("asset", "../other.tar.gz"), ("sha256", "0" * 64), ("size", 1)):
            with self.subTest(key=key):
                self.change_manifest(lambda item: item.update({key: value}))
                self.rejected()
                self.archive(PREPARE.TARGETS[0])

    def test_tag_commit_and_all_repository_versions_must_match(self):
        for value in ("0.0.17", "v0.00.17", "v0.0.17/../x", "v0.0.18", "v0.0.17\n"):
            with self.subTest(tag=value):
                self.rejected(tag=value)
        for value in ("a" * 39, "A" * 40, "a" * 40 + "\n"):
            self.rejected(commit=value)
        for path in (self.repo / "Cargo.toml", self.repo / "Cargo.lock",
                     self.repo / "editors/vscode-ku/package.json",
                     self.repo / "editors/vscode-ku/package-lock.json"):
            with self.subTest(path=path.name):
                original = path.read_bytes()
                path.write_bytes(original.replace(b"0.0.17", b"0.0.16"))
                self.rejected()
                path.write_bytes(original)

    def test_single_leading_bom_in_cargo_files_is_accepted_without_rewriting(self):
        originals = {}
        for name in ("Cargo.toml", "Cargo.lock"):
            path = self.repo / name
            originals[path] = b"\xef\xbb\xbf" + path.read_bytes()
            path.write_bytes(originals[path])
        self.assertEqual(self.prepare()["version"], VERSION)
        for path, original in originals.items():
            self.assertEqual(path.read_bytes(), original)

    def test_repeated_leading_bom_in_cargo_files_is_rejected(self):
        for name in ("Cargo.toml", "Cargo.lock"):
            with self.subTest(name=name):
                path = self.repo / name
                original = path.read_bytes()
                path.write_bytes(b"\xef\xbb\xbf" * 2 + original)
                with self.assertRaises(PREPARE.tomllib.TOMLDecodeError):
                    PREPARE.repo_version(self.repo, VERSION)
                self.rejected()
                path.write_bytes(original)

    def test_bom_between_cargo_statements_is_rejected(self):
        for name in ("Cargo.toml", "Cargo.lock"):
            with self.subTest(name=name):
                path = self.repo / name
                original = path.read_bytes()
                path.write_bytes(original.replace(b"\n", b"\n\xef\xbb\xbf", 1))
                with self.assertRaises(PREPARE.tomllib.TOMLDecodeError):
                    PREPARE.repo_version(self.repo, VERSION)
                self.rejected()
                path.write_bytes(original)

    def test_embedded_release_identity_is_independently_verified(self):
        for key in ("version", "target", "commit"):
            release = {"version": VERSION, "target": PREPARE.TARGETS[0], "commit": COMMIT}
            release[key] = "wrong"
            self.archive(PREPARE.TARGETS[0], release=release)
            self.rejected()

    def test_corrupt_archive_is_rejected_even_with_matching_compressed_hash(self):
        asset, _ = self.paths()
        asset.write_bytes(b"not a gzip or tar archive")
        self.change_manifest(lambda item: item.update({"sha256": hashlib.sha256(asset.read_bytes()).hexdigest(),
                                                       "size": asset.stat().st_size}))
        self.rejected()

    def test_member_hash_mode_size_and_path_contracts_are_enforced(self):
        for spec in ({"sha256": "0" * 64}, {"mode": 0o777}, {"size": True}, {"size": -1}):
            with self.subTest(spec=spec):
                self.change_manifest(lambda item: item["files"]["ku"].update(spec))
                self.rejected()
                self.archive(PREPARE.TARGETS[0])
        self.change_manifest(lambda item: item["files"].update({"../escape": item["files"]["ku"]}))
        self.rejected()

    def test_duplicate_json_and_case_collisions_are_rejected(self):
        _, path = self.paths()
        original = path.read_bytes()
        path.write_bytes(original.replace(b'{"asset":', b'{"version":"0.0.17","asset":', 1))
        self.rejected()
        path.write_bytes(original)
        self.change_manifest(lambda item: item["files"].update({"KU": item["files"]["ku"]}))
        self.rejected()

    def test_input_size_and_expanded_size_limits_reject_before_extraction(self):
        with mock.patch.object(PREPARE, "MAX_BYTES", 10):
            self.rejected()
        self.change_manifest(lambda item: item["files"]["ku"].update({"size": PREPARE.MAX_BYTES + 1}))
        self.rejected()

    def test_symlink_assets_and_external_parent_alias_are_rejected(self):
        asset, _ = self.paths()
        saved = self.root / "real.tar.gz"
        asset.rename(saved)
        try:
            asset.symlink_to(saved)
        except OSError as error:
            saved.rename(asset)
            self.skipTest(f"symlink creation unavailable (winerror={getattr(error, 'winerror', None)})")
        self.rejected()
        asset.unlink()
        saved.rename(asset)
        alias = self.root / "alias"
        alias.symlink_to(self.assets, target_is_directory=True)
        self.rejected(assets=alias)

    def test_tar_symlink_is_rejected_without_extracting_it(self):
        asset, _ = self.paths()
        with tarfile.open(asset, "w:gz") as archive:
            member = tarfile.TarInfo("ku")
            member.type, member.linkname = tarfile.SYMTYPE, "../outside"
            archive.addfile(member)
        self.change_manifest(lambda item: item.update({"sha256": hashlib.sha256(asset.read_bytes()).hexdigest(),
                                                       "size": asset.stat().st_size}))
        self.rejected()
        self.assertFalse((self.root / "outside").exists())

    def test_windows_reparse_attribute_is_rejected_without_symlink_privilege(self):
        original_lstat = Path.lstat
        for forbidden in (self.assets, self.paths()[0], self.repo):
            def replaced_lstat(path, *, follow_symlinks=False):
                info = original_lstat(path)
                if path == forbidden:
                    return SimpleNamespace(st_mode=info.st_mode, st_file_attributes=0x400)
                return info

            with self.subTest(path=forbidden.name), mock.patch.object(Path, "lstat", replaced_lstat):
                self.rejected()

    def test_inputs_changed_during_archive_verification_reject_all_reports(self):
        original_verify = PREPARE.EXPORTER.verify_archive
        _, manifest_path = self.paths()

        def verify_then_change(*args):
            original_verify(*args)
            with manifest_path.open("ab") as output:
                output.write(b" ")

        with mock.patch.object(PREPARE.EXPORTER, "verify_archive", verify_then_change):
            self.rejected()

    def test_final_target_failure_does_not_leave_partial_reports(self):
        target = PREPARE.TARGETS[-1]
        self.archive(target, release={"version": VERSION, "target": target, "commit": "b" * 40})
        self.rejected()

    def test_missing_release_metadata_unknown_fields_and_boolean_sizes_are_rejected(self):
        for update in (lambda item: item["files"].pop("RELEASE.json"),
                       lambda item: item.update({"unknown": "field"}),
                       lambda item: item.update({"size": True}),
                       lambda item: item.update({"files": []})):
            self.change_manifest(update)
            self.rejected()
            self.archive(PREPARE.TARGETS[0])

    def test_github_identity_cannot_inject_hosts_paths_or_markdown(self):
        for key, values in {"GITHUB_SERVER_URL": ("http://github.com", "https://github.com.evil", "https://github.com/@x"),
                            "GITHUB_REPOSITORY": ("other/ku", "example/../ku", "example/ku?x", "example/ku\n"),
                            "GITHUB_RUN_ID": ("", "0", "../123", "123\n")}.items():
            for value in values:
                with self.subTest(key=key, value=value), mock.patch.dict(os.environ, {key: value}):
                    self.rejected()

    def test_exclusive_report_creation_preserves_existing_report(self):
        notes = self.assets / "RELEASE-NOTES.md"
        notes.write_bytes(b"another writer")
        with self.assertRaises(FileExistsError):
            PREPARE.create_reports(self.assets, b"hashes", b"replacement")
        self.assertFalse((self.assets / "SHA256SUMS").exists())
        self.assertEqual(notes.read_bytes(), b"another writer")


if __name__ == "__main__":
    unittest.main()
