"""Identity/mode guards for PG comparisons; no VM or database is started."""
import json
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


class KuPGReadVariantTest(unittest.TestCase):
    def setUp(self):
        self.source = (Path(__file__).parent / "ku/server.ku").read_text()

    def test_default_retains_original_source_except_peer_port(self):
        self.assertEqual(expand_ku_server(self.source, 12345),
                         self.source.replace("__PEER_PORT__", "12345"))

    def test_integer_mode_changes_only_two_numeric_reads(self):
        original = expand_ku_server(self.source, 12345)
        candidate = expand_ku_server(self.source, 12345, pg_read="int")
        restored = candidate.replace("key = result.value_int(0, 0)?",
                                     "key_value = json.decode(result.value(0, 0)?)?\n            key = key_value.as_int()?")
        restored = restored.replace("score = result.value_int(0, 2)?",
                                    "score_value = json.decode(result.value(0, 2)?)?\n            score = score_value.as_int()?")
        self.assertEqual(restored, original)
        self.assertEqual(candidate.count("value_int("), 2)
        self.assertIn("label = result.value(0, 1)?", candidate)

    def test_composes_with_direct_and_json_response_and_rejects_unknown(self):
        combined = expand_ku_server(self.source, 12345, "direct", "response", "int")
        self.assertIn("text = str(total)", combined)
        self.assertIn("return ok(http.json({id:", combined)
        self.assertEqual(combined.count("value_int("), 2)
        with self.assertRaises(ValueError):
            expand_ku_server(self.source, 12345, pg_read="auto")
        with self.assertRaises(RuntimeError):
            expand_ku_server(self.source.replace("key_value = json.decode", "key_value = other_decode"),
                             12345, pg_read="int")


if __name__ == "__main__":
    unittest.main()
