"""Private opt-in library A/B guard; reuses hosted memory9 without new workload.

No command in this module starts a process. The hosted runner performs the
existing serial lifecycle only when explicitly invoked on Linux ARM64.
"""
import copy
import hashlib
import json
import os
from pathlib import Path

SOURCE='5b6b41138cce4086c19cc7daa24e820c9b6513d8'
C_SHA='830536089f72ce56118e4e6a1556bbe007f816cc16a3c2f96c0dcff010d14e77'
ELF_SHA='234300bba9b6fce603a178701ac289a82108398d5f35de3c0a7ad585f082367b'
OPTIONAL=('libldap.so.2.0.200','liblber.so.2.0.200','libsasl2.so.2.0.25',
    'libgnutls.so.30.37.1','libp11-kit.so.0.3.1','libidn2.so.0.4.0',
    'libunistring.so.5.0.0','libtasn1.so.6.6.3','libnettle.so.8.8',
    'libhogweed.so.6.8','libgmp.so.10.5.0','libffi.so.8.1.4')


def digest(path):return hashlib.sha256(Path(path).read_bytes()).hexdigest()


def reused_owner(root):
    owners=[{key:json.loads((Path(root)/arm/'manifest.json').read_text())[key] for key in ('uid','gid')} for arm in ('A','B')]
    if owners[0]!=owners[1] or any(type(value)is not int or value<=0 for value in owners[0].values()):
        raise ValueError('same ordinary uid/gid required for reused inputs')
    return owners[0]


def validate_selection(a,build_set):
    if (not getattr(a,'http_short_screen',False) or build_set!='diagnostic'
            or getattr(a,'http_short_goal',None)!='memory-probe'
            or getattr(a,'phase','diagnostic') not in ('diagnostic','archive')
            or a.prepatch!=SOURCE or a.candidate!=SOURCE):
        raise ValueError('loader screen requires explicit memory-probe diagnostic/archive, same original source5b and prepared same-ELF manifests')


def frozen_plan(versions,libraries):
    import hosted_cycle as h
    if set(libraries)!={'A','B'}:raise ValueError('exact A/B library bindings required')
    for arm,item in libraries.items():
        if set(item)!={'path','sha256','build_record','build_record_sha256','functional_gate','functional_gate_sha256',
                'source_build_identity','source_build_identity_sha256'}:
            raise ValueError('library and functional-gate identity required: '+arm)
        for field in ('sha256','build_record_sha256','functional_gate_sha256','source_build_identity_sha256'):
            value=item[field]
            if not isinstance(value,str) or len(value)!=64 or any(c not in '0123456789abcdef' for c in value):
                raise ValueError('full frozen SHA256 required: '+arm+':'+field)
    plan=h.http_short_plan(versions,'memory-probe')
    plan.update(prepatch=SOURCE,libpq_loader=copy.deepcopy(libraries),
        purpose='Same original source5b normal ELF with current-default full-C byte equality. '
            'Only precise same-source/configuration eagerA versus lazyB libpq paths differ. '
            'Three eagerA environment controls then AB/BA/AB; original memory/CPU/arrival/cleanup gates. '
            'DistroD is not a scored arm. No automatic adoption, retry, replacement, owner/smaps probe or expanded matrix.')
    return plan


