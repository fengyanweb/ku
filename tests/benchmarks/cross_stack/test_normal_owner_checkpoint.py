"""Pure scalar-memory checks, with no target process/HTTP/database launched."""
from contextlib import ExitStack
import json
from pathlib import Path
import re
import signal
import struct
import subprocess
import tempfile
import time
import unittest
from unittest.mock import Mock, patch
import normal_owner_checkpoint as n

class ScalarFixture:
    def __init__(self):
        self.layout={'pointer_bytes':8,'types':{},'element_bytes':dict(driver_slot=128,task_handle=128,ring=8,free_handle=4,binding_key=8,connection=512,registration=64,token_bucket=24,fd_bucket=4,int=4,pointer=8,blocking_job=64,blocking_input=32,pending_item=64)}
        for line in n.TEMPLATE.read_text().splitlines():
            begin=re.search(r'^  BEGIN\((\w+)\);',line)
            if begin:
                typename=begin[1];self.layout['types'][typename]={'size':0,'fields':{}}
            field=re.search(r'^  FIELD\(\w+,(\w+)\);',line)
            if field:
                t=self.layout['types'][typename];t['fields'][field[1]]={'offset':t['size'],'bytes':8};t['size']+=8
        t=self.layout['types']['KuHttpTaskRuntime']; ds=self.layout['types']['KuTaskDriverV1']['size']
        for key,f in t['fields'].items():
            if key!='driver':f['offset']+=ds-8
        t['fields']['driver']['bytes']=ds;t['size']+=ds-8
        self.memory={(0x1000,8):(0x2000).to_bytes(8,'little')}
        self.set(0x2000,'KuHttpLoopGroup',count=1,created=1,started=1,draining=0,tasks=0x4000)
        self.set(0x4000,'KuHttpTaskRuntime',capacity=16,active_handles=0,free_handle_count=16,slots=0x10000,ring=0x12000,handles=0x14000,free_handles=0x16000,blocking=0x6000)
        self.set(0x4000,'KuTaskDriverV1',capacity=16,resident=0,queued=0,running=0,building=0,closing=0,binding_cells=128,binding_keys=0x18000)
        self.set(0x6000,'KuTaskBlockingPoolV1',capacity=16,resident=0,queued=0,running=0,net_wait_count=0,net_step_count=0,storage=0x100000,storage_bytes=8192,jobs=0x100000,inputs=0x100400,ring=0x100600,free_indices=0x100680)
        owner=0x2000+self.layout['types']['KuHttpLoopGroup']['fields']['loops']['offset']
        self.set(owner,'KuHttpLoopOwner',index=0,slots=2,ready=1,exited=0,connection_base=0x20000,handler_queue=0x8000,queued=0,running=0,resident=0,active_requests=0,connections=1,pending_count=0)
        self.set(0x8000,'KuHttpQueue',count=0,resident=0,running=0,cap=2,items=0x22000)
        self.set(0x20000,'KuHttpConnection',active_set=0xa000,reactor=0xc000,phase=1,request_lease=0,request_queued=0,buffered=0,carry_length=0,job=0,async_task=0,header=0,capacity=0)
        self.set(0xa000,'KuHttpActiveSet',request_active=0,capacity=2,connections=0x20000,indices=0x24000,pending=0xe000)
        self.set(0xe000,'KuHttpPending',count=0,raw_count=0,cap=2,items=0x30000)
        self.set(0xc000,'KuHttpReactor',active=0xa000,slots=2,capacity=4,hash_capacity=16,fd_capacity=16,registrations=0x26000,hash=0x28000,fd_entries=0x2a000,work=0x2c000,idle_heap=0x2e000)
        self.maps=[dict(start=0x1000,end=0x200000,perms='rw-p')]
    def set(self,pointer,typename,**values):
        for name,value in values.items():
            f=self.layout['types'][typename]['fields'][name];self.memory[(pointer+f['offset'],f['bytes'])]=value.to_bytes(f['bytes'],'little')
    def read(self,fd,address,count):return self.memory[(address,count)]
    def capture(self):
        reader=n.OwnerReader(123,self.maps,self.layout,543,time.monotonic()+10)
        with patch.object(n.rs,'read_exact',side_effect=self.read):return reader.snapshot(0x1000)

