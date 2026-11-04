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
TYPED_PG_ARCHIVE_SHA='b6eaf2d1e1c3273017a63890d74ec42c366c0219056ea3ad64cb0d9bda885ea1'
TYPED_PG_PRODUCT='80aaf7bccd4a436ebdf83e2283a83fcc6deec4fd'
TYPED_GUARD_ARCHIVE_SHA='0e0b4c4fa47659d05222ae09e8e7d1147421189fa8c45437927f6ef95ca4af1b'
TYPED_GUARD_PRODUCT='166453a1d2768bbce19796d9c7823be73cc84ccb'
TYPED_GUARD_REJECTED='746e913538a76f111e44d212b3dbbfb4ca5a06c1'
PG_CACHE_ELFS={'normal':'234300bba9b6fce603a178701ac289a82108398d5f35de3c0a7ad585f082367b',
    'typed':'faf34cec074f10b225ad880f149fb60f2fda9068fd192872ee00222344bd5b93'}
TYPED_PG_INPUT_SHA='d9b5ea46dd2b401ae368471a822efa8467efa64c50e4b5966681c0d2a21bc4a0'
LARGE_PG_ARCHIVE_SHA='0e46b5fc4032cce8410d4c9fa8654f9e942af400bac5fd63e0783d328338b332'
LARGE_PG_PRODUCT='5857373ef316c9b67789141038a93b46e62a2f20'
LARGE_PG_ELF='2b884a0e6a4a44ee5aa603b74042504f4195cc62441cc4505259e259c98a39ae'
LARGE_PG_SOURCES={'src/backend/c_pg_async_runtime.c':'a2f775f6586ac09d09cf20c107fc7bfca847e0d5cd3f40da9b8b080086fa99ae',
    'tests/native_pg_query_poll_test.rs':'aa0c75aa408112877a57a26e55aa0509fadb68e8e6551e416045c5b3756f5179'}
PG_CONTRACT_TESTS=('native_pg_async_pump_suspends_preserves_owned_results_and_drains_all_waiter_pins',
    'native_pg_typed_single_row_binary_values_validate_types_and_survive_reset')
PG_CONTRACT_LOGS=tuple('contracts/'+test+'-'+mode+'.log' for test in PG_CONTRACT_TESTS for mode in ('O2','detectors'))
CURRENT_PG_ARCHIVE_SHA='2f1163c34bd45ad4172acf28f1a94f38f7b382a271ba99c6469592beb0744142'
CURRENT_PG_ELF='461108f63e95fd3d0acb0aad7b75483d911090c7872b039da85b68c808a503ea'
CURRENT_PG_C='44ea1ffc141048a081ba21edd379953a0d40c25ec2c21c6c91da3bba39f7891c'
CURRENT_PG_FILES={'ELF':CURRENT_PG_ELF,'generated C':CURRENT_PG_C,
    'expanded source':'59ede7cdba403d4d038c3720ab65f1cd940efd1038d07df782743a35eb9887ae',
    'Ku CLI':'35a31878f9cf688abaa2dbf4be73721d7477289587c9bd19a361623972246e8a',
    'driver ELF':'08f1f4cf722c2c9939dd2b2df3c19951895cddac8dbabd395053a7af1534efbd',
    'libpq':'b59de287c314a78cb70e7e0bb31fb2be34b961ccc90beff405d8c48412ace2df',
    'declared:protocol':'ac9438bbda3ed4298980dcca6c5f4a26536fc871fd24d4ec843c62c17b8a6cf7'}
CURRENT_TYPED_PG_FILES={**CURRENT_PG_FILES,
    'ELF':'b66d4a02726366410fd08c589ce20129401b1a6b878e9683e87b36a888a3fc31',
    'generated C':'6a54f5e24fd5451659dbf0585c0849c5c2b0f699540398d5f79e2826c95305d3',
    'expanded source':'d9b5ea46dd2b401ae368471a822efa8467efa64c50e4b5966681c0d2a21bc4a0'}


def checked(path, expected):
    if not re.fullmatch('[0-9a-f]{64}',expected or '') or h.sha(path)!=expected:
        raise ValueError('reused file identity differs: '+str(path))


def translated(value, root):
    if isinstance(value,dict):return {k:translated(v,root) for k,v in value.items()}
    if isinstance(value,list):return [translated(v,root) for v in value]
    if isinstance(value,str):return value.replace(TOKEN,str(root))
    return value


