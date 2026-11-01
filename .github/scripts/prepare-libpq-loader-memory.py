#!/usr/bin/env python3
"""Restore two fixed artifacts and freeze the existing preliminary memory9.

No process is built or started here. Original scored data is never restored.
"""
import argparse
import hashlib
import json
from pathlib import Path, PurePosixPath
import shutil
import stat
import sys
import tarfile
import zipfile

OLD_ZIP_SHA='ee3328487aca8bdb5c6bf855a5a8dbe3da47a101c3f13abf29ee1e3fd313e234'
NEW_ZIP_SHA='34ddb41cd5604978fb36ba35abe46723943b0c63311d3835de19062bba4fcb98'
OLD_TAR_SHA='33526a0f802767104d869b8874b7b004fdf69d8567a62a8111d13cef529ea4e8'
NEW_FILES={
    'A/libpq.so.5.18':'630e4bea0b958c9ce1740ffef896a76de10afb10fa242dd3f0e87cfa46b8f0f0',
    'B/libpq.so.5.18':'5bb6b48165b9efdd2e99d6158dbe6bf9cd1af9f15763867da1a0b29b440f426e',
    'A/record.json':'2ba2453acdad63d5c3715cfb1262a12b85d09836d5be01fc126be89039688ef0',
    'B/record.json':'30d925eea4e2378c172e3c494b743d5d1238f20663a53f5f50ff071b48431b99',
    'identities.json':'18c70d287252983b1fc33fa3939d7886df79873ef0b5076336cb1627ff57d325',
    'result.json':'7b6ae61fd216c134ec649853755b6b3efc33b8e0e66a1b9e760d47c458eb61d9',
    'runtime-module-sha256.json':'df150f99f092ec9cad8a758ec6b14d8790f9508531c363b468da926fc4f8dc3c',
    'A-preprocessor-equivalence.json':'2f6f1c584e4b8123f5df9fdad672808888655fab2317529a84463e08dfbcf1bb',
    'reuse-checks.json':'8fd6edad007bf787fee78eb2291d05287a3ee77fe06364187b7c79ae9a1592ed',
    'paired-interposition.json':'c0a49c676441ae3484a6c963b8183e5083791a68c9b3f7135d8ef8c0e280cc88'}


def digest(path):
    h=hashlib.sha256()
    with Path(path).open('rb') as source:
        for block in iter(lambda:source.read(1<<20),b''):h.update(block)
    return h.hexdigest()


def checked(path,wanted):
    if digest(path)!=wanted:raise ValueError('artifact SHA256 differs: '+str(path))


def safe_name(name):
    path=PurePosixPath(name)
    if not path.parts or path.is_absolute() or '..' in path.parts or '\\' in name:
        raise ValueError('unsupported archive path: '+name)
    return path


def zip_index(source):
    entries={};total=0
    for item in source.infolist():
        safe_name(item.filename)
        if item.filename in entries or stat.S_ISLNK(item.external_attr>>16) or item.is_dir():
            raise ValueError('duplicate/link/directory ZIP member')
        if not 0<=item.file_size<=64<<20:raise ValueError('ZIP member exceeds 64MiB')
        entries[item.filename]=item;total+=item.file_size
    if len(entries)>500 or total>128<<20:raise ValueError('ZIP expanded bounds exceeded')
    return entries


def restore_old(zip_path,output,fixture):
    checked(zip_path,OLD_ZIP_SHA);receipt=json.loads(fixture.read_text())
    allow={entry['path']:entry for entry in receipt['files']}
    if len(allow)!=len(receipt['files']):raise ValueError('duplicate allowlist member')
    temporary=output.parent/'original-evidence.tar.gz'
    with zipfile.ZipFile(zip_path) as source:
        entries=zip_index(source)
        if set(entries)!={'ku-performance-v2-evidence.tar.gz','archive.json'}:
            raise ValueError('fixed old ZIP member set differs')
        with source.open('ku-performance-v2-evidence.tar.gz') as original,temporary.open('xb') as dest:
            shutil.copyfileobj(original,dest,1<<20)
    checked(temporary,OLD_TAR_SHA)
    output.mkdir(parents=True,exist_ok=False)
    try:
        with tarfile.open(temporary,'r:gz') as source:
            members=source.getmembers();seen=set();total=0
            for item in members:
                safe_name(item.name)
                if item.name in seen or not item.isfile() or not 0<=item.size<=128<<20:
                    raise ValueError('unsupported old TAR member')
                seen.add(item.name);total+=item.size
            if len(members)>20000 or total>768<<20:raise ValueError('old TAR expanded bounds exceeded')
            if not set(allow)<=seen:raise ValueError('required old input absent')
            for name,entry in allow.items():
                item=source.getmember(name)
                if item.size!=entry['bytes']:raise ValueError('original input byte count differs: '+name)
                raw=source.extractfile(item).read()
                if hashlib.sha256(raw).hexdigest()!=entry['archived_sha256']:
                    raise ValueError('original input archived SHA differs: '+name)
                dest=output/name;dest.parent.mkdir(parents=True,exist_ok=True);dest.write_bytes(raw)
                dest.chmod(entry['mode']&0o777)
    finally:
        temporary.unlink()
    return dict(run=41280471018,head='d553a2a5aa77152d693d88586f3d01fed1ca776c',
        zip_sha256=OLD_ZIP_SHA,tar_sha256=OLD_TAR_SHA,restored_files=len(allow),
        restored_bytes=sum(x['bytes'] for x in allow.values()),old_measurements_restored=False)