class FixedOwnerTests(unittest.TestCase):
    def test_valid_scalar_traversal_and_embedded_storage_not_double_counted(self):
        f=ScalarFixture(); result=f.capture(); labels={r['label'] for r in result['records']}
        self.assertEqual(len(result['records']),17)
        self.assertTrue({'task-slots','blocking-storage','reactor-fd_entries'}<=labels)
        self.assertFalse({'embedded-driver','blocking-jobs'}&labels)
        self.assertFalse(result['coverage_complete']);self.assertEqual(result['missing'],[])
    def test_quiet_uses_live_queue_driver_and_pool_not_cached_owner(self):
        for pointer,typename,name in [(0x4000,'KuTaskDriverV1','building'),(0x8000,'KuHttpQueue','count'),(0x6000,'KuTaskBlockingPoolV1','net_wait_count'),(0xe000,'KuHttpPending','count')]:
            with self.subTest(name=name):
                f=ScalarFixture();f.set(pointer,typename,**{name:1})
                with self.assertRaisesRegex(ValueError,'not quiet'):f.capture()
    def test_fd_backing_uses_allocated_hash_capacity_not_logical_lookup_capacity(self):
        f=ScalarFixture();f.set(0xc000,'KuHttpReactor',fd_capacity=4,hash_capacity=16)
        result=f.capture();row=next(x for x in result['records'] if x['label']=='reactor-fd_entries')
        self.assertEqual(row['requested_bytes'],16*4)
        metadata=next(x for x in result['metadata'] if x['kind'].startswith('reactor fd table'))
        self.assertEqual(metadata['fd_logical_cells'],4);self.assertEqual(metadata['fd_storage_cells'],16)
        self.assertEqual(metadata['backing_bytes'],64)
    def test_idle_connection_and_retained_header_capacity_are_allowed_without_payload_reads(self):
        f=ScalarFixture();f.set(0x20000,'KuHttpConnection',header=0x32000,capacity=4096)
        result=f.capture();row=next(x for x in result['records'] if x['label']=='idle-header-capacity')
        self.assertEqual(row['requested_bytes'],4096)
        self.assertEqual(next(x for x in result['metadata'] if x['kind'].startswith('idle connection'))['connections'],1)
    def test_null_active_marks_unknown_without_guessing(self):
        f=ScalarFixture();f.set(0x20000,'KuHttpConnection',active_set=0);r=f.capture()
        self.assertEqual(len(r['missing']),1)
        self.assertNotIn('reactor-hash',[x['label'] for x in r['records']])
    def test_caps_bad_subranges_overlaps_and_unmapped_ranges_fail(self):
        cases=[(0x4000,'KuTaskDriverV1','capacity',16385,'capacity'),(0x6000,'KuTaskBlockingPoolV1','inputs',0x200000,'subrange'),(0x4000,'KuHttpTaskRuntime','slots',0x4000,'overlap'),(0x4000,'KuHttpTaskRuntime','ring',0x300000,'readable')]
        for pointer,typename,name,value,message in cases:
            with self.subTest(name=name):
                f=ScalarFixture();f.set(pointer,typename,**{name:value})
                with self.assertRaisesRegex(ValueError,message):f.capture()
    def test_only_compiled_scalar_widths_inside_type_are_read(self):
        f=ScalarFixture();f.layout['types']['KuHttpLoopGroup']['fields']['count']['bytes']=16
        with self.assertRaisesRegex(ValueError,'scalar'):f.capture()
        f=ScalarFixture();f.layout['types']['KuHttpLoopGroup']['fields']['count']['offset']=99999
        with self.assertRaisesRegex(ValueError,'outside'):f.capture()
    def test_unique_page_union_is_not_residency(self):
        o=dict(records=[dict(label='a',context='x',pointer='0x1100',requested_bytes=0xf00),dict(label='b',context='y',pointer='0x2000',requested_bytes=0x1800),dict(label='c',context='z',pointer='0x3800',requested_bytes=0x100)])
        r=n.page_ranges(o,4096);self.assertEqual(r['unique_pages'],3);self.assertEqual(r['unique_boundary_pages'],2)
        self.assertEqual([x['full_interior_pages'] for x in r['records']],[0,1,0]);self.assertFalse(r['resident_bytes_inferred'])
        o['records'][0]['requested_bytes']=8193*4096
        with self.assertRaisesRegex(ValueError,'8192'):n.page_ranges(o,4096)
    def test_load_bias_anchors_header_not_duplicate_relro(self):
        maps=[dict(start=0x10000,end=0x11000,perms='r--p',inode=17,device='00:01',offset=0),dict(start=0x13000,end=0x14000,perms='rw-p',inode=17,device='00:01',offset=0x2000),dict(start=0x14000,end=0x15000,perms='r--p',inode=17,device='00:01',offset=0x2000)]
        seg=[(1,4,0,0,0,0x1000,0x1000,0x1000),(1,6,0x2000,0x3000,0,0x1000,0x1000,0x1000)]
        with patch.object(n,'elf_loads',return_value=(3,seg)):
            self.assertEqual(n.load_bias(maps,'/not-read',17,'00:01',0x3000,4096),0x10000)
            with self.assertRaises(ValueError):n.load_bias(maps,'/not-read',18,'00:01',0x3000,4096)
    def test_reused_page_reader_accepts_subset_and_keeps_unknown_permissions(self):
        owners=dict(complete=True,records=[dict(pid=123,label='one',context='fixed',pointer='0x2000',requested_bytes=4096)])
        with patch.object(n.hcd,'read_text',return_value='(fixture) '+' '.join(['x']*19+['77'])),patch.object(n.hcd.Path,'open',side_effect=PermissionError('metadata fixture unavailable')):
            result=n.hcd.allocation_owner_pages(123,owners,'77',maximum_records=16384)
        self.assertFalse(result['available']);self.assertIn('metadata fixture unavailable',result['unavailable_reason'])
        default=n.hcd.allocation_owner_pages(123,owners,'77')
        self.assertFalse(default['available']);self.assertIn('validated bounded',default['unavailable_reason'])
    def test_UTF8_header_name_is_literal_and_ambiguous_paths_are_rejected(self):
        wrapper=n.layout_wrapper(Path('/tmp/中文目录/server.c'))
        self.assertIn('#include "/tmp/中文目录/server.c"',wrapper)
        self.assertNotIn('\\u4e2d',wrapper)
        for path in ('/tmp/a"b.c','/tmp/a\\b.c','/tmp/a\nb.c'):
            with self.assertRaisesRegex(ValueError,'header-name'):n.layout_wrapper(Path(path))
    def test_scalar4_read_and_deadline_reject_before_remote_read(self):
        f=ScalarFixture();r=n.OwnerReader(123,f.maps,f.layout,543,time.monotonic()+10)
        with patch.object(n.rs,'read_exact',return_value=bytes([0x78,0x56,0x34,0x12])) as read:
            self.assertEqual(r.word(0x1800,4),0x12345678);read.assert_called_once_with(123,0x1800,4)
        r.deadline=time.monotonic()-1
        with patch.object(n.rs,'read_exact') as read:
            with self.assertRaises(TimeoutError):r.word(0x1800,4)
            read.assert_not_called()
    def test_exact_global_symbol_readback_is_retained(self):
        with tempfile.TemporaryDirectory() as temp,patch.object(n.subprocess,'check_output',return_value='00000010 00000008 b ku_http_live_group\n'):
            p=Path(temp)/'symbol.readback'
            self.assertEqual(n.global_symbol('/not-read',p),16)
            self.assertEqual(p.read_text(),'00000010 00000008 b ku_http_live_group\n')
    def test_global_is_one_exact8_byte_data_symbol(self):
        for raw in ['0000 0008 b ku_http_live_group\n0008 0008 b ku_http_live_group\n','0000 0010 b ku_http_live_group\n','0000 0008 t ku_http_live_group\n']:
            with patch.object(n.subprocess,'check_output',return_value=raw):
                with self.assertRaises(ValueError):n.global_symbol('/not-read')
    def test_prepare_rejects_mixed_manifest_before_compiler(self):
        with tempfile.TemporaryDirectory() as temp:
            p=Path(temp)
            for name in ('elf','source','compiler','libpq'):(p/name).write_bytes(b'fixture')
            r=dict(kind='compile',flags=['-std=c11','-O2','-pthread'])
            m={'candidate_base':'a'*40,'stacks':{'ku':{'build':{'native':{'server':dict(compiler_invocation=r,binary_sha256='b'*64,generated_c_sha256='b'*64)}}}}}
            (p/'receipt').write_text(json.dumps(r));(p/'manifest').write_text(json.dumps(m))
            with patch.object(n.subprocess,'run') as run:
                with self.assertRaisesRegex(ValueError,'ELF/C'):n.prepare(p/'elf',p/'source',p/'manifest',p/'receipt',p/'compiler',p/'libpq','a'*40,p/'out')
                run.assert_not_called();self.assertFalse((p/'out').exists())