def extract_builds(archive, root, expected_sha256, names, *, owner_observation=False, typed_pg_observation=False, pg_input_cache=False, pg_large_input_cache=False, current_pg_cache=False):
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
        prepared=json.load(tar.extractfile('prepared-current.json')) if current_pg_cache else None
        current_cache=(current_pg_cache and expected_sha256==CURRENT_PG_ARCHIVE_SHA and set(names) in ({'B','Driver'},{'B','BP','Driver'})
            and prepared.get('product')==h.PG_CURRENT_BASELINE and prepared.get('rust_toolchain')=='1.99.1'
            and prepared.get('new_builds') is True and prepared.get('imported_measurement_windows')==0
            and prepared.get('bindings',{}).get('B',{}).get('files',{}).get('ELF',{}).get('sha256')==CURRENT_PG_ELF)
        if current_pg_cache and not current_cache:raise ValueError('fixed current Rust1.99.1 B/Driver build cache required')
        old={} if current_cache else json.load(tar.extractfile('hosted-state.json'))
        typed_cache=(typed_pg_observation and expected_sha256==TYPED_PG_ARCHIVE_SHA
            and set(names)=={'B','BP','R'} and old.get('identity',{}).get('candidate')==TYPED_PG_PRODUCT
            and all(old.get('steps',{}).get(k,{}).get('state')=='completed' for k in ('build','run')))
        if typed_pg_observation and not typed_cache:raise ValueError('fixed typed PG build cache required')
        identity=old.get('identity',{})
        guard_cache=(pg_input_cache and expected_sha256==TYPED_GUARD_ARCHIVE_SHA and set(names)=={'A','Driver'}
            and identity.get('baseline')==identity.get('prepatch')==TYPED_GUARD_PRODUCT
            and identity.get('candidate')==TYPED_GUARD_REJECTED and identity.get('pg_input_guard') is True
            and all(old.get('steps',{}).get(k,{}).get('state')=='completed' for k in ('build','build-A','build-B','build-Driver')))
        if pg_input_cache and not guard_cache:raise ValueError('fixed default A/Driver build cache required')
        large_cache=(pg_large_input_cache and expected_sha256==LARGE_PG_ARCHIVE_SHA and set(names)=={'A','B','Driver'}
            and identity.get('baseline')==identity.get('prepatch')==TYPED_GUARD_PRODUCT
            and identity.get('candidate')==LARGE_PG_PRODUCT and identity.get('pg_input_guard') is True
            and all(old.get('steps',{}).get(k,{}).get('state')=='completed' for k in ('build','build-A','build-B','build-Driver')))
        if pg_large_input_cache and not large_cache:raise ValueError('fixed large-input A/B/Driver build cache required')
        if any(not name.startswith('build') for name in old.get('steps',{})):
            if not (typed_cache or guard_cache or large_cache) and not (owner_observation and set(names)=={'B','R'} and expected_sha256==OWNER_ARCHIVE_SHA
                    and set(old.get('steps',{}))=={'build','build-B','build-R','diagnostic'}
                    and all(v.get('state')=='completed' for v in old['steps'].values())):
                raise ValueError('this reuse path accepts build-only source runs except the fixed owner archive')
        if not (typed_cache or guard_cache or large_cache or current_cache) and any(old.get('steps',{}).get('build-'+name,{}).get('state')!='completed' for name in names):
            raise ValueError('requested normal build was not completed')
        selected=[m for m in members if PurePosixPath(m.name).parts[0] in names or (large_cache and m.name in PG_CONTRACT_LOGS)
            or (current_cache and m.name=='prepared-current.json')]
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
        provenance=old if not (owner_observation or typed_cache or guard_cache or large_cache) else dict(identity=old.get('identity'),
            steps={k:v for k,v in old.get('steps',{}).items() if k.startswith('build')},measurement_results_imported=False)
        if current_cache:provenance=dict(prepared_current=prepared,measurement_results_imported=False,hosted_state_imported=False)
        h.save(root/'reuse-original-state.json',provenance)
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


