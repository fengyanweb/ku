"""Pure fault-injection tests; no process, compiler, database or load."""
from contextlib import ExitStack
import json
from pathlib import Path
import signal
import struct
import tempfile
import unittest
from unittest.mock import patch
import http_cost_diagnostic as hcd
import resident_snapshot as r


class ResidentSnapshotTests(unittest.TestCase):
    def test_snapshot_always_resumes_and_does_not_snapshot_inflight_or_bad_counts(self):
        for mode in ('valid','inflight','bad-count','unstable-threads','lost'):
            with self.subTest(mode=mode),tempfile.TemporaryDirectory() as temporary,ExitStack() as stack:
                root=Path(temporary);library=root/'observer.so';library.write_bytes(b'fixture');library=library.resolve()
                maps='2000-3000 r-xp 00000000 00:00 %d %s\n5000-6000 rw-p 00000000 00:00 0\n'%(library.stat().st_ino,library)
                def text(path,limit):
                    if path.name=='maps':return maps
                    if path.name=='smaps':return maps
                    if path.name=='stat':return '(fixture) '+' '.join(['x']*19+['77'])
                    return 'State:\tT (stopped)\n' if 'task' in path.parts else 'State:\tS (sleeping)\n'
                stack.enter_context(patch.object(r,'read_text',side_effect=text))
                stack.enter_context(patch.object(r.subprocess,'check_output',return_value='0 B ku_resident_status\n100 B ku_resident_records\n'))
                stack.enter_context(patch.object(r.os,'open',return_value=99))
                close=stack.enter_context(patch.object(r.os,'close'))
                kill=stack.enter_context(patch.object(r.os,'kill'))
                stack.enter_context(patch.object(r.time,'sleep'))
                threads=stack.enter_context(patch.object(r,'stopped_threads',return_value=['123']))
                if mode=='unstable-threads':threads.side_effect=[['123'],['124']]*10
                words=[1,16384,int(mode=='inflight'),0,int(mode=='lost'),0,1,0,0,2 if mode=='bad-count' else 1,1]
                table=struct.pack('<5Q',0x5000,37,0x2008,123,1)+bytes((16384-1)*40)
                stack.enter_context(patch.object(r,'read_exact',side_effect=lambda fd,address,count:struct.pack('<11Q',*words) if count==88 else table))
                stack.enter_context(patch.object(r,'elf_identity',return_value={'sha256':'a'*64,'build_id':'fixed'}))
                stack.enter_context(patch.object(r,'dependency_owners',return_value={'dependencies':[]}))
                stack.enter_context(patch.object(hcd,'allocation_owner_pages',return_value={'available':True}))
                output=root/'result'
                if mode in ('bad-count','inflight','unstable-threads'):
                    with self.assertRaises(RuntimeError):r.capture(123,library,output)
                else:
                    result=r.capture(123,library,output)
                    value=json.loads((output/'live.json').read_text())
                    self.assertEqual(value['live_table_valid'],mode=='valid')
                    self.assertEqual(value['records'][0]['symbol_probe_pc'],'0x2007')
                    self.assertEqual(value['records'][0]['return_file_offset'],'0x8')
                    self.assertTrue(value['idle_only']);self.assertFalse(value['normal_score_eligible'])
                    self.assertGreaterEqual(value['total_stopped_ns'],0)
                    self.assertEqual(result['records'],1)
                signals=[call.args[1] for call in kill.call_args_list]
                self.assertEqual(signals[-1],signal.SIGCONT)
                self.assertEqual(signals.count(signal.SIGSTOP),signals.count(signal.SIGCONT))
                self.assertEqual(signals.count(signal.SIGSTOP),10 if mode in ('inflight','unstable-threads') else 1)
                close.assert_called_once_with(99)

    def test_already_stopped_process_is_never_owned_or_resumed(self):
        with patch.object(r,'read_text',return_value='State:\tT (stopped)\n'),patch.object(r.os,'kill') as kill:
            with self.assertRaisesRegex(RuntimeError,'already stopped'):r.capture(123,'/fixture.so','/unused')
            kill.assert_not_called()

    def test_ELF_tail_uses_anonymous_bounds_instead_of_virtual_size(self):
        with tempfile.TemporaryDirectory() as temporary:
            proc=Path(temporary);file=proc/'root/lib/fake.so';file.parent.mkdir(parents=True)
            raw=bytearray(4096);raw[:6]=b'\x7fELF\x02\x01'
            struct.pack_into('<Q',raw,32,64);struct.pack_into('<HH',raw,54,56,1)
            struct.pack_into('<IIQQQQQQ',raw,64,1,6,0,0,0,4096,8192,4096);file.write_bytes(raw)
            smaps='10000-11000 rw-p 00000000 00:00 1 /lib/fake.so\nAnonymous: 0 kB\n11000-13000 rw-p 00000000 00:00 0\nAnonymous: 4 kB\n'
            with patch.object(r.os,'sysconf',return_value=4096):
                result=r.dependency_owners(proc,r.maps_parse(smaps.splitlines()[0]),smaps)
            row=result['dependencies'][0]
            self.assertEqual(row['named_mapping_anonymous_bytes'],0)
            tail=row['ELF_zero_fill_tail_intersections'][0]
            self.assertEqual((tail['anonymous_lower_bytes'],tail['anonymous_upper_bytes']),(0,4096))


if __name__=='__main__':unittest.main()
