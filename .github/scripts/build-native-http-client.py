#!/usr/bin/env python3
"""Build the pinned optional native HTTP client using shared bounded pack tooling."""
from __future__ import annotations
import argparse,hashlib,importlib.util,json,os,re,shutil,sys,tempfile
from pathlib import Path
sys.dont_write_bytecode=True
ROOT=Path(__file__).resolve().parents[2]
spec=importlib.util.spec_from_file_location('native_rust_pack_helpers',Path(__file__).with_name('build-native-calendar.py'))
N=importlib.util.module_from_spec(spec);spec.loader.exec_module(N)
TOOLCHAIN=N.TOOLCHAIN
TARGETS=N.TARGETS
BUILD_ID='ku-native-http-client/0.1.0;abi=1;ureq=2.12.1;url=2.5.8;engine=shared'
HEADER='ku_native_http_client.h'
SOURCE_FILES=tuple(sorted(('crates/ku-native-http-client/Cargo.toml','crates/ku-native-http-client/build.rs','crates/ku-native-http-client/src/lib.rs','crates/ku-native-http-client/include/'+HEADER,'src/stdlib/http_client_engine.rs')))
API={b'__ku_http_client_abi_version',b'__ku_http_client_v1_build_id',b'__ku_http_client_call_v1',b'__ku_http_client_release_v1'}
plain,digest,record,run,native_tokens,compiler_toolchain=N.plain,N.digest,N.record,N.run,N.native_tokens,N.compiler_toolchain

def archive_name(target):
 if target not in TARGETS:raise ValueError('unsupported native HTTP client target')
 return 'ku_native_http_client.lib' if target.endswith('-msvc') else 'libku_native_http_client.a'

def source_hash():
 result=hashlib.sha256()
 for name in SOURCE_FILES:
  data=(ROOT/name).read_bytes();result.update(name.encode()+b'\0'+data+b'\0')
 return result.hexdigest()

def validate_archive(path,target):return N.validate_archive(path,target,API,BUILD_ID)
def dependency_licenses(metadata):return N.dependency_licenses(metadata,'ku-native-http-client')
def validate_pack(leaf,target,expected_source=None):
 leaf=Path(leaf)
 if leaf.is_symlink() or not leaf.is_dir():raise ValueError('native HTTP pack root must be a plain directory')
 for entry in leaf.rglob('*'):
  relative=entry.relative_to(leaf).as_posix()
  if entry.is_symlink() or not entry.is_file() and (not entry.is_dir() or relative not in ('include','lib')):raise ValueError('unexpected native HTTP pack entry')
 plain(leaf/'manifest.json',65536)
 manifest=json.loads((leaf/'manifest.json').read_text(encoding='utf-8'),object_pairs_hook=N.unique_object)
 required={'schema_version','abi_version','build_id','crate_version','ureq_version','rustc_version','rustc_verbose','target','source_sha256','archive','header','native_static_libs','licenses'}
 if set(manifest)-{'build'}!=required:raise ValueError('invalid native HTTP manifest fields')
 for key,want in {'schema_version':1,'abi_version':1,'build_id':BUILD_ID,'crate_version':'0.1.0','ureq_version':'2.12.1','rustc_version':TOOLCHAIN,'target':target}.items():
  if type(manifest.get(key)) is not type(want) or manifest.get(key)!=want:raise ValueError('native HTTP identity mismatch: '+key)
 if expected_source is not None and manifest['source_sha256']!=expected_source:raise ValueError('native HTTP source mismatch')
 if not re.fullmatch(r'[0-9a-f]{64}',manifest['source_sha256']):raise ValueError('invalid native HTTP source identity')
 if not re.search(r'^release: 1\.99\.1$',manifest['rustc_verbose'],re.M):raise ValueError('native HTTP compiler differs')
 files={'manifest.json','LICENSE.ku','LICENSE.dependencies','include/'+HEADER,'lib/'+archive_name(target)}
 if {p.relative_to(leaf).as_posix() for p in leaf.rglob('*') if p.is_file()}!=files:raise ValueError('native HTTP pack has missing or extra files')
 if len(manifest['licenses'])!=2:raise ValueError('native HTTP licenses missing')
 for rel,decl in [('lib/'+archive_name(target),manifest['archive']),('include/'+HEADER,manifest['header']),*[(name,row) for name,row in zip(['LICENSE.ku','LICENSE.dependencies'],manifest['licenses'])]]:
  plain(leaf/rel,128*1024*1024 if rel.startswith('lib/') else 1024*1024)
  if decl!=record(leaf/rel):raise ValueError('native HTTP file identity mismatch: '+rel)
 if len(manifest['licenses'])!=2:raise ValueError('native HTTP licenses missing')
 if expected_source is not None and (leaf/'include'/HEADER).read_bytes()!=(ROOT/'crates/ku-native-http-client/include'/HEADER).read_bytes():raise ValueError('native HTTP header differs')
 libs=manifest['native_static_libs']
 if set(libs)!={'raw','tokens'} or native_tokens(libs['raw'],target)!=libs['tokens']:raise ValueError('native HTTP link contract differs')
 validate_archive(leaf/'lib'/archive_name(target),target)
 return manifest

