#!/usr/bin/env python3
"""Reuse verified, unmeasured ARM64 builds; never import an old measurement."""
from __future__ import annotations
import argparse, copy, hashlib, json, os
from pathlib import Path, PurePosixPath
import platform, re, shutil, signal, subprocess, tarfile, tempfile
import hosted_cycle as h
from compile_variants import Commands, clean_environment

MAX_FILES=20000
MAX_FILE=128<<20
MAX_TOTAL=768<<20
TOKEN='${EVIDENCE_ROOT}'


def checked(path, expected):
    if not re.fullmatch('[0-9a-f]{64}',expected or '') or h.sha(path)!=expected:
        raise ValueError('reused file identity differs: '+str(path))


def translated(value, root):
    if isinstance(value,dict):return {k:translated(v,root) for k,v in value.items()}
    if isinstance(value,list):return [translated(v,root) for v in value]
    if isinstance(value,str):return value.replace(TOKEN,str(root))
    return value


def extract_builds(archive, root, expected_sha256, names):
    """Check all paths/map entries first; write only into a fresh owned tree."""
    checked(archive,expected_sha256)
    root=Path(root)
    if root.exists() and (root.is_symlink() or any(p.name!='tools' for p in root.iterdir())):
        raise ValueError('reuse requires a fresh output (only prepared tools may exist)')
    with tarfile.open(archive,'r:gz') as tar:
        members=tar.getmembers()
        if len(members)>MAX_FILES:raise ValueError('archive member limit')
        seen=set();total=0
        for member in members:
            path=PurePosixPath(member.name)
            if (not path.parts or path.is_absolute() or '..' in path.parts
                    or '\\' in member.name or member.name in seen
                    or not member.isfile() or not 0<=member.size<=MAX_FILE):
                raise ValueError('unsupported archive member: '+member.name)
            seen.add(member.name);total+=member.size
        if total>MAX_TOTAL:raise ValueError('archive expanded byte limit')
        index=json.load(tar.extractfile('portable-file-map.json'))
        if index.get('path_root')!=TOKEN or index.get('raw_and_redacted_hashes_distinct') is not True:
            raise ValueError('portable file provenance absent')
        entries=index.get('files',[])
        if len(entries)>MAX_FILES:raise ValueError('portable map entry limit')
        mapping={entry['path']:entry for entry in entries}
        if len(mapping)!=len(entries):raise ValueError('duplicate portable map entry')
        old=json.load(tar.extractfile('hosted-state.json'))
        if any(not name.startswith('build') for name in old.get('steps',{})):
            raise ValueError('this reuse path accepts build-only source runs')
        if any(old.get('steps',{}).get('build-'+name,{}).get('state')!='completed' for name in names):
            raise ValueError('requested normal build was not completed')
        selected=[m for m in members if PurePosixPath(m.name).parts[0] in names]
        root.mkdir(parents=True,exist_ok=True)
        for member in selected:
            entry=mapping.get(member.name,{})
            data=tar.extractfile(member).read()
            if entry.get('bytes')!=len(data) or hashlib.sha256(data).hexdigest()!=entry.get('archived_sha256'):
                raise ValueError('portable archived bytes differ: '+member.name)
            destination=root/member.name;destination.parent.mkdir(parents=True,exist_ok=True)
            with destination.open('xb') as stream:stream.write(data)
            destination.chmod(member.mode&0o777)
        h.save(root/'reuse-portable-file-map.json',index)
        h.save(root/'reuse-original-state.json',old)
        return mapping


def restore_json_inputs(source, root, commits, mapping):
    """Recover immutable input bytes, never normalize a corpus or its contract."""
    prefix='tests/benchmarks/cross_stack/'
    records=[]
    for name,commit in commits.items():
        start=name+'/src/'+prefix
        for path,entry in sorted(mapping.items()):
            if not path.startswith(start) or not path.endswith('.json'):continue
            target=root/path;checked(target,entry['archived_sha256'])
            if entry['archived_sha256']!=entry['original_sha256']:
                git_path=path[len(name+'/src/'):]
                raw=subprocess.check_output(['git','-C',str(source),'show',commit+':'+git_path],timeout=10)
                if len(raw)>MAX_FILE or hashlib.sha256(raw).hexdigest()!=entry['original_sha256']:
                    raise ValueError('immutable JSON input differs from original build: '+path)
                target.write_bytes(raw)
            checked(target,entry['original_sha256'])
            records.append(dict(path=path,source_commit=commit,archived_sha256=entry['archived_sha256'],
                original_sha256=entry['original_sha256'],recovered_sha256=h.sha(target),
                archived_bytes=entry['bytes'],recovered_bytes=target.stat().st_size))
    h.save(root/'reuse-json-input-recovery.json',dict(files=records,
        scope='source JSON only; git-committed bytes restored without changing protocol expectations or compiled artifacts'))


