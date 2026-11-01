#!/usr/bin/env python3
"""Pure fixed-artifact restoration guards; no compiler/service/database."""
import hashlib
import importlib.util
import io
import json
from pathlib import Path
import tarfile
import tempfile
import unittest
from unittest.mock import patch
import zipfile

spec=importlib.util.spec_from_file_location('prepare',Path(__file__).with_name('prepare-libpq-loader-memory.py'))
p=importlib.util.module_from_spec(spec);spec.loader.exec_module(p)


class RestoreTests(unittest.TestCase):
    def test_zip_paths_are_bounded_and_links_or_duplicates_rejected(self):
        for names in [('/escape',),('../escape',),('ok','ok')]:
            with self.subTest(names=names),tempfile.TemporaryDirectory() as d:
                z=Path(d)/'test.zip'
                with zipfile.ZipFile(z,'w') as target:
                    for name in names:target.writestr(name,b'x')
                with zipfile.ZipFile(z) as source,self.assertRaises(ValueError):p.zip_index(source)

    def test_fixed_old_restore_only_allowlisted_bytes_and_removes_temporary_tar(self):
        with tempfile.TemporaryDirectory() as d:
            root=Path(d);archive=root/'old.tar.gz';payload=b'original';raw=io.BytesIO()
            with tarfile.open(fileobj=raw,mode='w:gz') as target:
                for name,data in [('B/input',payload),('old-score.csv',b'never restore')]:
                    item=tarfile.TarInfo(name);item.size=len(data);target.addfile(item,io.BytesIO(data))
            z=root/'old.zip'
            with zipfile.ZipFile(z,'w') as target:
                target.writestr('ku-performance-v2-evidence.tar.gz',raw.getvalue());target.writestr('archive.json','{}')
            fixture=root/'allow.json';fixture.write_text(json.dumps({'files':[dict(path='B/input',bytes=len(payload),
                archived_sha256=hashlib.sha256(payload).hexdigest(),mode=0o644)]}))
            with patch.object(p,'OLD_ZIP_SHA',p.digest(z)),patch.object(p,'OLD_TAR_SHA',hashlib.sha256(raw.getvalue()).hexdigest()):
                receipt=p.restore_old(z,root/'original',fixture)
            self.assertEqual((root/'original/B/input').read_bytes(),payload)
            self.assertFalse((root/'original/old-score.csv').exists());self.assertFalse((root/'original-evidence.tar.gz').exists())
            self.assertFalse(receipt['old_measurements_restored'])

    def test_wrong_zip_digest_stops_before_creating_output(self):
        with tempfile.TemporaryDirectory() as d:
            root=Path(d);z=root/'wrong.zip';z.write_bytes(b'wrong')
            with self.assertRaises(ValueError):p.restore_new(z,root/'new')
            self.assertFalse((root/'new').exists())


if __name__=='__main__':unittest.main()