def restore_task_json_support(source,root,commit,mapping):
    """Recover only the omitted product IR file, bound to the original CLI receipt."""
    if not re.fullmatch('[0-9a-f]{40}',commit):raise ValueError('full Task JSON product commit required')
    manifest=translated(h.load(root/'B/manifest.json'),root)
    if manifest.get('candidate_base')!=commit:raise ValueError('Task JSON source commit differs from cached B')
    build=manifest['stacks']['ku']['build'];receipt=Path(build['cli_build_receipt'])
    entry=mapping[receipt.relative_to(root).as_posix()]
    if entry['original_sha256']!=build['cli_build_receipt_sha256']:raise ValueError('original Task JSON CLI receipt differs')
    checked(receipt,entry['archived_sha256']);cli=h.load(receipt)
    if cli.get('candidate_base')!=commit or cli.get('failure') is not None:
        raise ValueError('Task JSON source differs from the CLI build identity')
    relative='src/ir/task.rs';raw=subprocess.check_output(['git','-C',str(source),'show',commit+':'+relative],timeout=10)
    if not 0<len(raw)<=MAX_FILE:raise ValueError('Task JSON Git source byte limit')
    digest=hashlib.sha256(raw).hexdigest();recorded=cli.get('source_files',{}).get(relative)
    if recorded is not None and recorded!=digest:raise ValueError('Task JSON Git source differs from CLI source receipt')
    destination=root/'B/src'/relative;restored=not destination.exists()
    if restored:destination.parent.mkdir(parents=True,exist_ok=True);destination.write_bytes(raw)
    checked(destination,digest)
    h.save(root/'reuse-task-json-source-recovery.json',dict(path=destination.relative_to(root).as_posix(),
        source_path=relative,source_commit=commit,source='product Git object',sha256=digest,bytes=len(raw),restored=restored,
        original_file_in_archive=destination.relative_to(root).as_posix() in mapping,
        cli_source_file_sha256=recorded,cli_source_file_hash_checked=recorded is not None,
        cli_receipt_original_sha256=entry['original_sha256'],cli_receipt_archived_sha256=entry['archived_sha256'],
        scope='Only IR support source is verified from Git; this does not claim the archive contained its original file or import compiled artifacts/measurements.'))


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


def verify_manifest(root, name, commit, mapping, verify_system=True, *, expected_pg_read='int', client_source_name='R', require_current_rust=False):
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
        if expected_pg_read not in ('int','typed') or (expected_pg_read=='typed' and name!='BP'):
            raise ValueError('explicit typed BP or ordinary int mode required')
        if any(build.get(k)!=v for k,v in [('formatting_mode','helpers'),('json_response_mode','text'),('pg_execution_mode','async'),('pg_read_mode',expected_pg_read)]):
            raise ValueError('reused build is not the normal helpers/text/async/int contract')
        if build.get('json_execution_mode','worker')!=('task' if name=='BT' else 'worker'):
            raise ValueError('reused JSON execution mode differs: '+name)
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
        if require_current_rust:
            cli_receipt=Path(build['cli_build_receipt']);entry=mapping[cli_receipt.relative_to(root).as_posix()]
            if entry['original_sha256']!=build['cli_build_receipt_sha256']:raise ValueError('original Rust CLI receipt differs')
            checked(cli_receipt,entry['archived_sha256'])
            build['original_cli_build_receipt_sha256']=build['cli_build_receipt_sha256']
            build['cli_build_receipt_sha256']=entry['archived_sha256']
            h.bl.require_rust_manifest(m)
    old_client=m['client'];checked(old_client['command'][0],old_client['sha256']);require_elf(old_client['command'][0])
    if client_source_name not in ('R','Driver'):raise ValueError('explicit R or Driver source required')
    checked(root/client_source_name/'src/tests/benchmarks/cross_stack/client/main.go',old_client['source_sha256'])
    h.save(root/'reuse-provenance'/(name+'-manifest.json'),original)
    m['uid'],m['gid']=os.getuid(),os.getgid()
    m['source_inventory_scope']='original build inventory; portable tree is partial and documentation redactions are separately bound'
    m['reused_build']=dict(source_commit=commit,normal_artifact_bytes_unchanged=True,
        dependency_scope='selected libpq and compiler checked; complete dynamic dependency closure first recorded on this new host',
        imported_measurement_windows=0,original_manifest=str(root/'reuse-provenance'/(name+'-manifest.json')))
    h.save(path,m)
    return m


def prepare_typed_pg_cache(archive,root,source,*,verify_system=True):
    """The exact completed80 archive supplies artifacts only, never its scores."""
    root=Path(root);source=Path(source)
    mapping=extract_builds(archive,root,TYPED_PG_ARCHIVE_SHA,{'B','BP','R'},typed_pg_observation=True)
    commits={name:TYPED_PG_PRODUCT for name in ('B','BP','R')}
    restore_json_inputs(source,root,commits,mapping)
    manifests={}
    expected={'B':'234300bba9b6fce603a178701ac289a82108398d5f35de3c0a7ad585f082367b',
        'BP':'faf34cec074f10b225ad880f149fb60f2fda9068fd192872ee00222344bd5b93'}
    for name,mode in (('B','int'),('BP','typed')):
        validate_protocol_inputs(root,name,mapping)
        manifest=verify_manifest(root,name,TYPED_PG_PRODUCT,mapping,verify_system,expected_pg_read=mode)
        checked(manifest['stacks']['ku']['command'][0],expected[name])
        if not verify_system:
            # Offline provenance review only; actual Linux use still requires
            # the exact selected system libpq and compiler below the loader.
            manifest['stacks']['ku']['build']['libpq_source']=str(root/name/'build/libpq/libpq.so')
            h.save(root/name/'manifest.json',manifest)
        manifests[name]=str(root/name/'manifest.json')
    h.save(root/'typed-pg-cache.json',dict(manifests=manifests,source_run='41304747275',
        archive_sha256=TYPED_PG_ARCHIVE_SHA,source_commit=TYPED_PG_PRODUCT,system_verified=verify_system,
        imported_measurement_windows=0,product_rebuilt=False,normal_F166_ELF_identical=True))
    return manifests