def validate_protocol_inputs(root, name, mapping):
    base=root/name/'src/tests/benchmarks/cross_stack';path=base/'protocol.json'
    checked(path,mapping[path.relative_to(root).as_posix()]['original_sha256'])
    protocol=h.load(path)
    if protocol.get('schema')!=1 or not {'object','array','escape'}<=protocol.get('corpora',{}).keys():
        raise ValueError('frozen corpus protocol is incomplete')
    records={}
    for key,spec in protocol['corpora'].items():
        relative=PurePosixPath(spec['path'])
        if relative.is_absolute() or '..' in relative.parts or '\\' in spec['path']:
            raise ValueError('corpus path is outside frozen inputs')
        corpus=base/relative;checked(corpus,spec['sha256'])
        if type(spec.get('bytes')) is not int or corpus.stat().st_size!=spec['bytes']:
            raise ValueError('frozen corpus byte count differs: '+key)
        value=h.load(corpus)
        if set(value)!={'counter','data'} or value['counter']!=spec['counter']:
            raise ValueError('frozen corpus envelope differs: '+key)
        records[key]=dict(bytes=spec['bytes'],sha256=spec['sha256'])
    return dict(protocol_sha256=h.sha(path),corpora=records)


def require_elf(path):
    with Path(path).open('rb') as stream:header=stream.read(20)
    if len(header)!=20 or header[:6]!=b'\x7fELF\x02\x01' or int.from_bytes(header[18:20],'little')!=183:
        raise ValueError('reused artifact is not ELF64 little-endian AArch64: '+str(path))


def verify_manifest(root, name, commit, mapping, verify_system=True):
    path=root/name/'manifest.json';original=h.load(path);m=translated(original,root)
    receipt=h.load(m['build_receipt'])
    if (m.get('candidate_base')!=commit or receipt.get('candidate_base')!=commit
            or receipt.get('failure') is not None or not receipt.get('commands')
            or any(command.get('exit_code')!=0 for command in receipt['commands'])):
        raise ValueError('normal build commit/completion differs: '+name)
    if set(m['stacks'])!={'rust' if name=='R' else 'ku'}:raise ValueError('unexpected normal stack: '+name)
    stack=m['stacks']['rust' if name=='R' else 'ku'];build=stack['build']
    binary=Path(stack['command'][0]);require_elf(binary)
    if name=='R':
        checked(binary,build['binary_sha256'])
        source=Path(m['source_root'])/'tests/benchmarks/cross_stack/rust'
        for relative,digest in list(build['source_files_sha256'].items()):
            file=source/relative
            if relative=='README.md' and h.sha(file)!=digest:
                entry=mapping[file.relative_to(root).as_posix()]
                if entry['original_sha256']!=digest or any(b'README' in p.read_bytes() for p in source.rglob('*.rs')):
                    raise ValueError('redacted README is not an independent documentation input')
                checked(file,entry['archived_sha256'])
                build.setdefault('reused_document_redactions',{})[relative]=dict(original_sha256=digest,archived_sha256=entry['archived_sha256'])
                build['source_files_sha256'][relative]=entry['archived_sha256']
            else:checked(file,digest)
    else:
        if any(build.get(k)!=v for k,v in [('formatting_mode','helpers'),('json_response_mode','text'),('pg_execution_mode','async'),('pg_read_mode','int')]):
            raise ValueError('reused build is not the normal helpers/text/async/int contract')
        checked(build['cli'],build['cli_sha256']);require_elf(build['cli'])
        checked(build['libpq_source'] if verify_system else root/name/'build/libpq/libpq.so',build['libpq_sha256'])
        for kind,native in build['native'].items():
            checked(native['binary'],native['binary_sha256']);require_elf(native['binary'])
            checked(native['generated_c'],native['generated_c_sha256']);checked(native['source'],native['source_sha256'])
            invocation=native['compiler_invocation']
            if invocation['flags']!=['-std=c11','-O2',*(['-pthread'] if kind=='server' else [])]:
                raise ValueError('reused native compiler flags differ')
            if verify_system:
                checked(invocation['compiler']['path'],invocation['compiler']['sha256'])
                version=subprocess.check_output([invocation['compiler']['path'],'--version'],text=True,timeout=10).strip()
                if version!=m['versions']['cc']:raise ValueError('current compiler version differs from the frozen build')
            for library in invocation['library_inputs']:
                selected=library['selected_input'];checked(selected['real_path'],selected['sha256'])
            # JSON receipts were deliberately redacted during export. Bind the
            # original declaration to the dual-hash map, then bind actual bytes.
            receipt=Path(native['compiler_receipt']);entry=mapping[receipt.relative_to(root).as_posix()]
            if entry['original_sha256']!=native['compiler_receipt_sha256']:
                raise ValueError('original compiler receipt does not match portable provenance')
            checked(receipt,entry['archived_sha256'])
            native['original_compiler_receipt_sha256']=native['compiler_receipt_sha256']
            native['compiler_receipt_sha256']=entry['archived_sha256']
            native['compiler_receipt_scope']='archived redacted bytes; original declaration verified via portable file map'
    old_client=m['client'];checked(old_client['command'][0],old_client['sha256']);require_elf(old_client['command'][0])
    checked(root/'R/src/tests/benchmarks/cross_stack/client/main.go',old_client['source_sha256'])
    h.save(root/'reuse-provenance'/(name+'-manifest.json'),original)
    m['uid'],m['gid']=os.getuid(),os.getgid()
    m['source_inventory_scope']='original build inventory; portable tree is partial and documentation redactions are separately bound'
    m['reused_build']=dict(source_commit=commit,normal_artifact_bytes_unchanged=True,
        dependency_scope='selected libpq and compiler checked; complete dynamic dependency closure first recorded on this new host',
        imported_measurement_windows=0,original_manifest=str(root/'reuse-provenance'/(name+'-manifest.json')))
    h.save(path,m)
    return m