def validate_bindings(plan,bindings):
    """Require actual normal inputs; sparse archive receipts never replace files."""
    if set(plan['versions'])!={'A','B'}:raise ValueError('only A/B arms allowed')
    if os.environ.get('LD_PRELOAD') or os.environ.get('LD_AUDIT'):
        raise ValueError('scored loader screen cannot inherit loader instrumentation')
    records=[]
    for arm in ('A','B'):
        binding=bindings[arm];spec=plan['versions'][arm];item=plan['libpq_loader'][arm]
        if (binding['source_commit']!=SOURCE or binding['files']['ELF']['sha256']!=ELF_SHA
                or binding['files']['generated C']['sha256']!=C_SHA):
            raise ValueError('actual original normal ELF/full C required')
        for field,expected in [('path','sha256'),('build_record','build_record_sha256'),('functional_gate','functional_gate_sha256'),
                ('source_build_identity','source_build_identity_sha256')]:
            if digest(item[field])!=item[expected]:raise ValueError('loader artifact changed: '+arm+':'+field)
        library=Path(item['path']).resolve(strict=True)
        runtime=binding['files'].get('declared:runtime libpq',{})
        if runtime.get('path')!=str(library) or runtime.get('sha256')!=item['sha256']:
            raise ValueError('plan does not bind selected runtime libpq: '+arm)
        alias=library.parent/'libpq.so.5'
        if alias.resolve(strict=True)!=library:raise ValueError('exact SONAME alias required')
        manifest=json.loads((Path(spec['root'])/'manifest.json').read_text())
        wanted={'LD_LIBRARY_PATH':str(library.parent)}
        if manifest['stacks']['ku']['env']!=wanted or spec.get('env',{}) not in ({},wanted):
            raise ValueError('only the exact libpq search directory may vary')
        if manifest.get('libpq_loader_arm')!=arm:raise ValueError('explicit manifest arm required')
        record=json.loads(Path(item['build_record']).read_text())
        if record['shared_sha256']!=item['sha256']:raise ValueError('build record does not describe selected shared library')
        records.append(record)
        needed=set(record['DT_NEEDED'])
        required={'libssl.so.3','libcrypto.so.3','libgssapi_krb5.so.2'}
        if not required<=needed or ('libldap.so.2' in needed)!=(arm=='A'):
            raise ValueError('SSL/GSS or eager/lazy linkage contract differs')
    if plan['libpq_loader']['A']['source_build_identity_sha256']!=plan['libpq_loader']['B']['source_build_identity_sha256']:
        raise ValueError('single common source/compiler/configuration identity required')
    for field in ('configure_flags','exports','pkg_config_static_tokens'):
        if records[0][field]!=records[1][field]:raise ValueError('A/B library contract differs: '+field)
    if set(records[0]['DT_NEEDED'])-{'libldap.so.2'}!=set(records[1]['DT_NEEDED']):
        raise ValueError('only LDAP direct dependency may differ')
    for key in ('ELF','generated C','expanded source','Ku CLI','driver ELF'):
        if bindings['A']['files'][key]['sha256']!=bindings['B']['files'][key]['sha256']:
            raise ValueError('non-library input differs: '+key)
    if bindings['A']['build_mode']!=bindings['B']['build_mode']:raise ValueError('build modes differ')
    manifests=[json.loads((Path(plan['versions'][arm]['root'])/'manifest.json').read_text()) for arm in ('A','B')]
    for key in ('service_env','quality_policy','driver_cores','driver_cpuset','client','protocol'):
        if manifests[0].get(key)!=manifests[1].get(key):raise ValueError('non-library harness contract differs: '+key)


def check_maps(raw,arm,selected):
    if len(raw)>1<<20:raise ValueError('maps exceeds bounded 1MiB observation')
    paths={line.split(maxsplit=5)[-1] for line in raw.decode().splitlines() if len(line.split(maxsplit=5))==6}
    pq=[p for p in paths if Path(p).name.startswith('libpq.so')]
    if pq!=[str(Path(selected).resolve(strict=True))]:raise ValueError('service did not map exactly the selected libpq')
    names={Path(p).name for p in paths}
    if not {'libssl.so.3','libcrypto.so.3','libgssapi_krb5.so.2.2'}<=names:
        raise ValueError('actual SSL/GSS mappings missing')
    optional=set(OPTIONAL)&names
    if (arm=='A' and optional!=set(OPTIONAL)) or (arm=='B' and optional):
        raise ValueError('actual LDAP closure contradicts selected arm')
    return dict(mapped_libpq=pq[0],optional_closure_present=sorted(optional),optional_closure_count=len(optional),
        retained_SSL_GSS=True,scope='One warmup-before maps checkpoint, not resident bytes or sampled CPU')