def verify_driver_cache(root,*,expected_commit=TYPED_GUARD_REJECTED):
    """Verify the fixed Go driver's completed build without invoking Go/Rust."""
    path=root/'Driver/manifest.json';original=h.load(path);m=translated(original,root)
    receipt=h.load(m['build_receipt']);client=m['client']
    if (expected_commit not in (TYPED_GUARD_REJECTED,h.PG_CURRENT_BASELINE)
            or m.get('candidate_base')!=expected_commit or set(m.get('stacks',{}))!={'control'}
            or receipt.get('candidate_base')!=expected_commit or receipt.get('failure') is not None
            or not receipt.get('commands') or any(c.get('exit_code')!=0 for c in receipt['commands'])):
        raise ValueError('fixed completed Driver build required')
    checked(client['command'][0],client['sha256']);require_elf(client['command'][0])
    control=m['stacks']['control'];checked(control['command'][0],control['build']['binary_sha256']);require_elf(control['command'][0])
    checked(root/'Driver/src/tests/benchmarks/cross_stack/client/main.go',client['source_sha256'])
    h.save(root/'reuse-provenance/Driver-manifest.json',original)
    m['uid'],m['gid']=os.getuid(),os.getgid();h.save(path,m)
    return m


def prepare_typed_pg_from_input_guard(archive,root,source,tools):
    """Reuse default A/Driver; regenerate typed C with its exact cached CLI only."""
    if os.geteuid()==0 or platform.system()!='Linux' or platform.machine() not in ('aarch64','arm64'):
        raise ValueError('native C preparation requires ordinary Linux ARM64 benchmark user')
    root=Path(root);source=Path(source);tools=Path(tools).resolve(strict=True)
    mapping=extract_builds(archive,root,TYPED_GUARD_ARCHIVE_SHA,{'A','Driver'},pg_input_cache=True)
    restore_json_inputs(source,root,{'A':TYPED_GUARD_PRODUCT,'Driver':TYPED_GUARD_REJECTED},mapping)
    for name in ('A','Driver'):validate_protocol_inputs(root,name,mapping)
    driver=verify_driver_cache(root)
    normal=verify_manifest(root,'A',TYPED_GUARD_PRODUCT,mapping,client_source_name='Driver')
    checked(normal['stacks']['ku']['command'][0],PG_CACHE_ELFS['normal'])
    checked(normal['stacks']['ku']['build']['native']['server']['source'],
        '59ede7cdba403d4d038c3720ab65f1cd940efd1038d07df782743a35eb9887ae')
    if (normal['client']['command'][0]!=driver['client']['command'][0]
            or normal['client']['sha256']!=driver['client']['sha256']):
        raise ValueError('ordinary manifest does not use the verified cached Driver')
    protocol=root/'Driver/src/tests/benchmarks/cross_stack/protocol.json'
    checked(normal['protocol'],h.sha(protocol))
    normal=h.normalize_manifest(normal,protocol,normal['client']);h.save(root/'A/manifest.json',normal)
    directory=root/'BP';h.snapshot(source,TYPED_GUARD_PRODUCT,directory/'src')
    (directory/'tools').symlink_to(tools,target_is_directory=True)
    builder=h.bl.Builder(directory,directory/'src',uid=os.getuid(),gid=os.getgid(),
        candidate=TYPED_GUARD_PRODUCT,ku_pg_read='typed')
    try:
        builder.ku_native(Path(normal['stacks']['ku']['build']['cli']));builder.finish()
    except BaseException as error:
        builder.finish(failed=repr(error));raise
    typed=h.load(directory/'manifest.json');native=typed['stacks']['ku']['build']['native']['server']
    checked(native['binary'],PG_CACHE_ELFS['typed']);checked(native['source'],TYPED_PG_INPUT_SHA)
    if any(c['label'] not in {'version-ku','version-cc',*[prefix+kind for prefix in
            ('ku-native-','ku-elf-header-','ku-elf-dynamic-') for kind in ('server','batch')]} for c in builder.commands):
        raise ValueError('typed preparation invoked a command outside native C generation/linking')
    typed=h.normalize_manifest(typed,protocol,normal['client'])
    typed['stacks']['ku']['build']['reused_cli']=dict(path=normal['stacks']['ku']['build']['cli'],
        sha256=normal['stacks']['ku']['build']['cli_sha256'],Rust_rebuilt=False)
    h.save(directory/'manifest.json',typed)
    manifests={'B':str(root/'A/manifest.json'),'BP':str(directory/'manifest.json')}
    versions={name:dict(root=str(root/name),stack='ku',source_commit=TYPED_GUARD_PRODUCT,
        binary_sha256=PG_CACHE_ELFS[role]) for name,role in (('A','normal'),('BP','typed'))}
    bindings={name:h.fc.version_identity({'versions':versions},name) for name in versions}
    h.save(root/'typed-pg-cache.json',dict(manifests=manifests,bindings=bindings,source_run='41582847043',
        archive_sha256=TYPED_GUARD_ARCHIVE_SHA,source_commit=TYPED_GUARD_PRODUCT,system_verified=True,
        imported_measurement_windows=0,rejected_product_B_tree_imported=False,Rust_rebuilt=False,Go_rebuilt=False,
        typed_native_C_rebuilt=True,normal_ELF_unchanged=True,typed_ELF_byte_identical_to_original80=True))
    return manifests


