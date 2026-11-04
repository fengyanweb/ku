#!/usr/bin/env python3
"""Two candidate-only PG I/O observations using the existing mechanism runner."""
import argparse
import copy
import json
import os
from pathlib import Path
import platform
import signal
import sys

PRODUCT='d006d1f976910c4c9ecb890b3ce6507ecda02f37'
RECIPE_SHA='513b97d133b35936cb1d3bc9b19be0562f70f218fadbf74d25f029a8eb8cb8c3'


def main():
    p=argparse.ArgumentParser();p.add_argument('--root',required=True,type=Path)
    p.add_argument('--source',required=True,type=Path);p.add_argument('--tools',required=True,type=Path)
    p.add_argument('--phase',choices=('prepare','observe'),required=True);a=p.parse_args()
    guard=a.root.resolve();a.source=a.source.resolve();a.tools=a.tools.resolve()
    sys.path.insert(0,str(a.source/'tests/benchmarks/cross_stack'))
    import pg_read_screen as screen
    screen.helpers(a.source);h,fc=screen.h,screen.fc
    import typed_pg_self as runner
    if any(os.environ.get(k) for k in ('LD_PRELOAD','LD_AUDIT','LD_LIBRARY_PATH')):
        raise ValueError('library overrides are not the fixed default mechanism environment')
    normal=guard/'pg-read-screen';source_plan=h.load(normal/'frozen-plan.json')
    prepared=h.load(normal/'prepare.json');declared=h.load(normal/'build-identities.json')
    if source_plan.get('candidate')!=PRODUCT or source_plan!=screen.make_plan(source_plan['versions'],PRODUCT,h.RESOURCES,h.POLICY):
        raise ValueError('exact frozen candidate normal screen required')
    if prepared['entry_sha256']!=h.sha(screen.__file__) or prepared['normal_plan_sha256']!=h.sha(guard/'frozen-plan.json'):
        raise ValueError('normal preparation entry/parent plan changed')
    if any(h.sha(Path(h.__file__).parent/n)!=v for n,v in prepared['helper_sha256'].items()):
        raise ValueError('normal helpers changed after preparation')
    source_plan={'versions':{n:copy.deepcopy(source_plan['versions'][n]) for n in ('B','BP')}}
    bindings={n:screen.check_binding(source_plan,n,typed=n=='BP') for n in ('B','BP')}
    if any(bindings[n]!=declared[n] or bindings[n]['source_commit']!=PRODUCT for n in bindings):
        raise ValueError('normal candidate B/BP source or full file identity changed')
    a.mechanism_recipe=a.source/'tests/benchmarks/cross_stack/pg_owner_mechanism.py'
    if a.mechanism_recipe.is_symlink() or h.sha(a.mechanism_recipe)!=RECIPE_SHA:
        raise ValueError('fixed baseline-comparable I/O recipe changed')
    a.root=guard/'read-candidate-mechanism'
    if a.phase=='prepare':
        if os.geteuid()==0:raise ValueError('native-only probe preparation requires ordinary build user')
        a.root.mkdir(exist_ok=False)
        expected={n:{k:v['sha256'] for k,v in bindings[n]['files'].items()} for n in bindings}
        plan,derived=runner.prepare_mechanism(a,source_plan,bindings,product=PRODUCT,expected_files=expected)
        h.save(a.root/'parent.json',dict(product=PRODUCT,entry_sha256=h.sha(__file__),recipe_sha256=RECIPE_SHA,
            normal_plan_sha256=h.sha(normal/'frozen-plan.json'),normal_bindings=bindings,
            normal_versions=source_plan['versions'],derived_bindings=derived,
            reused_baseline_run='41835125551',new_baseline_windows=0,new_Rust_builds=0,
            purpose='Candidate recv/EAGAIN sites, epoll/NET_STEP and positive-read opportunities only; never instrumented CPU/memory benefit.'))
        print(json.dumps({'prepared':True,'product':PRODUCT,'maximum_windows':plan['maximum_windows']}));return 0
    if os.geteuid()!=0 or platform.system()!='Linux' or platform.machine() not in ('aarch64','arm64'):
        raise ValueError('observe requires owned Linux ARM64 cgroup root control')
    proof=h.load(a.root/'parent.json');plan=h.load(a.root/'frozen-plan.json')
    if (proof['entry_sha256']!=h.sha(__file__) or proof['normal_plan_sha256']!=h.sha(normal/'frozen-plan.json')
            or proof['normal_bindings']!=bindings or proof['normal_versions']!=source_plan['versions']
            or plan['current_product']!=PRODUCT or plan['original_bindings']!=bindings
            or plan['entry_sha256']!=h.sha(runner.__file__) or plan['recipe']['sha256']!=RECIPE_SHA):
        raise ValueError('candidate probe or its original normal identities changed')
    if {n:fc.version_identity(plan,n) for n in ('B','BP')}!=proof['derived_bindings']:
        raise ValueError('candidate probe binaries changed')
    def stop(*unused):raise KeyboardInterrupt('fixed candidate mechanism bound; preserve data and cleanup')
    signal.signal(signal.SIGTERM,stop);signal.signal(signal.SIGALRM,stop);signal.alarm(420)
    try:
        manifest=h.load(Path(source_plan['versions']['B']['root'])/'manifest.json')
        result=runner.observe_mechanism(a,plan,proof['derived_bindings'],manifest)
        print(json.dumps({'execution_complete':result['execution_complete'],'summary':str(a.root/'summary.json'),
            'instrumented':True,'CPU_or_memory_gain_claimed':False,'new_baseline_windows':0}))
        return 0 if result['execution_complete'] else 2
    finally:signal.alarm(0)


if __name__=='__main__':raise SystemExit(main())
