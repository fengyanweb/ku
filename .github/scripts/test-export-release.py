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
import time
from types import SimpleNamespace
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
                EXPORT, "verify_runtime_installation", return_value={"status": "passed"}) as runtime:
            result = self.run_export(verify_install=True)
        self.assertEqual(check.call_count, 1)
        runtime.assert_called_once_with(check.call_args.args[0], self.repo, self.target)
        self.assertEqual(result["runtime_installation"], {"status": "passed"})
        manifest = json.loads(Path(result["manifest_path"]).read_bytes())
        self.assertEqual(manifest["installation_check"], {"format": "test-receipt", "target": self.target})
        self.assertNotIn("runtime_installation", manifest)
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


if __name__ == "__main__":
    unittest.main()