def verify_large_pg_contracts(root,source,manifest,mapping):
    """Bind completed checks to the fixed artifact and the declared product Git objects."""
    records=[]
    for relative in PG_CONTRACT_LOGS:
        path=root/relative;entry=mapping[relative];checked(path,entry['archived_sha256'])
        text=path.read_text();test=path.name.rsplit('-',1)[0]
        results=re.findall(r'^test result: (ok|FAILED)\. (\d+) passed; (\d+) failed; (\d+) ignored;',text,re.M)
        if (results!=[('ok','1','0','0')] or len(re.findall(r'^test '+re.escape(test)+r' \.\.\. ok$',text,re.M))!=1):
            raise ValueError('fixed PG contract log is not exactly one passing test: '+relative)
        records.append(dict(path=relative,archived_sha256=entry['archived_sha256'],
            original_sha256=entry['original_sha256'],passed=1,failed=0,ignored=0))
    sources={}
    for relative,expected in LARGE_PG_SOURCES.items():
        raw=subprocess.check_output(['git','-C',str(source),'show',LARGE_PG_PRODUCT+':'+relative],timeout=10)
        if hashlib.sha256(raw).hexdigest()!=expected:raise ValueError('fixed product Git source differs: '+relative)
        sources[relative]=raw
    receipt=h.load(manifest['build_receipt']);runtime='src/backend/c_pg_async_runtime.c'
    if receipt.get('source_files',{}).get(runtime)!=LARGE_PG_SOURCES[runtime]:
        raise ValueError('cached product runtime source receipt differs')
    generated=Path(manifest['stacks']['ku']['build']['native']['server']['generated_c']).read_bytes()
    if sources[runtime] not in generated:raise ValueError('cached generated C does not contain the fixed runtime source')
    result=dict(source_commit=LARGE_PG_PRODUCT,source_sha256=LARGE_PG_SOURCES,contracts=records,
        source_scope='runtime bound by archived build receipt, actual generated C and fixed Git object; test source SHA is the fixed Git object, not an unarchived file read',
        original_test_file_archived=False,all_four_contracts_passed=True,imported_measurement_windows=0)
    h.save(root/'verified-pg-contract-cache.json',result);return result


def restore_current_pg_builds(archive,cache,source,*,typed=False):
    """Common fixed current-build verifier; no original observations or builds."""
    names={'B','Driver',*(['BP'] if typed else [])}
    mapping=extract_builds(archive,cache,CURRENT_PG_ARCHIVE_SHA,names,current_pg_cache=True)
    prepared=h.load(cache/'prepared-current.json');original={};actual={}
    restore_json_inputs(source,cache,{name:h.PG_CURRENT_BASELINE for name in names},mapping)
    for name in names:validate_protocol_inputs(cache,name,mapping)
    driver=verify_driver_cache(cache,expected_commit=h.PG_CURRENT_BASELINE)
    manifests={'Driver':driver}
    for name in ('B',*(['BP'] if typed else [])):
        binding=prepared['bindings'][name];expected=CURRENT_PG_FILES if name=='B' else CURRENT_TYPED_PG_FILES
        if (binding.get('source_commit')!=h.PG_CURRENT_BASELINE
                or mapping[name+'/manifest.json']['original_sha256']!=binding.get('manifest_sha256')
                or {key:value.get('sha256') for key,value in binding.get('files',{}).items()}!=expected):
            raise ValueError('original current build binding differs from frozen archive: '+name)
        options={'expected_pg_read':'typed'} if name=='BP' else {}
        manifest=verify_manifest(cache,name,h.PG_CURRENT_BASELINE,mapping,client_source_name='Driver',require_current_rust=True,**options)
        plan={'versions':{name:dict(root=str(cache/name),stack='ku',source_commit=h.PG_CURRENT_BASELINE,
            binary_sha256=expected['ELF'],artifacts={'protocol':dict(path=manifest['protocol'],sha256=expected['declared:protocol'])})}}
        observed=h.fc.version_identity(plan,name)
        if ({key:value['sha256'] for key,value in observed['files'].items()}!=expected
                or observed['source_inventory_sha256']!=binding.get('source_inventory_sha256')
                or observed['build_mode']!=binding.get('build_mode') or manifest['client']['sha256']!=driver['client']['sha256']):
            raise ValueError('restored current normal/typed/Driver build identity differs: '+name)
        checked(manifest['stacks']['ku']['build']['native']['server']['generated_c'],expected['generated C'])
        manifests[name]=manifest;original[name]=binding;actual[name]=observed
    return manifests,original,actual