class BssMappingTests(unittest.TestCase):
    BASE=0xab3fd4e10000
    # Header-only unit fixture with the actual234300 PT_LOAD geometry, not
    # executable code and never a substitute for the identified normal ELF.
    SEGMENTS=[(1,5,0,0,0,0x95150,0x95150,0x10000),
              (1,6,0x9fa80,0xafa80,0xafa80,0x928,0x10fe68,0x10000)]
    def fixture(self,root):
        path=root/'header-shape.elf';raw=bytearray(0xa03a8)
        raw[:6]=b'\x7fELF\x02\x01';struct.pack_into('<HH',raw,16,3,183)
        struct.pack_into('<Q',raw,32,64);struct.pack_into('<HH',raw,54,56,2)
        for i,segment in enumerate(self.SEGMENTS):struct.pack_into('<IIQQQQQQ',raw,64+i*56,*segment)
        path.write_bytes(raw)
        rows=[(0,0x96000,'r-xp',0,17,'08:01','/normal/ku-server'),
              (0xaf000,0xb0000,'r--p',0x9f000,17,'08:01','/normal/ku-server'),
              (0xb0000,0xb1000,'rw-p',0xa0000,17,'08:01','/normal/ku-server'),
              (0xb1000,0x1c0000,'rw-p',0,0,'00:00','')]
        return path,[dict(start=self.BASE+a,end=self.BASE+b,perms=perms,offset=offset,inode=inode,device=device,path=name) for a,b,perms,offset,inode,device,name in rows]
    def test_actual_shape_global_and_three_static_spans_use_one_proven_bias(self):
        with tempfile.TemporaryDirectory() as temp:
            elf,maps=self.fixture(Path(temp))
            for symbol,size in [(0xb84a8,8),(0xb03c0,0x8001),(0xb84b0,0x62c0),(0xbe778,0x101168)]:
                self.assertEqual(n.load_bias(maps,elf,17,'08:01',symbol,4096,symbol_bytes=size),self.BASE)
    def test_bss_does_not_relax_bad_inode_offset_extent_or_anon_identity(self):
        changes=[('anchor inode',lambda m:m[0].update(inode=18)),('data inode',lambda m:m[1].update(inode=18)),
                 ('data offset',lambda m:m[2].update(offset=0xa1000)),('anon inode',lambda m:m[3].update(inode=19)),
                 ('unrelated heap',lambda m:m[3].update(path='[heap]')),('truncated tail',lambda m:m[3].update(end=self.BASE+0x1bf000)),
                 ('unreadable tail',lambda m:m[3].update(perms='---p'))]
        with tempfile.TemporaryDirectory() as temp:
            for name,mutate in changes:
                with self.subTest(name=name):
                    elf,maps=self.fixture(Path(temp));mutate(maps)
                    with self.assertRaises(ValueError):n.load_bias(maps,elf,17,'08:01',0xb84a8,4096)
            elf,maps=self.fixture(Path(temp))
            for symbol,size in [(0x1bf8e8,8),(0xbe778,0x101171),(0x1c1000,8)]:
                with self.assertRaises(ValueError):n.load_bias(maps,elf,17,'08:01',symbol,4096,symbol_bytes=size)
    def test_known_static_nm_readback_exports_only_address_and_size(self):
        raw='000b84a8 00000008 b ku_http_live_group\n000b03c0 00008001 b ku_fs_base_path\n000b84b0 000062c0 b ku_pg_diagnostics\n000be778 00101168 b ku_service_output\n'
        with tempfile.TemporaryDirectory() as temp,patch.object(n.subprocess,'check_output',return_value=raw):
            root=Path(temp);self.assertEqual(n.global_symbol('/not-read',root/'symbols',root/'statics'),0xb84a8)
            values=json.loads((root/'statics').read_text());self.assertEqual([x['name'] for x in values],list(n.KNOWN_STATICS))
            self.assertEqual(values[-1]['symbol_bytes'],0x101168)
            self.assertTrue(all(set(x)=={'name','vaddr','symbol_bytes'} for x in values))


