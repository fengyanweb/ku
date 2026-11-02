"""Pure PMU protocol/format tests: no real perf, process, service or load."""
from contextlib import ExitStack
from pathlib import Path
from unittest.mock import patch
import collections
import os
import struct
import tempfile
import unittest
import hosted_cycle as h
import pmu_control as p


def csv(tid=101):
    return ''.join(f'ku-server-{tid};{value};{unit};{event};1000000;100.00;;\n'
        for event,value,unit in [('cycles',3000000,''),('instructions',6000000,''),('task-clock',1,'msec')])


class PmuTests(unittest.TestCase):
    def test_plan_is_separate_and_keeps_normal_input_and_fixed_three_windows(self):
        old=h.normal_self_plan({},'ok-gpr');new=h.normal_self_plan({},'ok-pmu')
        for key in ('order','rate','warmup_seconds','score_seconds','maximum_windows','maximum_replacements','resources','quality_policy'):
            self.assertEqual(old[key],new[key])
        self.assertEqual((old['event'],new['event']),('cpu-clock','cycles'))
        self.assertNotIn('--sample-cpu',old['perf_flags']);self.assertIn('--sample-cpu',new['perf_flags'])
        self.assertFalse(new['normal_score_eligible']);self.assertFalse(new['formal_qualified'])

    def test_stat_parser_requires_exact_threads_group_counts_and_reported_running(self):
        with tempfile.TemporaryDirectory() as tmp:
            file=Path(tmp)/'stat.csv'
            for issue in ('none','missing','duplicate','changed-tid','not-counted','nan','multiplex','group-runtime'):
                data=csv()
                if issue=='missing':data=data.splitlines()[0]+'\n'
                if issue=='duplicate':data+=data.splitlines()[0]+'\n'
                if issue=='changed-tid':data=data.replace('-101;','-202;')
                if issue=='not-counted':data=data.replace('3000000;','<not counted>;')
                if issue=='nan':data=data.replace('3000000;','nan;')
                if issue=='multiplex':data=data.replace('100.00','99.99')
                if issue=='group-runtime':data=data.replace('instructions;1000000','instructions;1000001')
                file.write_text(data)
                if issue!='none':
                    with self.assertRaises(ValueError):p.parse_stat(file,[101])
                else:
                    result=p.parse_stat(file,[101]);self.assertEqual(result['cycles_per_instruction'],.5)
                    self.assertEqual(result['task_clock_ns'],1000000);self.assertFalse(result['scaled'])

    def test_dormant_zero_cpu_threads_are_explicit_not_silently_missing(self):
        with tempfile.TemporaryDirectory() as tmp:
            file=Path(tmp)/'stat.csv';file.write_text(csv()+''.join(f'idle-202;{value};{unit};{event};0;0.00;;\n'
                for event,value,unit in [('cycles','<not counted>',''),('instructions','<not counted>',''),('task-clock',0,'msec')]))
            result=p.parse_stat(file,[101,202]);self.assertEqual(result['dormant_zero_CPU_tids'],[202])
            with self.assertRaises(ValueError):p.parse_stat(file,[101])

    def test_stat_control_disable_ack_then_owned_SIGINT_and_all_failure_cleanup(self):
        for issue in ('success','exit0','bad-ack','disable-ack','client','exit130','changed-threads','missing-csv'):
            with self.subTest(issue=issue),tempfile.TemporaryDirectory() as tmp,ExitStack() as patches:
                out=Path(tmp)/'stat';record={};launched=[];client_calls=[]
                class Process:
                    pid=99999999;returncode=None
                    def poll(self):return self.returncode
                    def wait(self,timeout):self.returncode=0 if issue=='exit0' else 130 if issue=='exit130' else -2;return self.returncode
                def launch(argv,**kwargs):
                    launched.append((argv,kwargs));Path(argv[-1]).write_text('' if issue=='missing-csv' else csv());return Process()
                patches.enter_context(patch.object(p.subprocess,'Popen',side_effect=launch))
                patches.enter_context(patch.object(p,'thread_ids',side_effect=[[101],[101],[101],[101,202],[101,202]] if issue=='changed-threads' else lambda pid:[101]))
                patches.enter_context(patch.object(p.select,'select',side_effect=lambda r,w,e,t:(r,[],[])))
                chunks=[b'bad\n\0'] if issue=='bad-ack' else [b'ack\n\0',b'bad\n\0'] if issue=='disable-ack' else [b'ack\n\0',b'ack\n\0']
                patches.enter_context(patch.object(p.os,'read',side_effect=chunks))
                writes=patches.enter_context(patch.object(p.os,'write',side_effect=lambda fd,data:len(data)))
                kill=patches.enter_context(patch.object(p.os,'killpg'))
                def client():
                    self.assertEqual([r['command'] for r in record['control']],['enable']);client_calls.append(True)
                    if issue=='client':raise RuntimeError('client failed')
                    return {'untouched':True}
                if issue in ('bad-ack','disable-ack','client','missing-csv'):
                    with self.assertRaises((RuntimeError,ValueError)):p.controlled_stat(client,123,out,'/perf',record)
                else:self.assertEqual(p.controlled_stat(client,123,out,'/perf',record),{'untouched':True})
                self.assertEqual(record['complete'],issue in ('success','exit0'))
                self.assertEqual(bool(client_calls),issue!='bad-ack')
                argv,options=launched[0];self.assertIn('{cycles,instructions}',argv);self.assertIn('--per-thread',argv)
                self.assertEqual(argv[argv.index('-p')+1],'123');self.assertIn('--delay=-1',argv)
                for fd in options['pass_fds']:
                    with self.assertRaises(OSError):os.fstat(fd)
                if issue in ('success','exit0'):
                    self.assertEqual([x['command'] for x in record['control']],['enable','disable'])
                    self.assertEqual(kill.call_args_list[0].args,(99999999,p.signal.SIGINT))
                self.assertTrue(all(call.args[0]==99999999 for call in kill.call_args_list))

    def test_cycles_attributes_require_actual_hardware_cpu_and_full_GPR(self):
        good=dict(type=10,config=17,frequency_mode=True,frequency=999,user_regs_mask='0x1ffffffff',stack_user=16384,sample_type=78247)
        identities=[dict(type=10,config=17)]
        self.assertTrue(p.cycles_attributes_valid([good],identities))
        for change in ({'sample_type':78119},{'type':1},{'frequency':499},{'user_regs_mask':'0x0'},{'stack_user':4096}):
            self.assertFalse(p.cycles_attributes_valid([{**good,**change}],identities))

    def test_raw_cpu_and_task_records_use_original_client_bounds(self):
        with tempfile.TemporaryDirectory() as tmp:
            def sample(when,cpu):
                payload=struct.pack('<QQIIQIIQ',1,0x1234,777,778,when,cpu,0,12345)+struct.pack('<QQ',0,2)+bytes(264)+struct.pack('<Q',0)
                return struct.pack('<IHH',9,2,len(payload)+8)+payload
            def task(kind,when):return struct.pack('<IHH4IQ',kind,0,32,777,777,779,778,when)
            data=sample(200,3)+sample(600,4)+task(7,250)+task(4,300)
            header=bytearray(104);header[:8]=b'PERFILE2';struct.pack_into('<6Q',header,8,104,144,104,144,256,len(data))
            attr=bytearray(144);struct.pack_into('<IIQQQ',attr,0,10,128,17,999,78247);struct.pack_into('<QQ',attr,128,248,8)
            file=Path(tmp)/'perf.data';file.write_bytes(header+attr+struct.pack('<Q',1)+data)
            result=p.raw_cpu_threads(file,100,400,777,[dict(type=10,config=17)])
            self.assertEqual(result['sample_count'],1);self.assertEqual(result['sampled_tids'],[778])
            self.assertEqual(result['running_sample_CPU_TID'][0]['cpu'],3);self.assertEqual(result['created_tids'],[779]);self.assertEqual(result['exited_tids'],[779])
            with self.assertRaises(ValueError):p.raw_cpu_threads(file,100,400,778,[dict(type=10,config=17)])


if __name__=='__main__':unittest.main()