def prepare_current_typed_pg_cache(archive,root,source):
    """Recover exact d05 ordinary/typed Rust1.99.1 builds for fresh SELF only."""
    if os.geteuid()==0 or h.bl.RUST_TOOLCHAIN!='1.99.1':raise ValueError('ordinary user and current Rust1.99.1 required')
    root=Path(root)
    _,original,actual=restore_current_pg_builds(archive,root,Path(source),typed=True)
    manifests={name:str(root/name/'manifest.json') for name in ('B','BP')}
    h.save(root/'verified-typed-pg-cache.json',dict(manifests=manifests,source_run='41630973206',archive_sha256=CURRENT_PG_ARCHIVE_SHA,
        source_commit=h.PG_CURRENT_BASELINE,rust_toolchain='1.99.1',original_bindings=original,restored_bindings=actual,
        imported_measurement_windows=0,product_rebuilt=False,scope='build artifacts only; fresh same-boot preflight and windows required'))
    return manifests


def prepare_current_pg_guard_cache(archive,a):
    """Reuse only the exact Rust1.99.1 normal B as A; build candidate B afresh."""
    if a.baseline!=h.PG_CURRENT_BASELINE or a.prepatch!=h.PG_CURRENT_BASELINE or h.bl.RUST_TOOLCHAIN!='1.99.1':
        raise ValueError('current cache requires explicit d05 baseline/prepatch and Rust1.99.1')
    root=Path(a.root);source=Path(a.source)
    if root.exists() and (root.is_symlink() or any(p.name!='tools' for p in root.iterdir())):
        raise ValueError('current cache requires fresh guard root')
    cache=root/'verified-current-cache'
    typed_cache={'typed':True} if getattr(a,'pg_read_screen',False) and getattr(a,'pg_input_guard_workload','db')=='db' else {}
    restored,bindings,observations=restore_current_pg_builds(archive,cache,source,**typed_cache)
    normal,driver=restored['B'],restored['Driver'];binding,actual=bindings['B'],observations['B']
    unchanged=subprocess.run(['git','-C',str(source),'diff','--quiet',h.PG_CURRENT_BASELINE,
        a.candidate,'--','tests/benchmarks/cross_stack'],timeout=10)
    if unchanged.returncode!=0:raise ValueError('cached benchmark inputs differ from the explicit new candidate')
    new_driver_source=subprocess.check_output(['git','-C',str(source),'show',
        a.candidate+':tests/benchmarks/cross_stack/client/main.go'],timeout=10)
    if hashlib.sha256(new_driver_source).hexdigest()!=driver['client']['source_sha256']:
        raise ValueError('cached driver source differs from the explicit new candidate')
    # Keep original B paths/receipts intact in the private verified tree. Only
    # manifest aliases name it A; root/B remains absent for the fresh candidate.
    manifests={'A':normal,'Driver':driver}
    state=dict(identity=h.hosted_identity(a,'diagnostic'),uid=os.getuid(),gid=os.getgid(),
        steps={},goal_status='unknown',imported_measurement_windows=0)
    for name,manifest in manifests.items():
        h.save(root/name/'manifest.json',manifest)
        state['steps']['build-'+name]=dict(state='completed',result=manifest,reused_from_run='41630973206',
            original_build_name='B' if name=='A' else name,build_repeated=False)
    h.save(root/'hosted-state.json',state)
    h.save(root/'verified-baseline-cache.json',dict(source_run='41630973206',archive_sha256=CURRENT_PG_ARCHIVE_SHA,
        original_B_source=h.PG_CURRENT_BASELINE,original_B_role='A',A_ELF=CURRENT_PG_ELF,
        rust_toolchain='1.99.1',candidate_source=a.candidate,verified_cache=str(cache),
        driver_source_matches_candidate=True,benchmark_inputs_match_candidate=True,imported_measurement_windows=0,
        reused_steps=['build-A','build-Driver'],new_candidate_build_still_required=True,
        typed_baseline_restored='BP' in restored,
        original_normal_binding=binding,restored_normal_binding=actual))
    return state


