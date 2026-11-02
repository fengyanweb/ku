"""Original archive/export contracts; synthetic fixtures never execute archived bytes."""
import gzip
import io
import json
import os
from pathlib import Path
import tarfile
import tempfile
import unittest
from unittest.mock import patch
import evidence_slice as s


class EvidenceSliceTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.archive, self.receipt, self.metadata, self.output = [self.root / n for n in (
            'source.tgz', 'archive.json', 'source-actions.json', 'slice.tgz')]
        self.files = {'frozen-plan.json': b'{}', 'A/manifest.json': b'{"candidate_base":"aaa"}',
            'A/build/bin/ku-server': b'\x7fELFnormal-service',
            'A/build/ku-native/.ku/build/release/c/key/ku-server.c': b'normal C',
            'http-short-screen/json_object/g1-A/window.json': b'original window',
            'http-short-screen/json_object/g1-A/score/resources.json': b'original samples',
            'http-short-screen/json_object/g1-A/score/resources.samples.jsonl': b'raw samples\n',
            'http-short-screen/json_object/g1-A/score/events.csv.gz': b'requests',
            'Driver/build/bin/client': b'big driver excluded', 'A/build/ku-cargo/release/ku': b'CLI excluded',
            'fixture/config.json': b'private conninfo excluded'}
        self.metadata.write_bytes(s.encoded(dict(repository=s.REPOSITORY, source_run='123',
            head_sha='a' * 40, run=dict(id=123, head_sha='a' * 40),
            artifact=dict(name=s.ARTIFACT, workflow_run=dict(id=123, head_sha='a' * 40)))))

    def pack(self, mutate_map=None, extra=None, omit_map=False):
        entries = [dict(path=n, bytes=len(b), original_sha256=s.digest(b), archived_sha256=s.digest(b))
                   for n, b in self.files.items()]
        index = dict(files=entries, path_root='${EVIDENCE_ROOT}', raw_and_redacted_hashes_distinct=True)
        if mutate_map:
            mutate_map(index)
        with tarfile.open(self.archive, 'w:gz') as tar:
            for name, data in self.files.items():
                s.add(tar, name, data)
            if not omit_map:
                s.add(tar, s.MAP, s.encoded(index))
            if extra:
                tar.addfile(extra, io.BytesIO(b'x') if extra.isfile() else None)
        self.receipt.write_bytes(s.encoded(dict(complete=True, bytes=self.archive.stat().st_size,
            sha256=s.digest(self.archive.read_bytes()), file_count=len(self.files))))

    def run_slice(self, include_self=False):
        return s.slice_archive(self.archive, self.receipt, self.metadata, self.output, include_self)

    def test_byte_exact_export_original_map_and_manifest_without_private_driver_or_csv(self):
        self.pack()
        report = self.run_slice()
        self.assertEqual(report['verified_files'], len(self.files))
        with tarfile.open(self.output) as tar:
            names = tar.getnames()
            manifest = json.load(tar.extractfile('slice-manifest.json'))
            for record in manifest['files']:
                raw = tar.extractfile(record['path']).read()
                self.assertEqual(raw, self.files[record['path']])
                self.assertEqual(s.digest(raw), record['exported_sha256'])
            self.assertIn('http-short-screen/json_object/g1-A/score/resources.samples.jsonl', names)
            self.assertNotIn('fixture/config.json', names)
            self.assertNotIn('Driver/build/bin/client', names)
            self.assertNotIn('A/build/ku-cargo/release/ku', names)
            self.assertFalse(any(n.endswith('.csv.gz') for n in names))
            with tarfile.open(self.archive) as source:
                self.assertEqual(tar.extractfile(s.MAP).read(), source.extractfile(s.MAP).read())
        original = self.output.read_bytes()
        with self.assertRaisesRegex(ValueError, 'never overwritten'):
            self.run_slice()
        self.assertEqual(self.output.read_bytes(), original)

    def test_optional_self_raw_and_only_named_symbol_dsos(self):
        base = 'normal-self-probe/B/sample/score/'
        self.files.update({base + n: b'raw' for n in ('perf.data', 'perf-pcs.log', 'maps.log',
            'symbol-identities.json', 'normal-self-observations.json', 'resources.json.raw')})
        for name in ('libpq.so.5.18', 'libc.so.6', 'ld-linux-aarch64.so.1', 'libm.so.6', 'other.so', 'ku'):
            self.files[base + 'symfs/usr/lib/' + name] = b'\x7fELFsymbol bytes'
        self.files[base + 'config.json'] = b'private'
        self.files[base + 'pmu-stat/stat.csv'] = b'ku-server-1;1;;cycles;1;100.00\n'
        self.files[base + 'pmu-stat/stat.log'] = b'Events disabled\n'
        self.files[base + 'requests.csv'] = b'private request rows'
        self.pack()
        self.run_slice()
        with tarfile.open(self.output) as tar:
            self.assertFalse(any(n.startswith('normal-self-probe/') for n in tar.getnames()))
        self.output.unlink()
        self.run_slice(True)
        with tarfile.open(self.output) as tar:
            for name in ('perf.data', 'perf-pcs.log', 'maps.log', 'symbol-identities.json', 'resources.json.raw'):
                self.assertIn(base + name, tar.getnames())
            for name in ('libpq.so.5.18', 'libc.so.6', 'ld-linux-aarch64.so.1', 'libm.so.6'):
                self.assertIn(base + 'symfs/usr/lib/' + name, tar.getnames())
            for name in ('other.so', 'ku'):
                self.assertNotIn(base + 'symfs/usr/lib/' + name, tar.getnames())
            self.assertNotIn(base + 'config.json', tar.getnames())
            self.assertIn(base + 'pmu-stat/stat.csv', tar.getnames())
            self.assertIn(base + 'pmu-stat/stat.log', tar.getnames())
            self.assertNotIn(base + 'requests.csv', tar.getnames())

    def test_unselected_bytes_and_every_map_entry_are_verified(self):
        mutations = [lambda m: m['files'][-1].update(archived_sha256='0' * 64),
            lambda m: m['files'][0].update(bytes=999), lambda m: m['files'].pop(),
            lambda m: m['files'].__setitem__(1, m['files'][0]),
            lambda m: m.update(path_root='/tmp/elsewhere')]
        for mutation in mutations:
            with self.subTest(mutation=mutation):
                self.pack(mutation)
                with self.assertRaises(ValueError):
                    self.run_slice()
                self.assertFalse(self.output.exists())
                self.assertEqual(list(self.root.glob('.evidence-slice-*')), [])

    def test_paths_links_directories_duplicates_and_missing_map_fail_closed(self):
        for name in ('../escape', '/absolute', 'a/../escape', 'a\\escape', 'C:/escape', './alias', 'a//alias', '.',
                     'http-short-screen/json_object/g1-A/window.json', s.MAP):
            with self.subTest(name=name):
                info = tarfile.TarInfo(name); info.size = 1
                self.pack(extra=info)
                with self.assertRaises(ValueError):
                    self.run_slice()
                self.assertFalse(self.output.exists())
        for kind in (tarfile.SYMTYPE, tarfile.LNKTYPE, tarfile.DIRTYPE, tarfile.FIFOTYPE):
            info = tarfile.TarInfo('unselected-link'); info.type = kind; info.linkname = '/tmp/escape'
            self.pack(extra=info)
            with self.assertRaises(ValueError):
                self.run_slice()
        self.pack(omit_map=True)
        with self.assertRaisesRegex(ValueError, 'map provenance absent'):
            self.run_slice()

    def test_bounds_receipt_and_trailing_gzip_expansion(self):
        self.pack()
        for name in ('MAX_ARCHIVE', 'MAX_FILE', 'MAX_EXPANDED', 'MAX_FILES'):
            with self.subTest(bound=name), patch.object(s, name, 1), self.assertRaises(ValueError):
                self.run_slice()
            self.assertFalse(self.output.exists())
        receipt = json.loads(self.receipt.read_bytes()); receipt['complete'] = False
        self.receipt.write_bytes(s.encoded(receipt))
        with self.assertRaisesRegex(ValueError, 'receipt'):
            self.run_slice()
        self.pack()
        with self.archive.open('ab') as stream:
            stream.write(gzip.compress(b'padding' * 4000))
        receipt = json.loads(self.receipt.read_bytes())
        receipt.update(bytes=self.archive.stat().st_size, sha256=s.digest(self.archive.read_bytes()))
        self.receipt.write_bytes(s.encoded(receipt))
        with patch.object(s, 'MAX_EXPANDED', 30000), self.assertRaisesRegex(ValueError, 'expanded bound'):
            self.run_slice()
        self.assertFalse(self.output.exists())
        with self.assertRaisesRegex(ValueError, 'after tar terminator'):
            self.run_slice()

    def test_metadata_restricts_numeric_same_repo_completed_exact_run_artifact(self):
        for value in ('', '0', '-1', '+123', ' 123', '123\n', '1;echo bad', '${x}', '9' * 21):
            with self.subTest(value=value), self.assertRaises(ValueError):
                s.run_id(value)
        run = dict(id=123, repository=dict(full_name=s.REPOSITORY), status='completed', head_sha='a' * 40,
            head_branch='branch', actor=dict(login='actor'), triggering_actor=dict(login='actor'),
            event='workflow_dispatch', conclusion='failure', html_url='https://github.com/fengyanweb/ku/actions/runs/123',
            created_at='now', updated_at='now')
        artifact = dict(id=456, name=s.ARTIFACT, expired=False, size_in_bytes=100,
                        workflow_run=dict(id=123, head_sha='a' * 40))
        listing = dict(artifacts=[artifact], total_count=1)
        class Response(io.BytesIO):
            pass
        calls = []
        def fetch(request, timeout):
            calls.append(request.full_url)
            return Response(s.encoded(listing if '/artifacts?' in request.full_url else run))
        destination, output = self.root / 'api.json', self.root / 'github-output'
        with patch.dict(os.environ, GITHUB_REPOSITORY=s.REPOSITORY, GH_TOKEN='test', GITHUB_OUTPUT=str(output)), \
                patch.object(s.urllib.request, 'urlopen', side_effect=fetch):
            s.fetch_metadata('123', destination)
            self.assertEqual(len(calls), 2)
            self.assertTrue(all(url.startswith('https://api.github.com/repos/fengyanweb/ku/') for url in calls))
            self.assertIn('artifact-id=456', output.read_text())
            self.assertEqual(json.loads(destination.read_text())['run']['actor']['login'], 'actor')
            for field, value in (('expired', True), ('workflow_run', dict(id=999, head_sha='a' * 40)),
                                 ('workflow_run', dict(id=123, head_sha='b' * 40)),
                                 ('size_in_bytes', s.MAX_ARCHIVE + (2 << 20))):
                with self.subTest(field=field), patch.dict(artifact, {field: value}), self.assertRaises(ValueError):
                    s.fetch_metadata('123', self.root / 'rejected.json')
            for field, value in (('total_count', 101), ('artifacts', []), ('artifacts', [artifact, artifact])):
                with self.subTest(field=field), patch.dict(listing, {field: value}), self.assertRaises(ValueError):
                    s.fetch_metadata('123', self.root / 'rejected.json')
            with patch.dict(run, status='in_progress'), self.assertRaises(ValueError):
                s.fetch_metadata('123', self.root / 'rejected.json')
            with patch.dict(os.environ, GITHUB_REPOSITORY='other/repo'), self.assertRaises(ValueError):
                s.fetch_metadata('123', self.root / 'rejected.json')
            run['repository']['full_name'] = 'other/repo'
            with self.assertRaises(ValueError):
                s.fetch_metadata('123', self.root / 'rejected.json')


if __name__ == '__main__':
    unittest.main()
