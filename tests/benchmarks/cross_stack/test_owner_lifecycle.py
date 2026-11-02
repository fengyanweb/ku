"""Pure owner-lifecycle wiring: never compile, launch or inspect live procfs."""
import hashlib, io, json, tarfile, tempfile, types, unittest
from pathlib import Path
from unittest.mock import patch
import owner_lifecycle as owner
import reuse_build as reuse

class OwnerLifecycleTests(unittest.TestCase):
    def test_correct_requests_do_not_hide_failed_driver_cleanup(self):
        report=dict(valid_driver=True,ledger=dict(results={'correct':3},planned=3,attempted=3,sent=3,terminal=3,correct_total=3,planned_reconciled=True),service_cost=dict(driver_cgroup_removed=True),assessment=dict(arrival_schedule_quality='pass'))
        owner.check_report(report,3)
        for value in (False,None,1):
            with self.subTest(cleanup=value):
                report['service_cost']['driver_cgroup_removed']=value
                with self.assertRaisesRegex(ValueError,'driver cleanup'):owner.check_report(report,3)
        report['service_cost']['driver_cgroup_removed']=True
        for value in ('fail','unknown',None):
            report['assessment']['arrival_schedule_quality']=value
            with self.assertRaises(ValueError):owner.check_report(report,3)

    def test_archive_only_keeps_fixed_artifacts_and_redacted_diagnostic_records(self):
        with tempfile.TemporaryDirectory() as temp:
            root=Path(temp)
            for name,data in {'M/owner.c':b'normal source','M/bin/ku-owner':b'ELF','owner-lifecycle/ready.smaps':b'raw pages','owner-lifecycle/ready.json':b'{"db_url":"private","pid":123}','owner-lifecycle/harness/config.json':b'private','B/build/ku-cargo/release/ku':b'CLI-cache','fixture/database/data/private':b'private'}.items():
                p=root/name;p.parent.mkdir(parents=True,exist_ok=True);p.write_bytes(data)
            result=owner.archive(root)
            with tarfile.open(result['path']) as t:
                self.assertIn('owner-lifecycle/ready.smaps',t.getnames())
                self.assertIn('M/bin/ku-owner',t.getnames())
                self.assertNotIn('db_url',json.load(t.extractfile('owner-lifecycle/ready.json')))
                self.assertFalse(any('config.json' in n or 'ku-cargo' in n or n.startswith('fixture/') for n in t.getnames()))

    def test_plan_is_one_derived_service_three_snapshots_and_no_score(self):
        plan=owner.plan({'B':{'id':'normal'},'M':{'id':'derived'}})
        self.assertEqual((plan['workload'],plan['rate'],plan['warm_seconds'],plan['steady_seconds']),('db',200,10,15))
        self.assertEqual(plan['stages'],['ready','warm','steady'])
        self.assertFalse(plan['normal_score_eligible']);self.assertFalse(plan['formal_qualified'])
        self.assertEqual(plan['maximum_replacements'],0)
        self.assertEqual(plan['archive_sha256'],reuse.OK_PMU_ARCHIVE_SHA)

    def test_reuse_restores_only_fixed_builds_not_old_windows_or_preflight(self):
        with tempfile.TemporaryDirectory() as temp:
            root=Path(temp);archive=root/'source.tgz';destination=root/'fresh'
            state=dict(identity=dict(candidate=reuse.h.NORMAL_OK_PRODUCT,build_set='diagnostic',normal_self_probe=True,normal_self_workload='ok-gpr'),steps={n:{'state':'completed'} for n in ['build','build-B','build-Driver','diagnostic']})
            files={'B/build/normal':b'normal','Driver/build/driver':b'driver','normal-self-probe/results.json':b'old windows','normal-perf-preflight/preflight.json':b'old boot','hosted-state.json':json.dumps(state).encode()}
            index={'path_root':reuse.TOKEN,'raw_and_redacted_hashes_distinct':True,'files':[{'path':name,'bytes':len(data),'original_sha256':hashlib.sha256(data).hexdigest(),'archived_sha256':hashlib.sha256(data).hexdigest()} for name,data in files.items()]}
            with tarfile.open(archive,'w:gz') as tar:
                for name,data in dict(files,**{'portable-file-map.json':json.dumps(index).encode()}).items():
                    entry=tarfile.TarInfo(name);entry.size=len(data);tar.addfile(entry,io.BytesIO(data))
            sha=hashlib.sha256(archive.read_bytes()).hexdigest()
            with patch.object(reuse,'OK_PMU_ARCHIVE_SHA',sha):reuse.extract_builds(archive,destination,sha,{'B','Driver'},owner_lifecycle_observation=True)
            self.assertEqual((destination/'B/build/normal').read_bytes(),b'normal')
            self.assertFalse((destination/'normal-self-probe').exists());self.assertFalse((destination/'normal-perf-preflight').exists())
            receipt=json.loads((destination/'reuse-byte-verification.json').read_text())
            self.assertEqual(receipt['imported_measurement_windows'],0)

    def test_new_reuse_mode_is_exclusive(self):
        with patch.object(reuse,'checked'),self.assertRaisesRegex(ValueError,'one explicit'):
            reuse.extract_builds('none','none','a'*64,{'B','Driver'},ok_pmu_observation=True,owner_lifecycle_observation=True)

    def test_sampler_uses_existing_thread_and_attempts_once_even_on_error(self):
        class Base:
            def __init__(self):self.samples=0
            def sample(self):self.samples+=1
        calls=[]
        def observe():calls.append(1);raise ValueError('capture denied')
        with patch.object(owner.time,'monotonic',side_effect=[0,7,8,9]):
            cls=owner.sampler_class(Base,observe);sampler=cls();sampler.sample();sampler.sample();sampler.sample()
        self.assertEqual(calls,[1]);self.assertEqual(sampler.samples,3)
        self.assertTrue(sampler.owner_attempted);self.assertIn('capture denied',sampler.owner_error)
        self.assertEqual(cls.instances,[sampler])

    def test_capture_keeps_same_pid_and_no_pfn_exports(self):
        state='(service) '+' '.join(['x']*19+['77'])
        with tempfile.TemporaryDirectory() as temp,patch.object(owner.cost,'read_text',side_effect=lambda p: 'PPid:\t9\n' if p.name=='status' else state),patch.object(owner.h,'sha',return_value='a'*64),patch.object(owner.h,'diagnostic_memory',return_value={'complete':True,'mappings':[]}),patch.object(owner.cost,'allocation_owner_ranges',return_value={'complete':True,'records':[]}),patch.object(owner.cost,'allocation_owner_pages',return_value={'available':False,'PFNs_exported':False,'unavailable_reason':'permission'}):
            result=owner.capture(123,'77','a'*64,'private.log',Path(temp)/'ready.json',9)
            self.assertEqual(result['pid'],123);self.assertFalse(result['pages']['PFNs_exported'])
            self.assertFalse(result['normal_score_eligible']);self.assertFalse(result['pages']['available'])

    def test_capture_rejects_different_pid_generation_before_observation(self):
        state='(service) '+' '.join(['x']*19+['78'])
        with patch.object(owner.cost,'read_text',side_effect=lambda p:'PPid:\t9\n' if p.name=='status' else state),patch.object(owner.h,'diagnostic_memory') as memory,self.assertRaisesRegex(ValueError,'identity changed'):
            owner.capture(123,'77','a'*64,'private.log','ready.json',9)
        memory.assert_not_called()

if __name__=='__main__':unittest.main()
