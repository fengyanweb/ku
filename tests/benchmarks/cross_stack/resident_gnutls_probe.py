"""Optional existing-owner-mode dependency-init discriminator; never a product setting."""
from __future__ import annotations
import copy
import json
import os
from pathlib import Path
import select
import shutil
import subprocess
import time
import resident_snapshot


def compact(value):
    groups={}
    for row in value['records']:
        key=(row['callsite_file'],row.get('asn1_call',0),row['requested_bytes'])
        group=groups.setdefault(key,dict(caller=key[0],asn1_call=key[1],object_bytes=key[2],count=0,requested_bytes=0))
        group['count']+=1;group['requested_bytes']+=row['requested_bytes']
    return dict(status=value['status'],executable=value['executable'],modules=value['modules'],
        asn1_provenance=value.get('asn1_provenance'),groups=list(groups.values()),
        total_confirmed_stopped_ns=value['total_confirmed_stopped_ns'],
        scope='Live-object provenance/requested bytes only; not normal service RSS/PSS/cgroup saving or complete untracked allocation ownership.')


def tiny_case(binary,library,output,explicit,ffi,uid,gid,expected_dependencies):
    output.mkdir();env=dict(os.environ,LD_PRELOAD=str(library))
    # 3.8.3 also honors this deprecated alias in its constructor/destructor.
    for key in ('GNUTLS_NO_IMPLICIT_INIT','GNUTLS_NO_EXPLICIT_INIT'):env.pop(key,None)
    if explicit:env['GNUTLS_NO_IMPLICIT_INIT']='1'
    rows=[];snapshots=[];command=[str(binary)]+(['ffi-without-explicit-init'] if ffi else [])
    with (output/'stderr.log').open('xb') as errors:
        child=subprocess.Popen(command,env=env,stdin=subprocess.PIPE,stdout=subprocess.PIPE,stderr=errors,
            user=uid,group=gid)
        deadline=time.monotonic()+25;pending=b''
        try:
            for stage in range(2):
                while True:
                    while b'\n' not in pending:
                        left=deadline-time.monotonic()
                        if left<=0 or not select.select([child.stdout],[],[],min(left,5))[0]:raise TimeoutError('tiny LDAP stage deadline')
                        data=os.read(child.stdout.fileno(),4096)
                        if not data:raise RuntimeError('tiny LDAP exited before checkpoint')
                        pending+=data
                        if len(pending)>16384:raise RuntimeError('tiny LDAP output bound')
                    line,pending=pending.split(b'\n',1);row=json.loads(line);rows.append(row)
                    if 'stage' in row:break
                resident_snapshot.capture(child.pid,library,output/str(stage))
                value=json.loads((output/str(stage)/'live.json').read_text())
                if not value['live_table_valid'] or not value.get('asn1_provenance',{}).get('valid'):
                    raise RuntimeError('tiny LDAP invalid observer table')
                if any(value['modules'].get(path,{}).get('sha256')!=digest for path,digest in expected_dependencies.items()):
                    raise RuntimeError('tiny LDAP dependency differs from actual normal service')
                snapshots.append(compact(value));child.stdin.write(b'x');child.stdin.flush()
            code=child.wait(timeout=max(.1,deadline-time.monotonic()))
            if code:raise RuntimeError('tiny LDAP natural exit='+str(code))
            result=dict(complete=True,explicit_init_env=explicit,ffi_without_explicit_init=ffi,rows=rows,snapshots=snapshots,
                natural_exit=True,scope='Context/session/priority and direct-FFI availability discriminator only; no LDAP server, authentication or full TLS handshake is claimed.')
            (output/'tiny-summary.json').write_text(json.dumps(result,separators=(',',':'))+'\n')
            return dict(path=str(output/'tiny-summary.json'),complete=True,natural_exit=True)
        except Exception as error:
            (output/'tiny-summary.json').write_text(json.dumps(dict(complete=False,error=repr(error),
                explicit_init_env=explicit,ffi_without_explicit_init=ffi,rows=rows,snapshots=snapshots,
                returncode=child.poll(),scope='Incomplete discriminator; no capability-preservation inference.'),separators=(',',':'))+'\n')
            raise
        finally:
            if child.poll() is None:child.kill();child.wait(timeout=3)
            child.stdin.close();child.stdout.close()