def restore_new(zip_path,output):
    checked(zip_path,NEW_ZIP_SHA);output.mkdir(parents=True,exist_ok=False)
    with zipfile.ZipFile(zip_path) as source:
        entries=zip_index(source)
        if not set(NEW_FILES)<=entries.keys():raise ValueError('new gate inputs absent')
        for name,wanted in NEW_FILES.items():
            raw=source.read(name)
            if hashlib.sha256(raw).hexdigest()!=wanted:raise ValueError('new gate member changed: '+name)
            dest=output/name;dest.parent.mkdir(parents=True,exist_ok=True);dest.write_bytes(raw)
    identity=json.loads((output/'identities.json').read_text());gate=json.loads((output/'result.json').read_text())
    if identity['tools_commit']!='0c2c351efb9af884b041fe92df0872f83bc30caf':raise ValueError('new B source identity differs')
    if gate['status']!='PASS_SCOPED_SHARED_LOADER_GATE' or gate['paired_global_interposition'] is not True:
        raise ValueError('new global binding gate absent')
    old_a=json.loads((output/'A/record.json').read_text())['build_provenance']
    if (old_a['actual_tools_commit']!='2fd62cda3d0c44e499bc66e1157ecd012f8d213d' or old_a['reused_from_run']!=41286409116
            or old_a['actual_source_patch_sha256']!='7af0d6cfe39c55e5a8d0d56123a74ebfeb12e677a83ffdc5d9a277daa81d927f'):
        raise ValueError('eager A original build provenance differs')
    bindings={}
    for arm in ('A','B'):
        library=output/arm/'libpq.so.5.18';(library.parent/'libpq.so.5').symlink_to(library.name)
        bindings[arm]=dict(path=str(library),sha256=NEW_FILES[arm+'/libpq.so.5.18'],
            build_record=str(output/arm/'record.json'),build_record_sha256=NEW_FILES[arm+'/record.json'],
            functional_gate=str(output/'result.json'),functional_gate_sha256=NEW_FILES['result.json'],
            source_build_identity=str(output/'identities.json'),source_build_identity_sha256=NEW_FILES['identities.json'])
    return bindings


def main(argv=None):
    parser=argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--old-zip',required=True,type=Path);parser.add_argument('--new-zip',required=True,type=Path)
    parser.add_argument('--root',required=True,type=Path);args=parser.parse_args(argv)
    repo=Path(__file__).resolve().parents[2];fixture=repo/'.github/fixtures/libpq-loader-memory'
    sys.path.insert(0,str(repo/'tests/benchmarks/cross_stack'))
    import libpq_loader_screen as lps
    root=args.root.resolve();root.mkdir(parents=True,exist_ok=False)
    old=restore_old(args.old_zip,root/'original',fixture/'original-allowlist.json')
    bindings=restore_new(args.new_zip,root/'libraries')
    (root/'libraries.json').write_text(json.dumps(bindings,sort_keys=True,indent=2)+'\n')
    original_c=next((root/'original/B/build/ku-native').rglob('ku-server.c'))
    plan=lps.prepare(root/'original',root/'memory9',bindings,fixture/'current-default-C-proof.json',original_c)
    modules=lps.verify_system_modules(plan)
    (root/'system-modules-preparation.json').write_text(json.dumps(modules,sort_keys=True,indent=2)+'\n')
    old.update(new_run=41288923081,new_head='0c2c351efb9af884b041fe92df0872f83bc30caf',
        new_zip_sha256=NEW_ZIP_SHA,actual_A_DSO_SHA256=NEW_FILES['A/libpq.so.5.18'],
        actual_B_DSO_SHA256=NEW_FILES['B/libpq.so.5.18'],preliminary_only=True,default_adopted=False,
        servers_started=0,programs_built=0,performance_windows=0)
    (root/'restoration.json').write_text(json.dumps(old,sort_keys=True,indent=2)+'\n')
    provenance=root/'memory9/loader-provenance';provenance.mkdir()
    for name in NEW_FILES:
        if name.endswith('.json'):
            target=provenance/name;target.parent.mkdir(parents=True,exist_ok=True);shutil.copyfile(root/'libraries'/name,target)
    shutil.copyfile(root/'restoration.json',provenance/'restoration.json')
    shutil.copyfile(fixture/'current-default-C-proof.json',provenance/'current-default-C-proof.json')
    args.old_zip.unlink();args.new_zip.unlink()
    print(json.dumps(dict(prepared=True,windows=len(plan['windows']),**old),sort_keys=True))


if __name__=='__main__':main()
