"""Portable cache identity/path tests; no executable, compiler or service runs."""
import hashlib
import io
import json
from pathlib import Path
import tarfile
import tempfile
import unittest
import reuse_build as r


def archive(path, changes=None, old_run=False):
    files={'A/build/bin/ku-server':b'normal immutable artifact',
           'hosted-state.json':json.dumps({'steps':{'build-A':{'state':'completed'},
               **({'run':{'state':'completed'}} if old_run else {})}}).encode()}
    index={'files':[{'path':n,'bytes':len(b),'original_sha256':hashlib.sha256(b).hexdigest(),
                    'archived_sha256':hashlib.sha256(b).hexdigest()} for n,b in files.items()],
           'path_root':r.TOKEN,'raw_and_redacted_hashes_distinct':True}
    files['portable-file-map.json']=json.dumps(index).encode()
    with tarfile.open(path,'w:gz') as tar:
        for name,raw in files.items():
            info=tarfile.TarInfo(name);info.size=len(raw);info.mode=0o755
            tar.addfile(info,io.BytesIO(raw))
        if changes is not None:
            info=changes;tar.addfile(info,io.BytesIO(b'x') if info.isfile() else None)
    return r.h.sha(path)


class ReuseBuildTests(unittest.TestCase):
    def test_checksum_or_unsafe_member_cannot_write_destination(self):
        for kind in ('checksum','traversal','absolute','link','duplicate'):
            with self.subTest(kind=kind),tempfile.TemporaryDirectory() as d:
                root=Path(d);tar=root/'cache.tgz';destination=root/'fresh'
                member=None
                if kind!='checksum':
                    member=tarfile.TarInfo({'traversal':'../outside','absolute':'/absolute',
                        'link':'A/link','duplicate':'A/build/bin/ku-server'}[kind])
                    if kind=='link':member.type=tarfile.SYMTYPE;member.linkname='../outside'
                    else:member.size=1
                digest=archive(tar,member)
                with self.assertRaises(ValueError):
                    r.extract_builds(tar,destination,'0'*64 if kind=='checksum' else digest,{'A'})
                self.assertFalse(destination.exists());self.assertFalse((root/'outside').exists())

    def test_only_fresh_tools_directory_is_allowed_and_old_results_never_resume(self):
        with tempfile.TemporaryDirectory() as d:
            root=Path(d);tar=root/'cache.tgz';digest=archive(tar)
            existing=root/'existing';existing.mkdir();(existing/'keep').write_text('user data')
            with self.assertRaises(ValueError):r.extract_builds(tar,existing,digest,{'A'})
            self.assertEqual((existing/'keep').read_text(),'user data')
            fresh=root/'fresh';(fresh/'tools').mkdir(parents=True)
            mapping=r.extract_builds(tar,fresh,digest,{'A'})
            r.checked(fresh/'A/build/bin/ku-server',mapping['A/build/bin/ku-server']['archived_sha256'])
            self.assertTrue((fresh/'reuse-original-state.json').exists())
            self.assertFalse((fresh/'hosted-state.json').exists())
            self.assertTrue((fresh/'A/build/bin/ku-server').stat().st_mode&0o100)
            old=root/'measured.tgz';digest=archive(old,old_run=True)
            with self.assertRaisesRegex(ValueError,'build-only'):
                r.extract_builds(old,root/'old-measurement',digest,{'A'})
            self.assertFalse((root/'old-measurement').exists())

    def test_corrupt_or_wrong_architecture_normal_artifact_is_rejected(self):
        with tempfile.TemporaryDirectory() as d:
            path=Path(d)/'binary';header=bytearray(20);header[:6]=b'\x7fELF\x02\x01';header[18:20]=(183).to_bytes(2,'little')
            path.write_bytes(header);r.require_elf(path)
            good=r.h.sha(path);path.write_bytes(header+b'tampered')
            with self.assertRaises(ValueError):r.checked(path,good)
            header[18:20]=(62).to_bytes(2,'little');path.write_bytes(header)
            with self.assertRaises(ValueError):r.require_elf(path)


if __name__=='__main__':unittest.main()