def new_driver(a):
    directory=a.source/'tests/benchmarks/cross_stack/client';out=a.root/'fresh-driver';out.mkdir()
    def sources():return {p.relative_to(directory).as_posix():h.sha(p) for p in directory.rglob('*')
        if p.is_file() and (p.suffix=='.go' or p.name in ('go.mod','go.sum'))}
    source_hashes=sources()
    environment=clean_environment(a.tools)
    environment.update(GOPROXY='off',GOSUMDB='off',GOMAXPROCS='2')
    go=a.tools/'go/bin/go';logs=Commands(out/'commands',environment)
    version=logs.run('version',[go,'version'],directory,timeout=30)
    if not re.fullmatch(r'go version go1\.27\.2 linux/arm64',version):raise ValueError('fixed Go ARM64 toolchain required')
    binary=out/'cross-stack-client'
    logs.run('build',[go,'build','-trimpath','-mod=readonly','-o',binary,'.'],directory,timeout=180)
    logs.run('contracts',[go,'test','./...'],directory,timeout=180)
    driver_input_preflight(directory,a.root/'R/src/tests/benchmarks/cross_stack/protocol.json',go,logs)
    if sources()!=source_hashes:raise ValueError('driver sources changed during preparation')
    client=dict(command=[str(binary)],cwd=str(a.root),env={'GOMAXPROCS':'1'},sha256=h.sha(binary),
        source_sha256=h.sha(directory/'main.go'),source_files_sha256=source_hashes,
        tool_checkout_commit=subprocess.check_output(['git','-C',str(a.source),'rev-parse','HEAD'],text=True,timeout=10).strip())
    h.save(out/'identity.json',dict(client=client,commands=logs.rows,scope='new common driver for all reused services; no old results imported'))
    m=h.load(a.root/'R/manifest.json');m['client']=client;h.save(a.root/'R/manifest.json',m)


def driver_input_preflight(directory, protocol, go, logs):
    """Run the real Go loader offline against the recovered input directory."""
    test='''package main
import ("encoding/json";"os";"testing")
func TestReusedProtocolInputBytes(t *testing.T) {
 c:=config{protocolPath:os.Getenv("KU_REUSED_PROTOCOL"),workload:"json_object"}
 if err:=load(&c);err!=nil {t.Fatal(err)}
 if c.protocolHash!=os.Getenv("KU_REUSED_PROTOCOL_SHA256") {t.Fatal("protocol SHA differs")}
 raw,err:=os.ReadFile(c.protocolPath);if err!=nil {t.Fatal(err)}
 var p struct {Corpora map[string]struct {Bytes int}}
 if err=json.Unmarshal(raw,&p);err!=nil {t.Fatal(err)}
 for _,name:=range []string{"object","array","escape"} {
  if len(c.corpora[name].bytes)!=p.Corpora[name].Bytes {t.Fatalf("corpus bytes differ: %s",name)}
 }
}
'''
    with tempfile.TemporaryDirectory(prefix='offline-inputs-') as temp:
        target=Path(temp)
        for file in directory.rglob('*'):
            if file.is_file() and (file.suffix=='.go' or file.name in ('go.mod','go.sum')):
                destination=target/file.relative_to(directory);destination.parent.mkdir(parents=True,exist_ok=True)
                shutil.copyfile(file,destination)
        (target/'reused_inputs_test.go').write_text(test)
        logs.run('recovered-inputs',[go,'test','-run','^TestReusedProtocolInputBytes$','-count=1','.'],target,
            env={'KU_REUSED_PROTOCOL':str(protocol),'KU_REUSED_PROTOCOL_SHA256':h.sha(protocol)},timeout=180)