class ResumeTests(unittest.TestCase):
    def call(self,mode):
        child=Mock(spec=subprocess.Popen);child.pid=3456;child.poll.return_value=None
        p=dict(schema='ku-normal-fixed-owner-prepared-v1',ELF='/fake/elf',source='/fake/C',compiler='/fake/CC',libpq='/fake/pq',identities=dict(ELF='x',source='x',compiler='x',libpq='x'),layout='/fake/layout',layout_SHA256='x',template_SHA256='x',normal_manifest='/fake/manifest',normal_manifest_SHA256='x',global_symbol_vaddr=8,product_source_commit='a'*40)
        with tempfile.TemporaryDirectory() as temp,ExitStack() as stack:
            stack.enter_context(patch.object(n,'bounded_json',side_effect=[p,{}]));stack.enter_context(patch.object(n,'digest',return_value='x'))
            stack.enter_context(patch.object(n.Path,'stat',return_value=Mock(st_ino=123,st_dev=0)))
            stack.enter_context(patch.object(n,'read_start',return_value='77'));stack.enter_context(patch.object(n,'check_identity',return_value='State:\tT (stopped)\n' if mode=='stopped' else 'State:\tS (sleeping)\n'))
            stack.enter_context(patch.object(n.os,'open',return_value=90));stack.enter_context(patch.object(n.os,'pidfd_open',return_value=91,create=True))
            send=stack.enter_context(patch.object(n.signal,'pidfd_send_signal',create=True));close=stack.enter_context(patch.object(n.os,'close'))
            if mode=='bias-failed':
                stack.enter_context(patch.object(n.rs,'stopped_threads',return_value=['3456']))
                stack.enter_context(patch.object(n.rs,'read_text',return_value='1000-2000 r--p 00000000 00:00 123 /fake/elf\n'))
                stack.enter_context(patch.object(n,'load_bias',side_effect=ValueError('injected BSS proof failure')))
            else:stack.enter_context(patch.object(n.rs,'stopped_threads',side_effect=RuntimeError('injected failure')))
            if mode=='stopped':
                with self.assertRaisesRegex(ValueError,'pre-stopped'):n.capture_owned(child,'/fake/prepared',Path(temp)/'out',dict.fromkeys(n.REQUIRED_BOUNDARY,True))
                send.assert_not_called();close.assert_not_called()
            elif mode=='identity-changed':
                with patch.object(n,'check_identity',side_effect=['State:\tS (sleeping)\n',ValueError('identity changed')]):
                    with self.assertRaisesRegex(ValueError,'identity changed'):n.capture_owned(child,'/fake/prepared',Path(temp)/'out',dict.fromkeys(n.REQUIRED_BOUNDARY,True))
                send.assert_not_called();self.assertEqual([c.args[0] for c in close.call_args_list],[91,90])
            elif mode=='bias-failed':
                with self.assertRaisesRegex(ValueError,'injected BSS proof failure'):n.capture_owned(child,'/fake/prepared',Path(temp)/'out',dict.fromkeys(n.REQUIRED_BOUNDARY,True))
                output=Path(temp)/'out';self.assertTrue((output/'maps.log').exists())
                attempt=json.loads((output/'attempt.json').read_text());self.assertIn('injected BSS proof failure',attempt['primary_error']);self.assertFalse(attempt['memory_read_complete'])
                for key in ('STOP_requested_monotonic_ns','all_threads_STOP_ack_monotonic_ns','CONT_requested_monotonic_ns','CONT_returned_monotonic_ns'):self.assertIsNotNone(attempt[key])
                present={p.name for p in output.iterdir()};self.assertNotIn('owners.json',present);self.assertNotIn('smaps.log',present)
                self.assertEqual([c.args[1] for c in send.call_args_list],[signal.SIGSTOP,signal.SIGCONT]);self.assertEqual([c.args[0] for c in close.call_args_list],[91,90])
            else:
                with self.assertRaisesRegex(RuntimeError,'injected failure'):n.capture_owned(child,'/fake/prepared',Path(temp)/'out',dict.fromkeys(n.REQUIRED_BOUNDARY,True))
                self.assertEqual([c.args[1] for c in send.call_args_list],[signal.SIGSTOP,signal.SIGCONT])
                self.assertEqual([c.args[0] for c in close.call_args_list],[91,90])
    def test_bias_failure_retains_maps_and_real_attempt_timing_without_fabricated_outputs(self):self.call('bias-failed')
    def test_exception_always_continues_exact_pidfd_and_closes(self):self.call('failure')
    def test_pre_stopped_child_is_not_signalled_or_resumed(self):self.call('stopped')
    def test_identity_changed_after_pidfd_open_is_not_stopped(self):self.call('identity-changed')
    def test_missing_boundary_never_reads_or_signals_child(self):
        child=Mock(spec=subprocess.Popen);child.poll.return_value=None
        with patch.object(n,'bounded_json') as read:
            with self.assertRaisesRegex(ValueError,'independent idle'):n.capture_owned(child,'/not-read','/not-created',{})
            read.assert_not_called()

if __name__=='__main__':unittest.main()
