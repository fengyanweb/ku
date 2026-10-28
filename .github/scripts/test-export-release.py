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
import shutil
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
