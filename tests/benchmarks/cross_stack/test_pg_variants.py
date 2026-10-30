"""Identity/mode guards for PG comparisons; no VM or database is started."""
import json
import os
import subprocess
from pathlib import Path
import tempfile
import unittest

from pg_variants import derive, digest
from build_linux import expand_ku_server


class PGVariantManifestTest(unittest.TestCase):
    def fixture(self, root):
        manifest = {"source_root": str(root), "build_complete": True,
                    "stacks": {"ku": {"command": ["unchanged-ku"]}}}
        for stack, filename in (("go", "pg_mode.go"), ("rust", "src/pg_mode.rs")):
            source = root / "tests/benchmarks/cross_stack" / stack / filename
            source.parent.mkdir(parents=True)
            source.write_text("fixture source identity\n")
            binary = root / (stack + "-fixture-binary")
            binary.write_bytes(b"not executed\n")
            manifest["stacks"][stack] = {"command": [str(binary)], "env": {}, "build": {
                "binary_sha256": digest(binary), "pg_modes": ["normal", "reset-equivalent"],
                "source_files_sha256": {filename: digest(source)}}}
        path = root / "manifest.json"
        path.write_text(json.dumps(manifest))
        return path

    def test_modes_are_explicit_and_original_is_unchanged(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            original = self.fixture(root)
            before = original.read_bytes()
            for mode, table in (("normal", "A"), ("reset-equivalent", "B")):
                output = derive(original, root / mode, mode)
                result = json.loads(output.read_text())
                for stack in ("go", "rust"):
                    self.assertEqual(result["stacks"][stack]["env"]["BENCH_PG_MODE"], mode)
                self.assertEqual(result["stacks"]["ku"], json.loads(before)["stacks"]["ku"])
                self.assertEqual(result["pg_comparison"]["table"], table)
                self.assertFalse(result["build_complete"])
            self.assertEqual(original.read_bytes(), before)
            with self.assertRaises(ValueError):
                derive(original, root / "normal", "normal")

    def test_changed_source_or_binary_cannot_relabel_prior_build(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            original = self.fixture(root)
            source = root / "tests/benchmarks/cross_stack/go/pg_mode.go"
            source.write_text("changed")
            with self.assertRaisesRegex(ValueError, "inventory changed"):
                derive(original, root / "out", "reset-equivalent")
            self.assertFalse((root / "out").exists())
            source.write_text("fixture source identity\n")
            (root / "rust-fixture-binary").write_bytes(b"changed")
            with self.assertRaisesRegex(ValueError, "binary identity mismatch"):
                derive(original, root / "out", "normal")

    def test_old_binary_and_unknown_mode_are_rejected(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            original = self.fixture(root)
            manifest = json.loads(original.read_text())
            del manifest["stacks"]["go"]["build"]["pg_modes"]
            original.write_text(json.dumps(manifest))
            with self.assertRaisesRegex(ValueError, "no attested"):
                derive(original, root / "out", "reset-equivalent")
            with self.assertRaises(ValueError):
                derive(original, root / "out", "reset")


class KuPGExecutionVariantTest(unittest.TestCase):
    def setUp(self):
        self.source = (Path(__file__).parent / "ku/server.ku").read_text()

    def test_default_retains_original_source_except_peer_port(self):
        self.assertEqual(expand_ku_server(self.source, 12345),
                         self.source.replace("__PEER_PORT__", "12345"))

    def test_sync_changes_only_handler_execution_and_optional_numeric_reads(self):
        candidate = expand_ku_server(self.source, 12345)
        sync_int = expand_ku_server(self.source, 12345, pg_execution="sync")
        restored_execution = sync_int.replace('    app.get("/db", fn(req) {',
                                               "    async fn database(req: HttpRequest): HttpResponse! {")
        restored_execution = restored_execution.replace('        return http.text(status, body)\n    })',
                                                        '        return ok(http.text(status, body))\n    }\n    app.get("/db", database)')
        self.assertEqual(restored_execution, candidate)
        legacy = expand_ku_server(self.source, 12345, pg_read="value", pg_execution="sync")
        restored = sync_int.replace("key = result.value_int(0, 0)?",
                                    "key_value = json.decode(result.value(0, 0)?)?\n            key = key_value.as_int()?")
        restored = restored.replace("score = result.value_int(0, 2)?",
                                    "score_value = json.decode(result.value(0, 2)?)?\n            score = score_value.as_int()?")
        self.assertEqual(restored, legacy)
        self.assertIn('app.get("/db", database)', candidate)
        self.assertIn('return ok(http.text(status, body))', candidate)
        for text in (sync_int, legacy):
            self.assertIn('app.get("/db", fn(req) {', text)
            self.assertIn('return http.text(status, body)', text)
        self.assertNotIn("value_int(", legacy)
        self.assertEqual(candidate.count("value_int("), 2)
        self.assertIn("label = result.value(0, 1)?", candidate)
        for text in (candidate, sync_int, legacy):
            self.assertIn('SELECT id,label,score FROM cross_stack_items WHERE id=$1', text)
            self.assertIn('max_connections: 16, max_waiters: 128', text)

    @unittest.skipUnless(os.environ.get("KU_BENCHMARK_CLI"), "set KU_BENCHMARK_CLI to an existing Ku CLI for semantic fixture regression")
    def test_pg_overload_catch_preserves_other_errors_and_next_request(self):
        # Execute the actual fixture catch body, not a Python copy of its policy.
        # The isolated program never imports PG, opens a socket or retries SQL.
        start = self.source.index('        } catch (error) {')
        end = self.source.index('        return ok(http.text(status, body))', start)
        catch = self.source[start + len('        } '):end]
        program = '''fn main() {
    for failure in [{domain: "pg", code: "pool_busy", message: "synthetic"},
                     {domain: "pg", code: "query_error", message: "synthetic"},
                     {domain: "app", code: "pool_busy", message: "synthetic"},
                     {domain: "", code: "", message: "synthetic"}] {
        body = "database_error"
        status = 500
        try {
            if (failure.code != "") { fail failure }
            body = "ok:next-plan"
            status = 200
        } ''' + catch + '''
        println(status)
        println(body)
    }
}
'''
        with tempfile.TemporaryDirectory(prefix="ku-pg-overload-") as directory:
            source = Path(directory) / "contract.ku"
            source.write_text(program)
            result = subprocess.run([os.environ["KU_BENCHMARK_CLI"], str(source)],
                                    cwd=directory, text=True, capture_output=True, timeout=15)
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(result.stdout.splitlines(), ["503", "database_error:pool_busy",
                "500", "database_error:query_error", "500", "database_error:pool_busy",
                "200", "ok:next-plan"])

    def test_composes_with_direct_and_json_response_and_rejects_unknown(self):
        combined = expand_ku_server(self.source, 12345, "direct", "response", "int", "async")
        self.assertIn("text = str(total)", combined)
        self.assertIn("return ok(http.json({id:", combined)
        self.assertEqual(combined.count("value_int("), 2)
        with self.assertRaises(ValueError):
            expand_ku_server(self.source, 12345, pg_read="auto")
        with self.assertRaises(ValueError):
            expand_ku_server(self.source, 12345, pg_execution="auto")
        with self.assertRaisesRegex(ValueError, "sync-only"):
            expand_ku_server(self.source, 12345, pg_read="value")
        with self.assertRaises(RuntimeError):
            expand_ku_server(self.source.replace("key = result.value_int", "key = other_value_int"),
                             12345)
        with self.assertRaises(RuntimeError):
            expand_ku_server(self.source.replace("async fn database", "async fn other_database"),
                             12345)


if __name__ == "__main__":
    unittest.main()