def prepare(a):
    if os.geteuid()==0 or platform.system()!='Linux' or platform.machine() not in ('aarch64','arm64'):
        raise ValueError('ordinary benchmark user on a fresh Linux ARM64 host required')
    expected={'A':a.prepatch,**{f'C{i+1}':c for i,c in enumerate(a.screen_variants)},'B':a.candidate,'R':a.candidate}
    mapping=extract_builds(a.archive,a.root,a.archive_sha256,expected)
    restore_json_inputs(a.source,a.root,expected,mapping)
    input_checks={name:validate_protocol_inputs(a.root,name,mapping) for name in expected}
    h.save(a.root/'reuse-input-preflight.json',dict(versions=input_checks,network_requests=0))
    for name,commit in expected.items():
        verify_manifest(a.root,name,commit,mapping)
        (a.root/name/'tools').symlink_to(a.tools,target_is_directory=True)
    new_driver(a)
    identity=dict(baseline=a.baseline,candidate=a.candidate,entry_sha256=h.sha(h.__file__),
        build_set='followup-screen',prepatch=a.prepatch,screen_variants=a.screen_variants)
    state=dict(identity=identity,steps={},goal_status='unknown',uid=os.getuid(),gid=os.getgid(),
        reused_build=dict(archive_sha256=a.archive_sha256,source_run=a.source_run,
            product_commit=a.candidate,tool_checkout=str(a.source),normal_versions=expected,
            imported_measurement_windows=0,old_boot_or_runtime_state_imported=False,
            receipt_redaction='original compiler declarations checked against original/archived portable hashes'))
    h.save(a.root/'hosted-state.json',state)
    result=h.step(state,a.root,'build',lambda:h.complete_build(a,state,'followup-screen',a.prepatch,a.screen_variants))
    h.save(a.root/'reuse-build.json',dict(**state['reused_build'],complete=result['execution_complete']))
    return result


def main(argv=None):
    parser=argparse.ArgumentParser(description=__doc__)
    for key in ['archive','root','source','tools']:parser.add_argument('--'+key,required=True,type=Path)
    parser.add_argument('--archive-sha256',required=True);parser.add_argument('--source-run',required=True)
    parser.add_argument('--candidate',required=True);parser.add_argument('--prepatch',default=h.FOLLOWUP_PREPATCH)
    parser.add_argument('--baseline',default=h.BASELINE);parser.add_argument('--screen-variants',default='')
    a=parser.parse_args(argv)
    for key in ['archive','source','tools']:setattr(a,key,getattr(a,key).resolve(strict=True))
    a.root=a.root.resolve();a.screen_variants=a.screen_variants.split(',') if a.screen_variants else []
    if (not re.fullmatch('[0-9a-f]{64}',a.archive_sha256) or not re.fullmatch('[1-9][0-9]{0,19}',a.source_run)
            or any(not re.fullmatch('[0-9a-f]{40}',x) for x in [a.baseline,a.candidate,a.prepatch,*a.screen_variants])
            or a.baseline!=h.BASELINE or len(a.screen_variants)>7 or len(set(a.screen_variants))!=len(a.screen_variants)
            or a.candidate==a.prepatch or a.candidate in a.screen_variants or a.prepatch in a.screen_variants):
        parser.error('frozen archive/run/commit identities required')
    if a.root==a.source or a.root in a.source.parents or a.source in a.root.parents:
        parser.error('output must be independent of checkout')
    def stop(*unused):raise KeyboardInterrupt('build reuse interrupted')
    signal.signal(signal.SIGTERM,stop);signal.signal(signal.SIGALRM,stop);signal.alarm(15*60)
    result=prepare(a);print(json.dumps(result));return 0


if __name__=='__main__':raise SystemExit(main())