def build(target, output, toolchain=TOOLCHAIN, target_dir=None):
    if toolchain != TOOLCHAIN:
        raise ValueError('native HTTP client requires frozen Rust ' + TOOLCHAIN)
    output = Path(output).absolute()
    if output.exists() or output.is_symlink():
        return validate_pack(output, target, source_hash())
    target_dir = Path(target_dir or ROOT/'target').resolve()
    if output == target_dir or target_dir in output.parents or output in target_dir.parents:
        raise ValueError('HTTP client pack and build cache must be separate directory trees')
    env = dict(os.environ)
    if any(env.get(key) for key in ('RUSTFLAGS', 'CARGO_ENCODED_RUSTFLAGS', 'RUSTC_WRAPPER', 'RUSTC_WORKSPACE_WRAPPER')):
        raise ValueError('HTTP client pack requires an unwrapped compiler without ambient Rust flags')
    env['CARGO_PROFILE_RELEASE_PANIC'] = 'unwind'
    env['CARGO_BUILD_JOBS'] = '1'
    if target.endswith('-apple-darwin'):
        env['MACOSX_DEPLOYMENT_TARGET'] = '11.0'
    selector = compiler_toolchain(toolchain, target, os.name)
    verbose = run(['rustc', '+'+selector, '-Vv'], env, 60).stdout.decode('utf-8')
    if not re.search(r'^release: 1\.99\.1$', verbose, re.M):
        raise ValueError('the actual compiler is not the frozen release')
    host = re.search(r'^host: (.+)$', verbose, re.M)
    if host is None or (os.name == 'nt' and target == 'x86_64-pc-windows-gnu' and host[1] != target):
        raise ValueError('native HTTP client compiler host differs from its selected toolchain')
    before = source_hash()
    metadata = json.loads(run(['cargo', '+'+selector, 'metadata', '--locked', '--offline',
                              '--format-version=1', '--filter-platform', target], env, 120).stdout)
    license_text = dependency_licenses(metadata)
    command = ['cargo', '+'+selector, 'rustc', '--locked', '--offline', '--release',
               '--message-format=json-render-diagnostics', '--color', 'never',
               '--manifest-path', ROOT/'Cargo.toml', '--target-dir', target_dir,
               '-p', 'ku-native-http-client', '--target', target, '--', '--print', 'native-static-libs']
    result = run(command, env)
    diagnostics = [result.stderr.decode('utf-8', errors='replace')]
    artifacts = []
    for line in result.stdout.decode('utf-8').splitlines():
        try:
            item = json.loads(line)
        except json.JSONDecodeError:
            diagnostics.append(line)
            continue
        if item.get('reason') == 'compiler-message':
            diagnostics.extend((item['message'].get('rendered') or '', item['message'].get('message') or ''))
        if item.get('reason') == 'compiler-artifact' and item['target']['name'] == 'ku_native_http_client':
            artifacts.extend(Path(name) for name in item['filenames'] if Path(name).name == archive_name(target))
    libs = set(re.findall(r'(?m)^\s*(?:note:\s*)?native-static-libs:\s*([^\r\n]+)', '\n'.join(diagnostics)))
    if len(libs) != 1:
        raise ValueError('compiler did not report one actual native-static-libs contract')
    raw = libs.pop().strip()
    tokens = native_tokens(raw, target)
    artifacts = sorted(set(path.resolve() for path in artifacts))
    if len(artifacts) != 1 or artifacts[0] != (target_dir/target/'release'/archive_name(target)).resolve():
        raise ValueError('cargo did not identify one matching target Release static archive')
    objects = validate_archive(artifacts[0], target)
    if source_hash() != before:
        raise ValueError('HTTP client source changed during the build; refusing mixed identity')
    output.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix='.ku-http-client-stage-', dir=output.parent) as name:
        stage = Path(name)
        (stage/'include').mkdir()
        (stage/'lib').mkdir()
        shutil.copyfile(artifacts[0], stage/'lib'/archive_name(target))
        shutil.copyfile(ROOT/'crates/ku-native-http-client/include'/HEADER, stage/'include'/HEADER)
        shutil.copyfile(ROOT/'LICENSE', stage/'LICENSE.ku')
        (stage/'LICENSE.dependencies').write_bytes(license_text)
        manifest = {'schema_version': 1, 'abi_version': 1, 'build_id': BUILD_ID,
                    'crate_version': '0.1.0', 'ureq_version': '2.12.1',
                    'rustc_version': toolchain, 'rustc_verbose': verbose, 'target': target,
                    'source_sha256': before, 'archive': record(stage/'lib'/archive_name(target)),
                    'header': record(stage/'include'/HEADER),
                    'native_static_libs': {'raw': raw, 'tokens': tokens},
                    'licenses': [record(stage/'LICENSE.ku'), record(stage/'LICENSE.dependencies')],
                    'build': {'command': [str(part) for part in command], 'profile': 'release',
                              'panic': 'unwind', 'native_object_members': objects,
                              'compiler_host': host[1], 'target': target,
                              'macos_deployment_target': '11.0' if target.endswith('-apple-darwin') else None}}
        (stage/'manifest.json').write_text(json.dumps(manifest, indent=2)+'\n', encoding='utf-8')
        for path in stage.rglob('*'):
            path.chmod(0o755 if path.is_dir() else 0o644)
        stage.chmod(0o755)
        validate_pack(stage, target, before)
        if output.exists() or output.is_symlink():
            raise ValueError('HTTP client output appeared during build; refusing overwrite')
        stage.rename(output)
    print('Built native HTTP client pack:', output, flush=True)
    print('native-static-libs:', raw, flush=True)
    return manifest


def main():
 parser=argparse.ArgumentParser(description=__doc__)
 parser.add_argument('--target',choices=TARGETS,required=True);parser.add_argument('--output',type=Path,required=True)
 parser.add_argument('--toolchain',default=TOOLCHAIN);parser.add_argument('--target-dir',type=Path);parser.add_argument('--verify-only',action='store_true')
 a=parser.parse_args()
 if a.toolchain!=TOOLCHAIN:raise ValueError('HTTP client requires frozen Rust '+TOOLCHAIN)
 if a.verify_only:validate_pack(a.output,a.target,source_hash())
 else:build(a.target,a.output,a.toolchain,a.target_dir)
if __name__=='__main__':
 try:main()
 except (ValueError,OSError,RuntimeError,KeyError,TypeError) as e:print('native HTTP client pack:',e,file=sys.stderr);raise SystemExit(1)