def capture_maps(plan,manifest,service,output):
    arm=manifest.get('libpq_loader_arm');item=plan['libpq_loader'][arm]
    actual=digest(item['path'])
    if actual!=item['sha256']:raise ValueError('runtime libpq changed before warmup')
    with Path('/proc',str(service.process.pid),'maps').open('rb') as stream:raw=stream.read((1<<20)+1)
    receipt=check_maps(raw,arm,item['path'])
    if service.startup['environment'].get('LD_LIBRARY_PATH')!=str(Path(item['path']).resolve().parent):
        raise ValueError('actual service library environment differs')
    output=Path(output);rawpath=output.with_suffix('.maps');rawpath.write_bytes(raw)
    receipt.update(pid=service.process.pid,arm=arm,raw_path=str(rawpath),raw_sha256=digest(rawpath),
        selected_libpq_sha256=actual,timing='after startup, before warmup, before score CPU origin')
    output.write_text(json.dumps(receipt,sort_keys=True,indent=2)+'\n')


def prepare(original_root,output,libraries,current_c_proof,current_c):
    """Only stage/hash inputs; preserve original generator/link receipts and ELF."""
    import focus_cycle as fc
    original_root=Path(original_root).resolve(strict=True);output=Path(output).resolve()
    if output.exists():raise ValueError('new independent output required; never overwrite a screen')
    proof=Path(current_c_proof).resolve(strict=True)
    receipt=json.loads(proof.read_text())
    if receipt.get('byte_equal') is not True or receipt['current']['sha256']!=C_SHA:
        raise ValueError('actual current-default complete C equality proof required')
    emitted=Path(current_c).resolve(strict=True)
    if digest(emitted)!=C_SHA:raise ValueError('current complete C changed')
    original_manifest=original_root/'B/manifest.json'
    base=json.loads(original_manifest.read_text().replace('${EVIDENCE_ROOT}',str(original_root)))
    base['cgroup_root']='/sys/fs/cgroup/ku-loader-'+hashlib.sha256(str(output).encode()).hexdigest()[:12]
    # The compile-time libpq remains distro D; never rewrite its hash as A/B.
    build=base['stacks']['ku']['build']
    old_source=build['libpq_source']
    build['libpq_source']=str(original_root/'B/build/libpq/libpq.so')
    build['reused_original_link_library_path']=old_source
    versions={}
    for arm,item in libraries.items():
        directory=output/arm;directory.mkdir(parents=True)
        manifest=copy.deepcopy(base);manifest['libpq_loader_arm']=arm
        manifest['stacks']['ku']['env']={'LD_LIBRARY_PATH':str(Path(item['path']).resolve(strict=True).parent)}
        (directory/'manifest.json').write_text(json.dumps(manifest,sort_keys=True,indent=2)+'\n')
        artifacts={'runtime libpq':{'path':item['path'],'sha256':item['sha256']},
            'current complete C proof':{'path':str(proof),'sha256':digest(proof)},
            'current complete C':{'path':str(emitted),'sha256':C_SHA}}
        for name,key in [('build record','build_record'),('functional gate','functional_gate'),('source/build identity','source_build_identity')]:
            artifacts[name]={'path':item[key],'sha256':item[key+'_sha256']}
        versions[arm]=dict(root=str(directory),stack='ku',source_commit=SOURCE,binary_sha256=ELF_SHA,artifacts=artifacts)
    plan=frozen_plan(versions,libraries)
    bindings={arm:fc.version_identity(plan,arm) for arm in ('A','B')}
    validate_bindings(plan,bindings)
    (output/'frozen-plan.json').write_text(json.dumps(plan,sort_keys=True,indent=2)+'\n')
    fixture=copy.deepcopy(base);fixture['stacks']={}
    (output/'fixture').mkdir();(output/'fixture/manifest.json').write_text(json.dumps(fixture,sort_keys=True,indent=2)+'\n')
    return plan


if __name__=='__main__':
    import argparse
    parser=argparse.ArgumentParser(description='Prepare only; never launches a process')
    parser.add_argument('--original-root',required=True,type=Path)
    parser.add_argument('--output',required=True,type=Path)
    parser.add_argument('--libraries',required=True,type=Path)
    parser.add_argument('--current-c-proof',required=True,type=Path)
    parser.add_argument('--current-c',required=True,type=Path,help='Actual copied full current C; original proof keeps its original path identity')
    args=parser.parse_args()
    plan=prepare(args.original_root,args.output,json.loads(args.libraries.read_text()),args.current_c_proof,args.current_c)
    print(json.dumps(dict(prepared=True,windows=len(plan['windows']),services_started=0,performance_measured=False)))