def run(a,state,h):
    if os.geteuid()!=0:raise ValueError('dependency-init observer requires root cgroup control')
    if any(key in os.environ for key in ('GNUTLS_NO_IMPLICIT_INIT','GNUTLS_NO_EXPLICIT_INIT')):
        raise ValueError('dependency-init comparison requires an uncontaminated coordinator environment')
    plan=h.load(a.root/'frozen-plan.json')
    if plan!=h.memory_owner_plan(plan.get('versions',{}),'gnutls-explicit-init'):raise ValueError('matching dependency-init owner plan required')
    bindings={name:h.fc.version_identity(plan,name) for name in ('B','M','E')}
    if len({plan['versions'][name]['binary_sha256'] for name in ('B','M','E')})!=1:raise ValueError('normal ELF differs')
    library=Path(plan['versions']['M']['artifacts']['observer-library']['path'])
    binary=Path(plan['versions']['M']['artifacts']['ldap-public-init']['path'])
    output=a.root/'memory-owner-probe';output.mkdir();fixture=h.db.Database(a.root/'fixture',state['uid'],state['gid'])
    for name in ('owner-selftest.json','owner-compile.json','ldap-compile.json'):
        shutil.copyfile(a.root/'M'/name,output/name)
    original=h.fc.Harness;private=None;captured={};result=dict(execution_complete=False,diagnostic_only=True,
        normal_score_eligible=False,formal_qualified=False,candidate_accepted=False,normal_ELF_unchanged=True,
        memory_owner_variant='gnutls-explicit-init',rows=[],tiny_cases=[],goal_status='blocked',
        scope='Same normal ELF, exec-before-load environment discriminator only. Observer memory/CPU are unranked; no product setting or capability-preservation claim.')
    class Harness(original):
        def __init__(self,*args,**kwargs):
            super().__init__(*args,**kwargs);self.manifest=copy.deepcopy(self.manifest)
            self.manifest['db_url']=private['db_url'];self.db_domain=h.ExternalDomain(private['db_cgroup'])
        def endpoint(self,service,label):
            arm=self.root.name;target=output/arm/label;target.parent.mkdir(exist_ok=True)
            resident_snapshot.capture(service.process.pid,library,target);value=h.load(target/'live.json')
            if (not value['live_table_valid'] or not value.get('asn1_provenance',{}).get('valid')
                    or value['executable']['sha256']!=plan['versions']['B']['binary_sha256']):
                raise ValueError('invalid ASN1 provenance or changed normal ELF')
            if any('error' in module for module in value['modules'].values()):
                raise ValueError('actual dependency module identity unavailable')
            if arm=='M' and not value['asn1_provenance']['calls']:
                raise ValueError('baseline ASN1 wrapper was not exercised; no origin inference allowed')
            captured[arm+'-'+label]=compact(value)
        def start(self,*args,**kwargs):
            service=super().start(*args,**kwargs)
            try:self.endpoint(service,'startup');return service
            except BaseException:service.stop();raise
        def client(self,service,workload,rate,duration,path,**kwargs):
            report=super().client(service,workload,rate,duration,path,**kwargs)
            if Path(path).name=='score':self.endpoint(service,'after')
            return report
    try:
        fixture.start();private=h.load(fixture.manifest_path);h.fc.Harness=Harness
        for arm in plan['order']:
            row=h.fc.window(plan,arm,'json_object',500,3,5,output/arm/'window',binding=bindings[arm],job={'phase':'diagnostic'})
            for key in ('cpu_us','full_cpu_us','memory','qualification','upper_point','overload_boundary'):row.pop(key,None)
            row.update(diagnostic_only=True,normal_score_eligible=False,formal_qualified=False);result['rows'].append(row)
            if not (row.get('correct')==2500 and row.get('all_correct') is True and row.get('functional_valid') is True
                    and row.get('accounting_valid') is True and row.get('arrival_schedule_quality')=='pass' and row.get('natural_reclamation') is True):
                raise ValueError('dependency-init normal window invalid; no replacement')
        tiny=output/'tiny';tiny.mkdir()
        # Ku can dlopen unrelated native libraries such as libm; the tiny C
        # intentionally exercises only this dependency-initialization chain.
        required=('libpq.so.5','libldap.so.2','libgnutls.so.30','libtasn1.so.6','libp11-kit.so.0')
        modules=captured['M-startup']['modules']
        dependencies={path:identity['sha256'] for path,identity in modules.items()
                      if any(Path(path).name==name or Path(path).name.startswith(name+'.') for name in required)}
        if any(not any(Path(path).name==name or Path(path).name.startswith(name+'.') for path in dependencies) for name in required):
            raise ValueError('normal dependency-init chain missing required mapped module')
        for explicit in (False,True):
            for ffi in (False,True):
                result['tiny_cases'].append(tiny_case(binary,library,tiny/(('explicit' if explicit else 'default')+('-ffi' if ffi else '-ldap')),explicit,ffi,state['uid'],state['gid'],dependencies))
        if any(h.fc.version_identity(plan,name)!=bindings[name] for name in bindings):raise ValueError('owner artifact changed')
        result['execution_complete']=len(captured)==4
        if result['execution_complete']:result['goal_status']='unknown'
    except Exception as error:result['observer_error']=repr(error)
    finally:
        h.fc.Harness=original
        try:result['database_cleanup']=fixture.stop()
        except Exception as error:result['database_cleanup']={'error':repr(error)}
        cleanup=result['database_cleanup']
        if cleanup.get('forced') is not False or cleanup.get('cgroup_removed') is not True or cleanup.get('after',{}).get('process_count')!=0:result['execution_complete']=False
        h.save(output/'summary.json',result);h.save(output/'live-owner-map.json',captured)
        selected=[p for p in output.rglob('*') if p.is_file() and p.name in ('summary.json','live-owner-map.json','tiny-summary.json','stderr.log','window.json','client.json','driver-cleanup.json','service.log','termination.json','owner-selftest.json','owner-compile.json','ldap-compile.json')]
        selected.extend(p for arm in ('M','E') for p in (output/arm).rglob('*') if p.is_file() and p.name in ('maps.log','smaps.log','live.json'))
        result['small_artifact']=h.pg_failure_export(a,output,output_name='memory-owner-observation',schema='ku-memory-owner-gnutls-small-v1',selected=selected)
        h.save(a.root/'hosted-state.json',state)
    return result