def prepare_pg_input_guard_cache(archive,a,*,source_run='41582847043'):
    """Seed verified completed build steps, never a timed result, into a fresh guard."""
    if os.geteuid()==0:raise ValueError('prepare cached builds as the ordinary benchmark user')
    h.validate_screen_selection(a,'diagnostic')
    if source_run=='41630973206':return prepare_current_pg_guard_cache(archive,a)
    root=Path(a.root);source=Path(a.source)
    if source_run not in ('41582847043','41599087071'):raise ValueError('explicit fixed PG cache run required')
    if a.baseline!=TYPED_GUARD_PRODUCT or a.prepatch!=TYPED_GUARD_PRODUCT:
        raise ValueError('historical PG cache requires its original F166 baseline/prepatch')
    large=source_run=='41599087071'
    if large and a.candidate!=LARGE_PG_PRODUCT:raise ValueError('fixed large-input candidate required')
    archive_sha=LARGE_PG_ARCHIVE_SHA if large else TYPED_GUARD_ARCHIVE_SHA
    names={'A','B','Driver'} if large else {'A','Driver'}
    mapping=extract_builds(archive,root,archive_sha,names,**({'pg_large_input_cache':True} if large else {'pg_input_cache':True}))
    commits={'A':TYPED_GUARD_PRODUCT,'Driver':TYPED_GUARD_REJECTED,**({'B':LARGE_PG_PRODUCT} if large else {})}
    restore_json_inputs(source,root,commits,mapping)
    for name in commits:validate_protocol_inputs(root,name,mapping)
    driver=verify_driver_cache(root)
    normal=verify_manifest(root,'A',TYPED_GUARD_PRODUCT,mapping,client_source_name='Driver')
    checked(normal['stacks']['ku']['command'][0],PG_CACHE_ELFS['normal'])
    unchanged=subprocess.run(['git','-C',str(source),'diff','--quiet',TYPED_GUARD_PRODUCT,
        a.candidate,'--','tests/benchmarks/cross_stack'],timeout=10)
    if unchanged.returncode!=0:raise ValueError('cached benchmark inputs differ from the explicit new candidate')
    new_driver_source=subprocess.check_output(['git','-C',str(source),'show',
        a.candidate+':tests/benchmarks/cross_stack/client/main.go'],timeout=10)
    if hashlib.sha256(new_driver_source).hexdigest()!=driver['client']['source_sha256']:
        raise ValueError('cached driver source differs from the explicit new candidate')
    if normal['client']['sha256']!=driver['client']['sha256']:raise ValueError('cached A/Driver identity mismatch')
    manifests={'A':normal,'Driver':driver}
    if large:
        candidate=verify_manifest(root,'B',LARGE_PG_PRODUCT,mapping,client_source_name='Driver')
        checked(candidate['stacks']['ku']['command'][0],LARGE_PG_ELF)
        if candidate['client']['sha256']!=driver['client']['sha256']:raise ValueError('cached B/Driver identity mismatch')
        verify_large_pg_contracts(root,source,candidate,mapping);manifests['B']=candidate
    old=h.load(root/'reuse-original-state.json');state=dict(identity=h.hosted_identity(a,'diagnostic'),
        uid=os.getuid(),gid=os.getgid(),steps={},goal_status='unknown',imported_measurement_windows=0)
    for name,manifest in manifests.items():
        step=copy.deepcopy(old['steps']['build-'+name]);step['result']=manifest
        step.update(reused_from_run=source_run,build_repeated=False)
        state['steps']['build-'+name]=step
    h.save(root/'hosted-state.json',state)
    h.save(root/'verified-baseline-cache.json',dict(archive_sha256=archive_sha,
        original_A_source=TYPED_GUARD_PRODUCT,candidate_source=a.candidate,A_ELF=PG_CACHE_ELFS['normal'],
        source_run=source_run,driver_source_matches_candidate=True,benchmark_inputs_match_candidate=True,imported_measurement_windows=0,
        reused_steps=['build-'+name for name in manifests],new_candidate_build_still_required=not large,
        **({'B_ELF':LARGE_PG_ELF,'four_contract_logs_verified':True} if large else {})))
    return state


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
    (out/'bin').mkdir()
    binary=out/'bin/cross-stack-client'
    logs.run('build',[go,'build','-trimpath','-mod=readonly','-o',binary,'.'],directory,timeout=180)
    logs.run('contracts',[go,'test','./...'],directory,timeout=180)
    driver_input_preflight(directory,a.root/'R/src/tests/benchmarks/cross_stack/protocol.json',go,logs)
    if sources()!=source_hashes:raise ValueError('driver sources changed during preparation')
    for relative,digest in source_hashes.items():
        target=out/'source'/relative;target.parent.mkdir(parents=True,exist_ok=True)
        shutil.copyfile(directory/relative,target);checked(target,digest)
    client=dict(command=[str(binary)],cwd=str(a.root),env={'GOMAXPROCS':'1'},sha256=h.sha(binary),
        source_sha256=h.sha(directory/'main.go'),source_files_sha256=source_hashes,source_root=str(out/'source'),
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


OWNER_RUN='41182804290'
OWNER_ARCHIVE_SHA='5976cad0c051b684823214f898cf746e2c7d63cd54d3116b04b509eba4c9dac6'
OWNER_PRODUCT='4e5f5d94da277654862d8679f7ae4a7fda3f05ff'


def prepare_owner(a):
    h.validate_screen_selection(a,'diagnostic')
    if (a.source_run,a.archive_sha256,a.candidate)!=(OWNER_RUN,OWNER_ARCHIVE_SHA,OWNER_PRODUCT):
        raise ValueError('owner probe requires the fixed verified normal source/run/archive')
    if os.geteuid()==0 or platform.system()!='Linux' or platform.machine() not in ('aarch64','arm64'):
        raise ValueError('ordinary benchmark user on Linux ARM64 required')
    expected={'B':a.candidate,'R':a.candidate}
    mapping=extract_builds(a.archive,a.root,a.archive_sha256,expected,owner_observation=True)
    restore_json_inputs(a.source,a.root,expected,mapping)
    for name,commit in expected.items():
        validate_protocol_inputs(a.root,name,mapping);verify_manifest(a.root,name,commit,mapping)
    identity=h.hosted_identity(a,'diagnostic')
    state=dict(identity=identity,steps={},goal_status='unknown',uid=os.getuid(),gid=os.getgid(),
        reused_build=dict(archive_sha256=a.archive_sha256,source_run=a.source_run,product_commit=a.candidate,
            normal_versions=expected,imported_measurement_windows=0,old_boot_or_runtime_state_imported=False))
    h.save(a.root/'hosted-state.json',state)
    result=h.step(state,a.root,'build',lambda:h.prepare_memory_owner(a,state))
    h.save(a.root/'reuse-build.json',dict(**state['reused_build'],complete=result['execution_complete']))
    return result


def prepare(a):
    if getattr(a,'memory_owner_probe',False):return prepare_owner(a)
    h.validate_screen_selection(a,'followup-screen')
    if os.geteuid()==0 or platform.system()!='Linux' or platform.machine() not in ('aarch64','arm64'):
        raise ValueError('ordinary benchmark user on a fresh Linux ARM64 host required')
    expected={'A':a.prepatch,**{f'C{i+1}':c for i,c in enumerate(a.screen_variants)},'B':a.candidate,'R':a.candidate}
    if getattr(a,'task_json_screen',False):expected['BT']=a.candidate
    mapping=extract_builds(a.archive,a.root,a.archive_sha256,expected)
    if getattr(a,'task_json_screen',False):
        restore_task_json_support(a.source,a.root,a.candidate,mapping)
        h.require_task_json_support(a.root)
    restore_json_inputs(a.source,a.root,expected,mapping)
    input_checks={name:validate_protocol_inputs(a.root,name,mapping) for name in expected}
    h.save(a.root/'reuse-input-preflight.json',dict(versions=input_checks,network_requests=0))
    for name,commit in expected.items():
        if getattr(a,'common_http_screen',False) and a.prepatch==h.PG_CURRENT_BASELINE and name in ('A','B','BT'):
            verify_manifest(a.root,name,commit,mapping,require_current_rust=True)
        else:verify_manifest(a.root,name,commit,mapping)
        (a.root/name/'tools').symlink_to(a.tools,target_is_directory=True)
    new_driver(a)
    identity=h.hosted_identity(a,'followup-screen')
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
    parser.add_argument('--memory-owner-probe',action='store_true',help='Reuse only the fixed verified normal B/R archive for one instrumented allocation-owner observation')
    parser.add_argument('--task-json-screen',action='store_true',help='Require a completed, verified BT build in the source archive; never infer the screen from product capability')
    parser.add_argument('--final-task-json',action='store_true',help='Rejected: final/full requires a fresh full build, not followup archive reuse')
    parser.add_argument('--common-http-screen',action='store_true',help='Explicit shared HTTP screen; requires --task-json-screen and preserves its distinct cache identity')
    a=parser.parse_args(argv)
    try:h.validate_screen_selection(a,'diagnostic' if a.memory_owner_probe else 'followup-screen')
    except ValueError as error:parser.error(str(error))
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
