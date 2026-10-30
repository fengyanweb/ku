"""Portable cache identity/path tests; no executable, compiler or service runs."""
import hashlib
import io
import json
from pathlib import Path
import tarfile
import tempfile
import subprocess
import unittest
import types
from contextlib import ExitStack
from unittest.mock import patch
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
    def test_reuse_selection_and_state_identity_match_the_explicit_hosted_flag(self):
        for selected in (None,False,True):
            with self.subTest(selected=selected),tempfile.TemporaryDirectory() as d,ExitStack() as patches:
                root=Path(d);fresh=root/'fresh';tools=root/'tools';tools.mkdir()
                args=types.SimpleNamespace(root=fresh,source=root/'source',tools=tools,archive=root/'inert.tgz',
                    archive_sha256='1'*64,source_run='123',baseline=r.h.BASELINE,
                    candidate='b'*40,prepatch='f'*40,screen_variants=[])
                if selected is not None:args.task_json_screen=selected
                def extract(archive,root,digest,expected):
                    for name in expected:(root/name).mkdir(parents=True)
                    # Even capable normal cached builds must not implicitly
                    # change the requested experiment to a Task JSON screen.
                    ir=root/'B/src/src/ir/task.rs';ir.parent.mkdir(parents=True)
                    ir.write_text('enum TaskOp {\n    JsonDecode {\n}\n')
                    return {}
                patches.enter_context(patch.object(r.os,'geteuid',return_value=1))
                patches.enter_context(patch.object(r.platform,'system',return_value='Linux'))
                patches.enter_context(patch.object(r.platform,'machine',return_value='aarch64'))
                extract_mock=patches.enter_context(patch.object(r,'extract_builds',side_effect=extract))
                patches.enter_context(patch.object(r,'restore_json_inputs'))
                patches.enter_context(patch.object(r,'validate_protocol_inputs',return_value={}))
                verify=patches.enter_context(patch.object(r,'verify_manifest'))
                patches.enter_context(patch.object(r,'new_driver'))
                patches.enter_context(patch.object(r.h,'complete_build',return_value={'execution_complete':True}))
                self.assertTrue(r.prepare(args)['execution_complete'])
                expected={'A','B','R',*(['BT'] if selected else [])}
                self.assertEqual(set(extract_mock.call_args.args[3]),expected)
                self.assertEqual({call.args[1] for call in verify.call_args_list},expected)
                state=r.h.load(fresh/'hosted-state.json')
                self.assertEqual(state['identity'],r.h.hosted_identity(args,'followup-screen'))
                self.assertIs(state['identity']['task_json_screen'],selected is True)

    def test_reformatted_source_json_is_recovered_from_exact_commit_without_redefining_corpus(self):
        with tempfile.TemporaryDirectory() as d:
            root=Path(d);source=root/'repository';source.mkdir()
            subprocess.run(['git','init','-q',str(source)],check=True)
            prefix=Path('tests/benchmarks/cross_stack');directory=source/prefix
            (directory/'corpora').mkdir(parents=True)
            original={}
            spec={}
            for name in ('object','array','escape'):
                raw='{"counter":7,"data":{"text":"中文🙂"}}\n'.encode('utf-8')
                relative='corpora/'+name+'.json';original[relative]=raw
                spec[name]={'path':relative,'bytes':len(raw),'sha256':hashlib.sha256(raw).hexdigest(),'counter':7}
            original['protocol.json']=(json.dumps({'schema':1,'corpora':spec},indent=2)+'\n').encode()
            for relative,raw in original.items():(directory/relative).write_bytes(raw)
            subprocess.run(['git','-C',str(source),'add','.'],check=True)
            subprocess.run(['git','-C',str(source),'-c','user.name=Fixture','-c','user.email=fixture@example.invalid','commit','-qm','immutable inputs'],check=True)
            commit=subprocess.check_output(['git','-C',str(source),'rev-parse','HEAD'],text=True).strip()
            destination=root/'fresh';mapping={}
            for name in ('A','B'):
                for relative,raw in original.items():
                    path=name+'/src/'+prefix.as_posix()+'/'+relative
                    exported=(json.dumps(json.loads(raw),sort_keys=True,separators=(',',':'))+'\n').encode()
                    self.assertNotEqual(exported,raw)
                    target=destination/path;target.parent.mkdir(parents=True,exist_ok=True);target.write_bytes(exported)
                    mapping[path]={'original_sha256':hashlib.sha256(raw).hexdigest(),'archived_sha256':hashlib.sha256(exported).hexdigest(),'bytes':len(exported)}
                artifact=destination/name/'build/server.c';artifact.parent.mkdir(parents=True);artifact.write_bytes(b'untouched C')
            r.restore_json_inputs(source,destination,{'A':commit,'B':commit},mapping)
            for name in ('A','B'):
                result=r.validate_protocol_inputs(destination,name,mapping)
                self.assertEqual(result['corpora']['object']['sha256'],spec['object']['sha256'])
                for relative,raw in original.items():self.assertEqual((destination/name/'src'/prefix/relative).read_bytes(),raw)
                self.assertEqual((destination/name/'build/server.c').read_bytes(),b'untouched C')
            corpus=destination/'A/src'/prefix/'corpora/object.json'
            corpus.write_bytes(corpus.read_bytes()+b' ')
            with self.assertRaises(ValueError):r.validate_protocol_inputs(destination,'A',mapping)

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
            with self.assertRaisesRegex(ValueError,'build was not completed'):
                r.extract_builds(tar,root/'missing-task-build',r.h.sha(tar),{'A','BT'})
            self.assertFalse((root/'missing-task-build').exists())

    def test_corrupt_or_wrong_architecture_normal_artifact_is_rejected(self):
        with tempfile.TemporaryDirectory() as d:
            path=Path(d)/'binary';header=bytearray(20);header[:6]=b'\x7fELF\x02\x01';header[18:20]=(183).to_bytes(2,'little')
            path.write_bytes(header);r.require_elf(path)
            good=r.h.sha(path);path.write_bytes(header+b'tampered')
            with self.assertRaises(ValueError):r.checked(path,good)
            header[18:20]=(62).to_bytes(2,'little');path.write_bytes(header)
            with self.assertRaises(ValueError):r.require_elf(path)


if __name__=='__main__':unittest.main()
