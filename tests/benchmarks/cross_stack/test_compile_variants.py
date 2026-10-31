"""Profile receipts must not turn failed/seen-input training into accepted PGO."""
import copy
import hashlib
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
import types
from unittest.mock import patch
import batch
import compile_variants
import json_dense
import profile_training
from compile_variants import profile_input, native_flags
from pg_variants import derive
from profile_training import PLANS, expected, identity, plan, parse_output
from build_linux import (expand_ku_server, file_identity, ku_json_execution_evidence, native_emission_source,
                         normal_compiler_receipt, normal_compiler_recorder)


class KuCompilerCommandTests(unittest.TestCase):
    def test_profile_directory_uses_ku_grammar_without_shell_quoting(self):
        self.assertEqual(compile_variants.ku_cc_command(
            ['clang-18', '-fprofile-use=/tmp/a b/profile.profdata', '-flto=thin']),
            'clang-18 "-fprofile-use=/tmp/a b/profile.profdata" -flto=thin')
        self.assertEqual(compile_variants.ku_cc_command(['clang-18', "a'b"]), "clang-18 a'b")
        for argument in ['a"b', 'a\nb', 'a\rb']:
            with self.assertRaisesRegex(ValueError, 'KU_CC'):
                compile_variants.ku_cc_command(['clang-18', argument])


class KuJsonExecutionTests(unittest.TestCase):
    """Separate Task JSON diagnostic inputs without changing the frozen fixture."""

    def source(self):
        return Path(__file__).with_name('ku').joinpath('server.ku')

    def test_default_is_unchanged_and_task_changes_only_its_handler_declaration(self):
        source_path=self.source();frozen=source_path.read_bytes();source=frozen.decode('utf-8')
        worker=expand_ku_server(source,19191)
        self.assertEqual(worker,source.replace('__PEER_PORT__','19191'))
        self.assertEqual(expand_ku_server(source,19191,json_execution='worker'),worker)
        task=expand_ku_server(source,19191,json_execution='task')
        original='fn encode(req: HttpRequest): HttpResponse! {'
        selected='async '+original
        self.assertEqual(task.count(selected),1)
        self.assertEqual(task.replace(selected,original,1),worker)
        self.assertEqual(task.split('fn main():',1)[1],worker.split('fn main():',1)[1])
        self.assertEqual(source_path.read_bytes(),frozen)

        default=ku_json_execution_evidence(worker,worker,'worker')
        self.assertEqual(default['mode'],'worker')
        self.assertFalse(default['diagnostic_only'])
        self.assertTrue(default['original_worker_score_eligible'])
        self.assertEqual(default['source_diff'],'')
        self.assertEqual(default['worker_source_sha256'],default['selected_source_sha256'])
        selected=ku_json_execution_evidence(worker,task,'task')
        self.assertEqual(selected['mode'],'task')
        self.assertTrue(selected['diagnostic_only'])
        self.assertFalse(selected['original_worker_score_eligible'])
        self.assertIn('separate from the frozen synchronous JSON score',selected['scope'])
        self.assertEqual(selected['worker_source_sha256'],hashlib.sha256(worker.encode('utf-8')).hexdigest())
        self.assertEqual(selected['selected_source_sha256'],hashlib.sha256(task.encode('utf-8')).hexdigest())
        self.assertEqual(selected['source_diff_sha256'],hashlib.sha256(selected['source_diff'].encode('utf-8')).hexdigest())
        changed=[line for line in selected['source_diff'].splitlines()
                 if line.startswith(('+','-')) and not line.startswith(('+++','---'))]
        self.assertEqual(changed,['-'+original,'+async '+original])
        with self.assertRaisesRegex(RuntimeError,'differs beyond'):
            ku_json_execution_evidence(worker,task.replace('counter + 1','counter + 2'),'task')

    def test_task_requires_one_original_handler_and_normal_helpers_text(self):
        source=self.source().read_text();handler='fn encode(req: HttpRequest): HttpResponse! {'
        for changed in [source.replace(handler,'async '+handler),
                        source+'\n'+handler+'\n',source.replace(handler,'fn changed(req) {')]:
            with self.subTest(source=changed[-80:]),self.assertRaisesRegex(RuntimeError,'exactly one original synchronous'):
                expand_ku_server(changed,19191,json_execution='task')
        for mode,json_mode in [('helpers','typed'),('helpers','response'),('direct','text')]:
            with self.subTest(mode=mode,json_mode=json_mode),self.assertRaisesRegex(ValueError,'normal helpers/text'):
                expand_ku_server(source,19191,mode=mode,json_mode=json_mode,json_execution='task')
        with self.assertRaisesRegex(ValueError,'JSON execution mode'):
            expand_ku_server(source,19191,json_execution='automatic')


class NormalCompilerReceiptTests(unittest.TestCase):
    """Exercise real wrapper execution with inert files, never a C compiler."""

    def fixture(self, root):
        work = root / 'source with spaces'
        work.mkdir()
        source = work / 'generated server.c'
        source.write_text('/* fixture only: no C compilation */\n')
        staged = root / '.ku-link-library-fixture' / 'libpq.so'
        staged.parent.mkdir()
        staged.write_bytes(b'fixture libpq identity')
        final = root / 'bin with spaces' / 'ku-server'
        temporary = final.parent / '.ku-link-temp-fixture' / final.name
        temporary.parent.mkdir(parents=True)
        observed = root / 'executed-argv.json'
        compiler = root / 'compiler with spaces'
        compiler.write_text('#!' + sys.executable + '\n'
            'import json,sys\n'
            'open(' + repr(str(observed)) + ',"w").write(json.dumps(sys.argv))\n'
            'print(' + repr('native c ok: ' + str(source)) + ')\n'
            'print(' + repr('build ok: ' + str(final)) + ')\n')
        compiler.chmod(0o700)
        arguments = [str(source), '-std=c11', '-o', str(temporary), '-O2', '-pthread', str(staged)]
        return work, source, staged, final, temporary, observed, compiler, arguments

    def test_atomic_output_staged_library_and_stdout_survive_transparent_exec(self):
        with tempfile.TemporaryDirectory(prefix='ku receipts ') as directory:
            root = Path(directory)
            work, source, staged, final, temporary, observed, compiler, arguments = self.fixture(root)
            with normal_compiler_recorder(root / 'receipts', compiler, work, staged) as recorder:
                command = recorder['command']
                self.assertIn('"' + command[2] + '"', recorder['ku_cc'])
                run = subprocess.run([*command, *arguments], cwd=work, text=True,
                                     capture_output=True, timeout=10)
                self.assertEqual(run.returncode, 0, run.stderr)
                self.assertEqual(run.stderr, '')
                self.assertEqual(native_emission_source(run.stdout, final, work), source.resolve())
                with self.assertRaisesRegex(RuntimeError, 'emission output'):
                    native_emission_source('recorder noise\n' + run.stdout, final, work)
                receipt_path = Path(recorder['receipt'])
                receipt = json.loads(receipt_path.read_text())
                self.assertEqual(json.loads(observed.read_text()), [str(compiler), *arguments])
                self.assertEqual(receipt['argv'], [str(compiler), *arguments])
                self.assertEqual(receipt['cwd'], str(work.resolve()))
                self.assertEqual(receipt['output'], str(temporary.resolve()))
                self.assertNotEqual(receipt['output'], str(final))
                self.assertEqual(receipt['flags'], ['-std=c11', '-O2', '-pthread'])
                self.assertNotIn('-lm', receipt['flags'])
                self.assertEqual(receipt['library_inputs'][0]['sha256'], file_identity(staged)['sha256'])
                self.assertEqual(receipt['compiler'], file_identity(compiler))
                self.assertNotIn('env', receipt)
                # CLI deletes its private library after linking; its identity
                # must already be durable rather than read back after return.
                staged.unlink()
                self.assertEqual(json.loads(receipt_path.read_text()), receipt)
            self.assertFalse(Path(command[2]).exists())
            self.assertFalse(Path(command[3]).exists())
            self.assertTrue(receipt_path.is_file())

    def test_unknown_inputs_and_nonexact_o2_fail_before_exec_and_cleanup_on_failure(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            work, _, staged, _, _, _, compiler, arguments = self.fixture(root)
            extra = root / 'unselected.so'
            extra.write_bytes(b'different library')
            obj = root / 'extra.o'
            obj.write_bytes(b'object file')
            config = dict(compiler=file_identity(compiler), work=str(work),
                          library=file_identity(staged), helper_sha256='fixture')
            for changed in [arguments + ['-O3'], arguments + ['-O2'],
                            [x for x in arguments if x != '-O2'],
                            arguments + [str(extra)], arguments + [str(obj)],
                            arguments + [str(staged)], arguments + ['@options']]:
                with self.subTest(arguments=changed), self.assertRaises(RuntimeError):
                    normal_compiler_receipt(config, changed, work)
            with self.assertRaisesRegex(ValueError, 'interrupted fixture'):
                with normal_compiler_recorder(root / 'failed', compiler, work, staged) as recorder:
                    command = recorder['command']
                    raise ValueError('interrupted fixture')
            self.assertFalse(Path(command[2]).exists())
            self.assertFalse(Path(command[3]).exists())


class HostedWorkflowContracts(unittest.TestCase):
    """Read-only schema contracts for the opt-in job; no runner is dispatched."""

    def workflow(self):
        text=(Path(__file__).resolve().parents[3]/'.github/workflows/native-three-os.yml').read_text()
        return text,text.split('\n  performance-v2:\n',1)[1].split('\n  full-tests:\n',1)[0]

    def test_only_explicit_performance_dispatch_selects_one_arm_job(self):
        text,job=self.workflow()
        self.assertIn("task-main-path, windows-task-main-path, performance-v2, performance-v2-builds, performance-v2-finish, performance-v2-validation, performance-followup-diagnostic, followup-screen]",text)
        self.assertIn("if: github.event_name == 'workflow_dispatch' && (inputs.scope == 'performance-v2' || inputs.scope == 'performance-v2-builds' || inputs.scope == 'performance-v2-finish' || inputs.scope == 'performance-v2-validation' || inputs.scope == 'performance-followup-diagnostic' || inputs.scope == 'followup-screen' || inputs.common_http_screen || inputs.final_task_json || inputs.pg_failure_probe || inputs.normal_self_probe || inputs.pg_queue_screen || inputs.pg_driver_screen || inputs.http_wake_screen || inputs.memory_owner_probe)",job)
        self.assertIn('runs-on: ubuntu-24.04-arm',job)
        self.assertIn('timeout-minutes: 180',job)
        self.assertIn('permissions:\n      contents: read',job)
        job_env=job.split('\n    env:\n',1)[1].split('\n    steps:',1)[0]
        self.assertNotIn('runner.',job_env, 'GitHub does not allow runner context in jobs.<job>.env')
        self.assertIn("printf 'KU_PERFORMANCE_V2_ROOT=%s\\n' \"$RUNNER_TEMP/ku-performance-v2\" >> \"$GITHUB_ENV\"",job)
        self.assertNotIn('strategy:',job)
        self.assertIn("(inputs.scope != 'task-main-path' && inputs.scope != 'windows-task-main-path' && inputs.scope != 'performance-v2' && inputs.scope != 'performance-v2-builds' && inputs.scope != 'performance-v2-finish' && inputs.scope != 'performance-v2-validation' && inputs.scope != 'performance-followup-diagnostic' && inputs.scope != 'followup-screen')",text)
        self.assertIn("&& format('-{0}', inputs.scope) || ''",text)

    def test_fixed_toolchains_and_nonroot_build_precede_root_execution(self):
        _,job=self.workflow()
        self.assertIn('actions/checkout@de0fac2e4500dabe0009e67214ff5f5447ce83dd',job)
        self.assertIn('fetch-depth: 0',job);self.assertIn('persist-credentials: false',job)
        self.assertIn('1.89.0 --no-self-update --component llvm-tools-preview',job)
        self.assertIn('actions/setup-go@924ae3a1cded613372ab5595356fb5720e22ba16',job)
        self.assertIn("go-version: '1.27.2'",job);self.assertIn('cache: false',job)
        build=job.split('- name: Build frozen',1)[1].split('- name: Run the one',1)[0]
        run=job.split('- name: Run the one',1)[1].split('- name: Always archive',1)[0]
        self.assertIn('python3 -B tests/benchmarks/cross_stack/hosted_cycle.py --phase build',build)
        self.assertNotIn('sudo',build)
        self.assertIn('sudo -n python3 -B tests/benchmarks/cross_stack/hosted_cycle.py --phase "$performance_phase"',run)
        self.assertIn('performance_phases=(run)',run)
        self.assertIn('if [ "$KU_PERFORMANCE_V2_SCOPE" = performance-v2-builds ]; then\n            performance_phases=(compiler)',run)
        self.assertIn('elif [ "$KU_PERFORMANCE_V2_SCOPE" = performance-v2-finish ]; then\n            performance_phases=(tail compiler)',run)
        self.assertIn('elif [ "$KU_PERFORMANCE_V2_SCOPE" = performance-v2-validation ]; then\n            performance_phases=(lifecycle compiler)',run)
        self.assertIn('elif [ "$KU_PERFORMANCE_V2_SCOPE" = performance-followup-diagnostic ]; then\n            performance_phases=(diagnostic)\n            performance_build_set=diagnostic',run)
        self.assertIn('elif [ "$KU_PERFORMANCE_V2_SCOPE" = followup-screen ]; then\n            performance_phases=(followup-screen)\n            performance_build_set=followup-screen',run)
        self.assertIn('--build-set "$performance_build_set"',build)
        self.assertIn('--build-set "$performance_build_set"',run)
        self.assertIn('for performance_phase in "${performance_phases[@]}"; do',run)
        self.assertIn('set -euo pipefail',run)
        self.assertIn('KU_PERFORMANCE_V2_SCOPE: ${{ inputs.scope }}',run)
        self.assertIn('Dispatch these scopes serially',run)
        self.assertIn('timeout-minutes: ${{ inputs.memory_owner_probe && 3 || inputs.pg_failure_probe && 3 || inputs.normal_self_probe && 7 || inputs.pg_queue_screen && 8 || inputs.pg_driver_screen && 4 || inputs.http_wake_screen && 12 || inputs.final_task_json && 150 || 130 }}',run)
        for sdk in ('setup-node','setup-java','setup-dotnet','docker','gh release','git push'):
            self.assertNotIn(sdk,job)

    def test_memory_owner_probe_requires_fixed_reuse_and_only_one_diagnostic_artifact(self):
        text,job=self.workflow()
        self.assertIn('memory_owner_probe:\n',text)
        self.assertIn("KU_PERFORMANCE_V2_MEMORY_OWNER_PROBE: ${{ inputs.memory_owner_probe && 'true' || 'false' }}",job)
        self.assertEqual(job.count('performance_task_json_args+=(--memory-owner-probe)'),2)
        for value in ('41182804290','5976cad0c051b684823214f898cf746e2c7d63cd54d3116b04b509eba4c9dac6','4e5f5d94da277654862d8679f7ae4a7fda3f05ff'):
            self.assertIn(value,job)
        self.assertIn('if [ "$KU_PERFORMANCE_V2_MEMORY_OWNER_PROBE" != true ]; then test "$performance_build_set" = followup-screen; fi',job)
        self.assertIn('name: ku-memory-owner-observation',job)
        self.assertIn('inputs.memory_owner_probe && 3',job)

    def test_evidence_is_always_allowlisted_and_retained_for_one_day(self):
        _,job=self.workflow()
        archive=job.split('- name: Always archive',1)[1].split('- name: Retain one',1)[0]
        self.assertIn('if: always()',archive)
        self.assertIn('sudo -n python3 -B tests/benchmarks/cross_stack/hosted_cycle.py --phase archive',archive)
        self.assertEqual(job.count('uses: actions/upload-artifact@'),8)
        self.assertIn('retention-days: 1',job)
        self.assertIn('/ku-performance-v2/ku-performance-v2-evidence.tar.gz',job)
        self.assertIn('/ku-performance-v2/archive.json',job)
        self.assertIn('goal_status',archive)
        self.assertNotIn('path: ${{ runner.temp }}/ku-performance-v2\n',job)

    def test_pg_failure_probe_is_explicit_fresh_diagnostic_and_exports_only_small_receipts(self):
        text,job=self.workflow()
        self.assertIn('pg_failure_probe:\n',text)
        self.assertIn("KU_PERFORMANCE_V2_PG_FAILURE_PROBE: ${{ inputs.pg_failure_probe && 'true' || 'false' }}",job)
        verify=job.split('- name: Verify runner entry',1)[1].split('- name: Install only',1)[0]
        for check in ('test "$KU_PERFORMANCE_V2_SCOPE" = performance-followup-diagnostic',
                'test "$KU_PERFORMANCE_V2_TASK_JSON_SCREEN" = false',
                'test "$KU_PERFORMANCE_V2_COMMON_HTTP_SCREEN" = false',
                'test "$KU_PERFORMANCE_V2_FINAL_TASK_JSON" = false',
                'test -z "$KU_PERFORMANCE_V2_REUSE_RUN"','test -z "$KU_PERFORMANCE_V2_VARIANTS"'):
            self.assertIn(check,verify)
        self.assertEqual(job.count('performance_task_json_args+=(--pg-failure-probe)'),2)
        self.assertIn('timeout-minutes: ${{ inputs.memory_owner_probe && 3 || inputs.pg_failure_probe && 3 || inputs.normal_self_probe && 7 || inputs.pg_queue_screen && 8 || inputs.pg_driver_screen && 4 || inputs.http_wake_screen && 12 || inputs.final_task_json && 150 || 130 }}',job)
        small=job.split('- name: Retain the small redacted PG failure observation',1)[1]
        self.assertIn('if: always() && inputs.pg_failure_probe',small)
        self.assertIn('name: ku-pg-failure-observation',small)
        self.assertIn('path: ${{ runner.temp }}/ku-performance-v2/pg-failure-observation',small)
        self.assertNotIn('*.csv',small);self.assertNotIn('bin/',small)

    def test_normal_self_probe_is_explicit_exclusive_fresh_and_bounded(self):
        text,job=self.workflow()
        self.assertIn('normal_self_probe:\n',text)
        self.assertIn("KU_PERFORMANCE_V2_NORMAL_SELF_PROBE: ${{ inputs.normal_self_probe && 'true' || 'false' }}",job)
        verify=job.split('- name: Verify runner entry',1)[1].split('- name: Install only',1)[0]
        for check in ('test "$KU_PERFORMANCE_V2_SCOPE" = performance-followup-diagnostic',
                'test "$KU_PERFORMANCE_V2_PG_FAILURE_PROBE" = false',
                'test "$KU_PERFORMANCE_V2_TASK_JSON_SCREEN" = false',
                'test "$KU_PERFORMANCE_V2_COMMON_HTTP_SCREEN" = false',
                'test "$KU_PERFORMANCE_V2_FINAL_TASK_JSON" = false',
                'test -z "$KU_PERFORMANCE_V2_REUSE_RUN"','test -z "$KU_PERFORMANCE_V2_VARIANTS"'):
            self.assertIn(check,verify)
        self.assertEqual(job.count('performance_task_json_args+=(--normal-self-probe)'),2)
        self.assertIn('timeout-minutes: ${{ inputs.memory_owner_probe && 3 || inputs.pg_failure_probe && 3 || inputs.normal_self_probe && 7 || inputs.pg_queue_screen && 8 || inputs.pg_driver_screen && 4 || inputs.http_wake_screen && 12 || inputs.final_task_json && 150 || 130 }}',job)
        small=job.split('- name: Retain the small redacted normal SELF observation',1)[1]
        self.assertIn('if: always() && inputs.normal_self_probe',small)
        self.assertIn('name: ku-normal-self-observation',small)
        self.assertIn('path: ${{ runner.temp }}/ku-performance-v2/normal-self-observation',small)
        self.assertNotIn('*.csv',small);self.assertNotIn('bin/',small)

    def test_normal_perf_preflight_blocks_build_before_any_six_window_capture(self):
        _,job=self.workflow()
        preflight=job.split('- name: Verify acknowledged perf control before any normal SELF build',1)[1].split('\n      - name:',1)[0]
        self.assertIn('if: inputs.normal_self_probe',preflight);self.assertIn('timeout-minutes: 1',preflight)
        self.assertIn('--phase perf-preflight --build-set diagnostic',preflight);self.assertIn('--normal-self-probe',preflight)
        self.assertLess(job.index('Verify acknowledged perf control'),job.index('Build frozen sources and plans'))
        self.assertNotIn('continue-on-error',preflight)
        artifact=job.split('- name: Retain the isolated perf control preflight',1)[1].split('\n      - name:',1)[0]
        self.assertIn('if: always() && inputs.normal_self_probe',artifact);self.assertIn('name: ku-normal-perf-preflight',artifact)

    def test_http_wake_screen_is_explicit_frozen_fresh_and_bounded(self):
        text,job=self.workflow()
        self.assertIn('http_wake_screen:\n',text)
        self.assertIn("KU_PERFORMANCE_V2_HTTP_WAKE_SCREEN: ${{ inputs.http_wake_screen && 'true' || 'false' }}",job)
        verify=job.split('- name: Verify runner entry',1)[1].split('- name: Install only',1)[0]
        mode=verify.split('if [ "$KU_PERFORMANCE_V2_HTTP_WAKE_SCREEN" = true ]; then',1)[1].split('fi',1)[0]
        for check in ('test "$KU_PERFORMANCE_V2_SCOPE" = performance-followup-diagnostic',
                'test "$KU_PERFORMANCE_V2_PREPATCH" = b765f9bc5d3a6e71353a562e27cef7b4cbb35f69',
                'test "$KU_PERFORMANCE_V2_PG_DRIVER_SCREEN" = false','test "$KU_PERFORMANCE_V2_PG_QUEUE_SCREEN" = false',
                'test "$KU_PERFORMANCE_V2_NORMAL_SELF_PROBE" = false','test "$KU_PERFORMANCE_V2_PG_FAILURE_PROBE" = false',
                'test "$KU_PERFORMANCE_V2_TASK_JSON_SCREEN" = false','test "$KU_PERFORMANCE_V2_COMMON_HTTP_SCREEN" = false',
                'test "$KU_PERFORMANCE_V2_FINAL_TASK_JSON" = false','test -z "$KU_PERFORMANCE_V2_REUSE_RUN"',
                'test -z "$KU_PERFORMANCE_V2_VARIANTS"'):self.assertIn(check,mode)
        self.assertEqual(job.count('performance_task_json_args+=(--http-wake-screen)'),2)
        self.assertIn('inputs.http_wake_screen && 12',job)
        self.assertIn('if: always() && inputs.http_wake_screen',job)
        self.assertIn('name: ku-http-wake-observation',job)
        self.assertIn('path: ${{ runner.temp }}/ku-performance-v2/http-wake-observation',job)

    def test_pg_driver_screen_is_fresh_exclusive_and_never_a_capacity_ranking(self):
        text,job=self.workflow()
        self.assertIn('pg_driver_screen:\n',text)
        self.assertIn("KU_PERFORMANCE_V2_PG_DRIVER_SCREEN: ${{ inputs.pg_driver_screen && 'true' || 'false' }}",job)
        verify=job.split('- name: Verify runner entry',1)[1].split('- name: Install only',1)[0]
        mode=verify.split('if [ "$KU_PERFORMANCE_V2_PG_DRIVER_SCREEN" = true ]; then',1)[1].split('fi',1)[0]
        for check in ('test "$KU_PERFORMANCE_V2_SCOPE" = performance-followup-diagnostic',
                'test "$KU_PERFORMANCE_V2_PG_QUEUE_SCREEN" = false','test "$KU_PERFORMANCE_V2_NORMAL_SELF_PROBE" = false',
                'test "$KU_PERFORMANCE_V2_PG_FAILURE_PROBE" = false','test "$KU_PERFORMANCE_V2_TASK_JSON_SCREEN" = false',
                'test "$KU_PERFORMANCE_V2_COMMON_HTTP_SCREEN" = false','test "$KU_PERFORMANCE_V2_FINAL_TASK_JSON" = false',
                'test -z "$KU_PERFORMANCE_V2_REUSE_RUN"','test -z "$KU_PERFORMANCE_V2_VARIANTS"'):self.assertIn(check,mode)
        self.assertEqual(job.count('performance_task_json_args+=(--pg-driver-screen)'),2)
        self.assertIn('if: always() && inputs.pg_driver_screen',job)
        self.assertIn('name: ku-pg-driver-observation',job)
        self.assertIn('path: ${{ runner.temp }}/ku-performance-v2/pg-driver-observation',job)

    def test_pg_queue_screen_is_explicit_frozen_and_never_a_formal_matrix(self):
        text,job=self.workflow()
        self.assertIn('pg_queue_screen:\n',text)
        self.assertIn("KU_PERFORMANCE_V2_PG_QUEUE_SCREEN: ${{ inputs.pg_queue_screen && 'true' || 'false' }}",job)
        verify=job.split('- name: Verify runner entry',1)[1].split('- name: Install only',1)[0]
        for check in ('test "$KU_PERFORMANCE_V2_SCOPE" = performance-followup-diagnostic',
                'test "$KU_PERFORMANCE_V2_PREPATCH" = f39c673df0cd08232f184141a7e2aedef4f585c9',
                'test "$KU_PERFORMANCE_V2_NORMAL_SELF_PROBE" = false','test "$KU_PERFORMANCE_V2_PG_FAILURE_PROBE" = false',
                'test "$KU_PERFORMANCE_V2_TASK_JSON_SCREEN" = false','test "$KU_PERFORMANCE_V2_COMMON_HTTP_SCREEN" = false',
                'test "$KU_PERFORMANCE_V2_FINAL_TASK_JSON" = false','test -z "$KU_PERFORMANCE_V2_REUSE_RUN"',
                'test -z "$KU_PERFORMANCE_V2_VARIANTS"'):self.assertIn(check,verify)
        self.assertEqual(job.count('performance_task_json_args+=(--pg-queue-screen)'),2)
        self.assertIn('timeout-minutes: ${{ inputs.memory_owner_probe && 3 || inputs.pg_failure_probe && 3 || inputs.normal_self_probe && 7 || inputs.pg_queue_screen && 8 || inputs.pg_driver_screen && 4 || inputs.http_wake_screen && 12 || inputs.final_task_json && 150 || 130 }}',job)
        self.assertIn('if: always() && inputs.pg_queue_screen',job)
        self.assertIn('name: ku-pg-queue-observation',job)
        self.assertIn('path: ${{ runner.temp }}/ku-performance-v2/pg-queue-observation',job)

    def test_actual_pg_queue_regressions_are_required_on_three_platforms_and_linux_detectors(self):
        text,_=self.workflow()
        task=text.split('\n  task-main-path:\n',1)[1].split('\n  task-main-path-linux-arm64:\n',1)[0]
        serial=task.split('- name: Run affected native HTTP and Task regressions serially',1)[1].split('- name: Check scalar growth',1)[0]
        self.assertIn('--test native_pg_query_poll_test',serial)
        detector=task.split('- name: Check scalar growth',1)[1].split('\n      - name:',1)[0]
        self.assertIn('KU_CC: "clang -fsanitize=address,undefined',detector)
        self.assertIn('ASAN_OPTIONS: "detect_leaks=1:halt_on_error=1"',detector)
        self.assertEqual(detector.count('native_pg_async_queued_ -- --'),2)
        self.assertIn('-- --list > "$RUNNER_TEMP/ku-pg-queue-tests.list"',detector)
        self.assertIn('test "$(grep -c',detector);self.assertIn('-eq 5',detector)


class BatchWindowTests(unittest.TestCase):
    """Synthetic outputs exercise gates without running the large CPU workload."""

    def test_formal_input_limits_and_sampling_are_frozen_in_protocol(self):
        root = Path(__file__).parent
        contract = json.loads((root / 'protocol.json').read_text())['cpu_algorithm']['batch']
        self.assertEqual(contract['fields'], {'iterations': batch.ITERATIONS,
                                              'seed': batch.SEED, 'repeat': batch.REPEAT})
        self.assertEqual(batch.REPEAT, 32768)
        self.assertEqual(contract['formal_minimum_process_wall_seconds'], batch.MIN_PROCESS_WALL_SECONDS)
        self.assertEqual(contract['formal_minimum_compute_ms_per_process'], batch.MIN_COMPUTE_MS)
        self.assertEqual(contract['process_group_absolute_deadline_seconds'], batch.PROCESS_DEADLINE_SECONDS)
        self.assertEqual(contract['maximum_bytes_per_stdout_or_stderr'], batch.STREAM_LIMIT)
        self.assertEqual(contract['resource_sampling_interval_seconds'], batch.SAMPLE_INTERVAL_SECONDS)
        self.assertEqual(contract['maximum_resource_samples'], batch.MAX_RESOURCE_SAMPLES)
        self.assertGreater(batch.MAX_RESOURCE_SAMPLES * batch.SAMPLE_INTERVAL_SECONDS,
                           batch.PROCESS_DEADLINE_SECONDS)
        for source, repeat in [('ku/batch.ku', 'repeat'), ('go/main.go', 'input.Repeat'),
                               ('java/src/main/java/ku/bench/Main.java', 'repeat'),
                               ('dotnet/Program.cs', 'repeat'), ('cpp/main.cc', 'repeat')]:
            with self.subTest(source=source):
                text = (root / source).read_text()
                self.assertIn(repeat + ' > 32768', text)
                self.assertNotIn(repeat + ' > 1024', text)
                self.assertIn('1024', text)  # Unrelated seed/HTTP bounds remain.

    def test_actual_wall_and_every_compute_duration_required_but_cold_is_separate(self):
        qualifies = lambda wall, timers: batch.formal_duration_eligibility(
            batch.REPEAT, wall, timers, 4)['eligible']
        self.assertTrue(qualifies(30, [30000] * 4))
        self.assertTrue(qualifies(600, [30000] * 4))
        for wall, timers in [(29.999, [30000] * 4), (600.001, [30000] * 4),
                             (30, [30000, 29999.9, 30000, 30000]), (30, [30000] * 3),
                             (30, []), (True, [30000] * 4),
                             (float('nan'), [30000] * 4), (30, [float('inf')] * 4)]:
            with self.subTest(wall=wall, timers=timers):
                self.assertFalse(qualifies(wall, timers))
        cold = batch.formal_duration_eligibility(0, .01, [0], 1)
        self.assertFalse(cold['applicable'])
        self.assertFalse(cold['eligible'])
        self.assertEqual(len(cold['reasons']), 1)
        legacy = batch.formal_duration_eligibility(64, 40, [40000], 1)
        self.assertFalse(legacy['eligible'])
        self.assertIn('R&D', legacy['reasons'][0])

    def test_validates_entire_large_output_and_exact_one_mib_stream_limit(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / 'stdout.json'
            value = {'checksums': [batch.EXPECTED] * batch.REPEAT, 'compute_ms': 30000}
            encoded = json.dumps(value)
            self.assertLess(len(encoded), batch.STREAM_LIMIT)
            path.write_text(encoded + ' ' * (batch.STREAM_LIMIT - len(encoded)))
            self.assertEqual(len(batch.parse_result(path, batch.REPEAT)['checksums']), batch.REPEAT)
            with path.open('a') as stream:
                stream.write(' ')
            with self.assertRaisesRegex(ValueError, '1-MiB'):
                batch.parse_result(path, batch.REPEAT)
            for wrong in (0, True, float(batch.EXPECTED)):
                value['checksums'][-1] = wrong
                path.write_text(json.dumps(value))
                with self.assertRaisesRegex(ValueError, 'incorrect CPU checksum'):
                    batch.parse_result(path, batch.REPEAT)

    def test_short_formal_result_is_retained_and_stops_later_rounds(self):
        with tempfile.TemporaryDirectory() as directory:
            calls = []

            def synthetic_group(harness, stack, cores, repeat, round_index, output):
                calls.append((stack, round_index, repeat))
                return {'identity': {'stack': stack, 'cores': cores, 'repeat': repeat},
                        'passed': True, 'formal_eligible': False, 'process_wall_seconds': .1,
                        'formal_duration': {'reasons': ['short formal compute window']}}

            with patch.object(batch, 'ROUNDS', 2), \
                    patch.object(batch, 'expected_checksum', return_value=batch.EXPECTED), \
                    patch.object(batch, 'run_group', side_effect=synthetic_group):
                report = batch.run(None, directory, stacks=('ku',))
            self.assertEqual(calls, [('ku', 0, 0), ('ku', 0, batch.REPEAT)])
            self.assertFalse(report['passed'])
            self.assertEqual(len(report['rows']), 2)
            self.assertEqual(len(report['skipped']), 1)
            self.assertEqual(report['summaries'][0]['valid_formal_rounds'], 0)
            self.assertEqual(report['summaries'][0]['failed_formal_rounds'], 1)
            self.assertTrue((Path(directory) / 'summary.json').exists())


class JsonDenseEntryTests(unittest.TestCase):
    """Check the retained entry's input/accounting contract without running C."""

    def test_http_fixture_and_earlier_twenty_inputs_keep_their_exact_identity(self):
        rows = json_dense.make_cases()
        frozen = Path(__file__).with_name('corpora').joinpath('object.json').read_bytes()
        http = [row for row in rows if row['family'] == 'http-object']
        self.assertEqual(len(http), 16)
        self.assertEqual(len(frozen), 1048)
        self.assertTrue(all(row['input'].encode() == frozen and len(row['rid']) == 16 for row in http))
        self.assertEqual(len({row['rid'] for row in http}), 16)
        # SHA of retained raw/dense/cases.json, with only its original family
        # labels restored. No old raw bundle is needed to reproduce those inputs.
        legacy = copy.deepcopy([row for row in rows if row['family'] == 'variants'])
        for index, row in enumerate(legacy):
            row['family'] = 'object' if index < 16 else 'array' if index < 18 else 'escape'
        self.assertEqual(json_dense.case_identity(legacy),
                         '467262874ec13458f56b2ef238bdebfe255d52a5f73c186dac92b3f2d13562ed')
        self.assertEqual(set(row['family'] for row in rows), set(json_dense.FAMILIES))
        self.assertEqual(json_dense.REQUEST_TIMEOUT_MS, 30000)
        self.assertEqual(json_dense.HANDLER_TIMEOUT_MS, 45000)

    @staticmethod
    def result(rows, family='ascii'):
        value = {'schema': json_dense.SCHEMA, 'family': family, 'batch_size': 2048,
                 'context': 'http', 'request_timeout_ms': 30000, 'handler_timeout_ms': 45000,
                 'iterations': 0, 'correct': 0, 'thread_cpu_ns': 0, 'wall_ns': 0,
                 'setup_thread_cpu_ns': 100, 'setup_wall_ns': 200, 'stop_reason': 'validation_only',
                 'golden_cleanup_thread_cpu_ns': 10, 'golden_cleanup_wall_ns': 20,
                 'validation': [{'case': index, 'body_hex': json.dumps(row['expected'],
                                 ensure_ascii=False).encode().hex()}
                                for index, row in enumerate(rows) if row['family'] == family]}
        return value

    def test_semantics_are_independent_of_candidate_golden_bytes(self):
        rows = json_dense.make_cases()
        value = self.result(rows)
        parse = lambda x, **kw: json_dense.validate_output(
            json.dumps(x, separators=(',', ':')), rows, 'ascii', **kw)
        checked = parse(value)
        self.assertFalse(checked['performance_measured'])
        self.assertIsNone(checked['ns_per_correct'])
        self.assertEqual(checked['semantic_preflight_correct'], 8)
        for kind in ('counter', 'bool', 'missing', 'duplicate', 'context', 'clock', 'repeat'):
            with self.subTest(kind=kind):
                changed = copy.deepcopy(value)
                if kind in ('counter', 'bool'):
                    body = json.loads(bytes.fromhex(changed['validation'][0]['body_hex']))
                    if kind == 'counter':
                        body['counter'] -= 1
                    else:
                        body['data']['items'][0] = 1  # True must not accept an integer.
                    changed['validation'][0]['body_hex'] = json.dumps(body).encode().hex()
                elif kind == 'missing':
                    changed['validation'].pop()
                elif kind == 'duplicate':
                    changed['validation'].append(changed['validation'][0])
                elif kind == 'context':
                    changed['context'] = 'none'
                elif kind == 'clock':
                    changed['thread_cpu_ns'] = True
                else:
                    changed.update(correct=1, iterations=1, stop_reason='iterations')
                with self.assertRaises(ValueError):
                    parse(changed)
        value.update(correct=17, iterations=17, thread_cpu_ns=1700, stop_reason='iterations')
        self.assertEqual(parse(value, repeat=17)['ns_per_correct'], 100)
        with self.assertRaisesRegex(ValueError, 'fixed repeat incomplete'):
            parse(value, repeat=18)
        value.update(context='none', stop_reason='thread_cpu_budget', thread_cpu_ns=1_000_000_000)
        self.assertIn('mechanism-only', parse(value, cpu_seconds=1, context='none')['deadline_context'])

    def test_layout_contract_accepts_inline_q1_and_cold_q2_without_resident_claims(self):
        # Synthetic protocol fixtures, not compiled target sizeof measurements.
        for job, config, cold, free in ((744, 360, 0, 0), (528, 136, 0, 4), (352, 136, 184, 4)):
            with self.subTest(job=job, cold=cold, free=free):
                offsets = dict(generation=0, output_charge=24, ticket=32, result=96,
                               exists_value=232, pg_operation=240, pg_pool=248, pg_ready=256,
                               net_deadline=264, net_requested=272, net_client=280,
                               net_write=288, net_owner=320, net_addresses=328)
                if cold:
                    offsets['inputs'] = 64
                else:
                    offsets.update(path=56, content=64, net_config=264, net_present=264+config)
                layout = {'schema': 'ku-json-dense-layout-v1', 'available': True,
                          'resident_memory_measurement': False, 'fixed_capacity': 2048,
                          'sizeof': {'void_pointer': 8, 'KuString': 32, 'KuValue': 40,
                                     'KuTaskBlockingJobV1': job, 'KuTaskBlockingPoolV1': 2800,
                                     'net_config': config, 'ring_cell': 8,
                                     'KuTaskBlockingInputsV1': cold, 'free_index_cell': free},
                          'features': {'KU_TASK_BLOCKING_FILES': 0, 'KU_TASK_BLOCKING_DIRECTORY': 0,
                                       'KU_TASK_BLOCKING_NETWORK': 1, 'KU_TASK_BLOCKING_PG': 1},
                          'offsetof': offsets,
                          'input_offsetof': dict(path=0, content=8, net_config=40, net_present=176) if cold else {},
                          'fixed_bytes': 2800+2048*(job+cold+8+free)}
                self.assertEqual(json_dense.validate_layout(layout), layout)
                wrong = copy.deepcopy(layout)
                wrong['fixed_bytes'] -= 2048*4
                with self.assertRaisesRegex(ValueError, 'fixed byte formula'):
                    json_dense.validate_layout(wrong)
                wrong = copy.deepcopy(layout)
                wrong['resident_memory_measurement'] = True
                with self.assertRaisesRegex(ValueError, 'layout metadata'):
                    json_dense.validate_layout(wrong)
                if cold:
                    wrong = copy.deepcopy(layout)
                    wrong['input_offsetof']['content'] = cold
                    with self.assertRaisesRegex(ValueError, 'cold input offsetof'):
                        json_dense.validate_layout(wrong)

    def test_emission_changes_only_main_name_and_appends_wrapper(self):
        with tempfile.TemporaryDirectory() as directory:
            source = Path(directory) / 'server.c'
            runtime = ('/* ku_object_try_set_owned ku_drop_struct___ku_http_request '
                       'ku_result_drop_struct___ku_http_response __ku_handler_timeout_begin '
                       'ku_http_deadline_ms ku_http_now_ms __ku_sync_reset __ku_sync_finish_request */\n'
                       'KuResult_struct___ku_http_response encode(KuStruct___ku_http_request req) {\n'
                       '  /* unchanged ordinary generated handler */\n}\n'
                       'int main(void) {\n  /* unchanged ordinary startup */\n}\n')
            source.write_text(runtime)
            emitted = json_dense.emit(source, json_dense.make_cases(),
                                      Path(__file__).with_name('json_dense.c.inc'))
            expected = runtime.replace('int main(void) {', 'static int ku_json_dense_original_main(void) {')
            self.assertTrue(emitted.startswith(expected))
            self.assertEqual(source.read_text(), runtime)
            self.assertIn('KuResult_struct___ku_http_response result=encode(request);', emitted)
            self.assertIn('int expired=__ku_sync_finish_request(&signal);', emitted)
            self.assertIn('handler_ms>remaining ? remaining:handler_ms', emitted)
            self.assertIn('#define KU_JSON_DENSE_HAS_COLD_INPUTS 0', emitted)
            self.assertIn('#define KU_JSON_DENSE_HAS_FREE_INDEX 0', emitted)
            source.write_text('typedef struct KuTaskBlockingInputsV1 { /* fixture */ } KuTaskBlockingInputsV1;\n'
                              '/* pool member */ uint32_t* free_indices;\n'+runtime)
            later = json_dense.emit(source, [], Path(__file__).with_name('json_dense.c.inc'))
            self.assertIn('#define KU_JSON_DENSE_HAS_COLD_INPUTS 1', later)
            self.assertIn('#define KU_JSON_DENSE_HAS_FREE_INDEX 1', later)
            source.write_text(runtime.replace('encode(KuStruct___ku_http_request req)', 'different(req)'))
            with self.assertRaisesRegex(ValueError, 'ordinary generated'):
                json_dense.emit(source, [], Path(__file__).with_name('json_dense.c.inc'))

    def test_explicit_phase_ledger_and_http_control_remain_diagnostic_only(self):
        source = ('typedef struct KuString { /* ordinary ABI */ } KuString;\n'
                  'KuResult_struct___ku_http_response encode(KuStruct___ku_http_request req) {\n'
                  '  KuResult_kuvalue decoded=ku_json_parse(req.body);\n'
                  '  KuResult_str encoded=ku_json_stringify(decoded.value);\n'
                  '  ku_object_drop(NULL);\n'
                  '  ku_drop_struct___ku_http_request(&req);\n'
                  '  return result;\n'
                  '__ku_sync_epilogue:;\n'
                  '  return fallback;\n}\n'
                  'int main(void) { return original_startup(); }\n')
        phases = json_dense.instrument_phases(source)
        self.assertIn('normal_score_eligible', phases)
        self.assertIn('ku_dense_decode(req.body)', phases)
        self.assertIn('ku_dense_encode(decoded.value)', phases)
        self.assertIn('ku_dense_object_drop(NULL)', phases)
        self.assertIn('ku_dense_request_drop(&req)', phases)
        self.assertEqual(phases.count('ku_dense_change_phase(ku_dense_outer);'), 2)
        self.assertIn('allocator_retained_bytes', phases)
        self.assertIn('ku_perf_allocate', phases)  # Existing shared allocation hook.
        # Generated insertion guards close at column zero too; a textual
        # first "\n}\n" would truncate the Transform before its encode call.
        nested = source.replace('  KuResult_str encoded=',
            '  if (condition) {\n    text="} /* not a closing brace */";\n}\n  KuResult_str encoded=')
        self.assertIn('ku_dense_encode(decoded.value)', json_dense.instrument_phases(nested))
        with self.assertRaisesRegex(ValueError, 'boundaries changed'):
            json_dense.instrument_phases(source.replace('ku_json_parse(', 'different('))
        control, identity = json_dense.make_http_control_source(source, json_dense.make_cases())
        self.assertTrue(identity['diagnostic_only'])
        self.assertFalse(identity['normal_score_eligible'])
        self.assertEqual(identity['input_body_bytes'], 1048)
        self.assertEqual(identity['output_body_bytes'], 1071)
        self.assertEqual(identity['rid_bytes'], 16)
        self.assertIn('memcpy(body.ptr+', control)
        self.assertIn('rid.ptr,16u)', control)
        self.assertIn('body.storage=KU_STRING_OWNED', control)
        self.assertIn('malloc(sizeof(template_body))', control)
        self.assertNotIn('ku_json_parse(req.body)', control)
        self.assertIn('int main(void) { return original_startup(); }', control)

    def test_http_control_cli_supplies_clean_environment_to_compiler(self):
        source=('KuResult_struct___ku_http_response encode(KuStruct___ku_http_request req) {\n'
                '  KuResult_kuvalue decoded=ku_json_parse(req.body);\n'
                '  KuResult_str encoded=ku_json_stringify(decoded.value);\n'
                '  return result;\n__ku_sync_epilogue:;\n  return fallback;\n}\n'
                'int main(void) { return original_startup(); }\n')
        calls=[]
        def compile_only(commands,label,argv,cwd,**kwargs):
            calls.append((label,argv,dict(commands.environment)))
            Path(argv[-1]).write_bytes(b'explicit fake compiler output; no executable started')
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory);generated=root/'normal.c';generated.write_text(source)
            with patch.object(json_dense.Commands,'run',new=compile_only),patch.dict(os.environ,{'CFLAGS':'-O3','LLVM_PROFILE_FILE':'unexpected'}),patch('builtins.print'):
                json_dense.main(['--generated-c',str(generated),'--output',str(root/'control'),
                    '--source-commit','a'*40,'--cli-sha256','b'*64,'--http-control'])
            self.assertEqual(len(calls),1)
            self.assertIn('-O2',calls[0][1]);self.assertNotIn('CFLAGS',calls[0][2])
            self.assertNotIn('LLVM_PROFILE_FILE',calls[0][2])
            receipt=json.loads((root/'control/emission.json').read_text())
            self.assertTrue(receipt['diagnostic_only']);self.assertFalse(receipt['normal_score_eligible'])

    def test_phase_output_cannot_silently_become_plain_performance(self):
        rows = json_dense.make_cases()
        value = self.result(rows)
        phases = ['request_projection', 'object_and_control', 'decode', 'encode', 'drop', 'validation']
        value['phase_ledger'] = {'diagnostic_only': True, 'normal_score_eligible': False,
            'exclusive': True, 'allocation_hook_enabled': True,
            'rows': [{'phase': p, 'gross_thread_cpu_ns': 0, 'segments': 0} for p in phases],
            'phase_clock_reads': 0, 'clock_calibration_reads': 20000,
            'clock_calibration_ns': 1000000, 'live_after_cleanup_bytes': 0}
        parse = lambda x, **kw: json_dense.validate_output(
            json.dumps(x, separators=(',', ':')), rows, 'ascii', **kw)
        self.assertTrue(parse(value, phase_mode='allocation')['phase_ledger']['diagnostic_only'])
        with self.assertRaisesRegex(ValueError, 'plain dense'):
            parse(value)
        malformed = copy.deepcopy(value)
        malformed['phase_ledger']['rows'][2]['segments'] = 1
        with self.assertRaisesRegex(ValueError, 'denominator'):
            parse(malformed, phase_mode='allocation')
        malformed = copy.deepcopy(value)
        malformed['phase_ledger']['live_after_cleanup_bytes'] = 1
        with self.assertRaisesRegex(ValueError, 'return to zero'):
            parse(malformed, phase_mode='allocation')
        with self.assertRaisesRegex(ValueError, 'mismatched'):
            parse(value, phase_mode='clocks')


class ProfileReceiptTests(unittest.TestCase):
    def variant_fixture(self, root):
        tools = root / 'tools'
        manifest = {'candidate_base': 'a' * 40, 'source_root': str(root),
                    'source_inventory_sha256': 'b' * 64,
                    'stacks': {'ku': {'command': ['unchanged-ku']}}}
        for stack, filename, compiler in [('go', 'pg_mode.go', 'go/bin/go'),
                                          ('rust', 'src/pg_mode.rs', 'cargo/bin/rustc')]:
            source = root / 'tests/benchmarks/cross_stack' / stack / filename
            source.parent.mkdir(parents=True)
            source.write_text('fixture source identity\n')
            binary = root / (stack + '-normal')
            binary.write_bytes(b'normal binary; never executed\n')
            launcher = tools / compiler
            launcher.parent.mkdir(parents=True)
            launcher.write_bytes(b'compiler fixture; never executed\n')
            manifest['stacks'][stack] = {'command': [str(binary)], 'env': {}, 'build': {
                'binary_sha256': compile_variants.digest(binary),
                'flags': 'normal flags only', 'pg_modes': ['normal', 'reset-equivalent'],
                'source_files_sha256': {filename: compile_variants.digest(source)}}}
        path = root / 'manifest.json'
        path.write_text(json.dumps(manifest))
        return path, tools

    @staticmethod
    def fake_compile(commands, label, command, cwd, env=None, timeout=900):
        # The real variant manifest builder runs, but no compiler or workload does.
        if label == 'go':
            binary = Path(command[command.index('-o') + 1])
        elif label == 'rust':
            binary = Path(command[command.index('--target-dir') + 1]) / 'release/ku-cross-stack-rust'
        else:
            return 'fixture compiler version'
        binary.parent.mkdir(parents=True, exist_ok=True)
        binary.write_bytes(b'new variant binary; never executed\n')
        return ''

    def test_tuned_pg_manifests_keep_verified_modes_and_new_binary_identity(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            original, tools = self.variant_fixture(root)
            before = original.read_bytes()
            current = original
            for stack in ('go', 'rust'):
                output = root / (stack + '-variant')
                with patch.object(compile_variants.Commands, 'run', new=self.fake_compile):
                    receipt = compile_variants.build(current, output, tools, stack, 'normal')
                self.assertTrue(receipt['passed'])
                current = output / 'manifest.json'
                derive(current, root / ('verified-' + stack), 'normal')
                spec = json.loads(current.read_text())['stacks'][stack]
                baseline = json.loads(before)['stacks'][stack]['build']
                self.assertEqual(spec['build']['pg_modes'], baseline['pg_modes'])
                self.assertEqual(spec['build']['source_files_sha256'], baseline['source_files_sha256'])
                self.assertEqual(spec['build']['binary_sha256'], compile_variants.digest(spec['command'][0]))
                self.assertNotEqual(spec['build']['binary_sha256'], baseline['binary_sha256'])
                self.assertNotIn('flags', spec['build'])
                self.assertEqual(spec['build']['normal_flags_reference'], baseline)
            for mode in ('normal', 'reset-equivalent'):
                result = json.loads(derive(current, root / ('pg-' + mode), mode).read_text())
                for stack in ('go', 'rust'):
                    self.assertEqual(result['stacks'][stack]['env']['BENCH_PG_MODE'], mode)
            self.assertEqual(original.read_bytes(), before)

    def test_tuned_pg_contract_rejects_changed_or_unattested_source(self):
        for stack in ('go', 'rust'):
            for change in ('source', 'inventory', 'modes'):
                with self.subTest(stack=stack, change=change), tempfile.TemporaryDirectory() as directory:
                    root = Path(directory)
                    original, tools = self.variant_fixture(root)
                    manifest = json.loads(original.read_text())
                    reference = manifest['stacks'][stack]['build']
                    if change == 'source':
                        filename = next(iter(reference['source_files_sha256']))
                        (root / 'tests/benchmarks/cross_stack' / stack / filename).write_text('changed source\n')
                    elif change == 'inventory':
                        reference.pop('source_files_sha256')
                    else:
                        reference['pg_modes'] = ['normal']
                    original.write_text(json.dumps(manifest))
                    with patch.object(compile_variants.Commands, 'run') as compiler:
                        with self.assertRaisesRegex(ValueError, 'PG mode source identity mismatch'):
                            compile_variants.build(original, root / 'variant', tools, stack, 'normal')
                    compiler.assert_not_called()
                    self.assertFalse((root / 'variant/manifest.json').exists())

    def test_legacy_variant_cannot_invent_pg_mode_support(self):
        for stack in ('go', 'rust'):
            with self.subTest(stack=stack), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                original, tools = self.variant_fixture(root)
                manifest = json.loads(original.read_text())
                manifest['stacks'][stack]['build'].pop('pg_modes')
                original.write_text(json.dumps(manifest))
                output = root / 'variant'
                with patch.object(compile_variants.Commands, 'run', new=self.fake_compile):
                    receipt = compile_variants.build(original, output, tools, stack, 'normal')
                self.assertTrue(receipt['passed'])
                actual = json.loads((output / 'manifest.json').read_text())['stacks'][stack]['build']
                self.assertNotIn('pg_modes', actual)
                with self.assertRaisesRegex(ValueError, 'no attested'):
                    derive(output / 'manifest.json', root / 'pg', 'reset-equivalent')

    def test_requires_complete_exit_matching_source_and_held_out_inputs(self):
        with tempfile.TemporaryDirectory() as directory:
            profile = Path(directory)/'profile.data'
            profile.write_bytes(b'owned test profile')
            receipt = {'passed':True,'domain_removed':True,'stack':'ku', 'source_fingerprint':'source', 'normal_exit':True,
                'all_business_results_correct':True, 'profile_complete':True,
                'forced_termination':False, 'training_plan_sha256':'a'*64,
                'validation_plan_sha256':'b'*64, 'training_input_sha256':'c'*64,
                'validation_input_sha256':'d'*64, 'profile_path':str(profile),
                'profile_sha256':hashlib.sha256(profile.read_bytes()).hexdigest()}
            path = Path(directory)/'training.json'
            path.write_text(json.dumps(receipt))
            self.assertEqual(profile_input(path,'ku','source')[0],profile.resolve())
            for change in [{'passed':False}, {'domain_removed':False}, {'normal_exit':False}, {'forced_termination':True},
                           {'all_business_results_correct':False}, {'profile_complete':False},
                           {'source_fingerprint':'other'}, {'profile_sha256':'0'*64},
                           {'training_input_sha256':None}, {'validation_input_sha256':'c'*64},
                           {'validation_plan_sha256':'a'*64}]:
                with self.subTest(change=change):
                    changed = copy.deepcopy(receipt)
                    changed.update(change)
                    path.write_text(json.dumps(changed))
                    with self.assertRaises(ValueError):
                        profile_input(path,'ku','source')

    def test_training_and_validation_have_independent_business_inputs(self):
        self.assertNotEqual(identity(plan('train')), identity(plan('validate')))
        self.assertNotEqual(identity(PLANS['train']), identity(PLANS['validate']))
        self.assertEqual(expected(PLANS['train']), 22944003)
        with tempfile.TemporaryDirectory() as directory:
            path=Path(directory)/'output.json'
            value=dict(PLANS['validate'],repeat=2)
            checksums=[expected(value)]*2
            path.write_text(json.dumps({'checksums':checksums,'compute_ms':1}))
            self.assertEqual(parse_output(path,value)['correct_computations'],2)
            for wrong in ([True,True], [float(checksums[0])]*2, checksums[:1], [0,0]):
                path.write_text(json.dumps({'checksums':wrong,'compute_ms':1}))
                with self.assertRaises(ValueError):
                    parse_output(path,value)

    def test_expansion_preserves_default_async_identity_and_only_changes_selected_handlers(self):
        source=Path(__file__).with_name('ku').joinpath('server.ku').read_text()
        current=source.replace('__PEER_PORT__','19191')
        self.assertEqual(expand_ku_server(source,19191),current)
        self.assertIn('    async fn database(req: HttpRequest): HttpResponse! {',current)
        self.assertEqual(current.count('result.value_int('),2)
        response=expand_ku_server(source,19191,json_mode='response')
        encode_body=lambda x:x.split('fn encode(',1)[1].split('async fn work(',1)[0]
        decode='    decoded = json.decode(req.body)?'
        counter='    counter = decoded["counter"]?.as_int()?'
        take='    data = decoded.take("data")?'
        for expanded in (current,response):
            handler=encode_body(expanded)
            for step in (decode,counter,take):
                self.assertEqual(handler.count(step),1)
            self.assertLess(handler.index(decode),handler.index(counter))
            self.assertLess(handler.index(counter),handler.index(take))
            self.assertNotIn('data = decoded["data"]?',handler)
        self.assertNotIn('json.encode(',encode_body(response))
        self.assertIn('return ok(http.json(',encode_body(response))
        self.assertEqual(response.split('fn main():',1)[1],current.split('fn main():',1)[1])
        direct=expand_ku_server(source,19191,'direct','response')
        self.assertIn('while (text.len() < 8)',direct)
        self.assertIn('async fn work_response(',direct)
        self.assertEqual(encode_body(direct),encode_body(response))
        with self.assertRaises(RuntimeError):
            expand_ku_server(source.replace('    return ok(http.text(body))','    return ok(http.text("changed"))'),19191,json_mode='response')

    def test_typed_expansion_is_explicit_and_keeps_full_subtree_and_pg_work(self):
        source=Path(__file__).with_name('ku').joinpath('server.ku').read_text()
        baseline=expand_ku_server(source,19191)
        typed_json=expand_ku_server(source,19191,json_mode='typed')
        self.assertEqual(baseline,source.replace('__PEER_PORT__','19191'))
        self.assertNotIn('struct Envelope',baseline)
        self.assertIn('struct Envelope { id: str counter: int data: KuValue }',typed_json)
        handler=typed_json.split('fn encode(',1)[1].split('async fn work(',1)[0]
        for step in ('decoded = json.decode(req.body)?','counter = decoded["counter"]?.as_int()?','data = decoded.take("data")?','body = json.encode(envelope)?','return ok(http.text(body))'):
            self.assertEqual(handler.count(step),1)
        self.assertNotIn('http.json(',handler)
        self.assertNotIn('json.encode({',handler)
        self.assertEqual(typed_json.split('fn main():',1)[1],baseline.split('fn main():',1)[1])
        typed_pg=expand_ku_server(source,19191,pg_read='typed')
        self.assertIn('client.query_one_typed("SELECT id,label,score FROM cross_stack_items WHERE id=$1::int4"',typed_pg)
        self.assertIn('[req.query.key.clone()], ["text"], ["int4", "text", "int4"]',typed_pg)
        self.assertEqual(typed_pg.count('result.value_int('),2)
        self.assertIn('if (result.rows() != 1)',typed_pg)
        self.assertIn('return ok(http.text(status, body))',typed_pg)
        self.assertEqual(typed_pg.split('    async fn database(',1)[0],baseline.split('    async fn database(',1)[0])
        with self.assertRaises(ValueError):
            expand_ku_server(source,19191,pg_read='typed',pg_execution='sync')
        with self.assertRaises(RuntimeError):
            expand_ku_server(source.replace('WHERE id=$1','WHERE id=$2'),19191,pg_read='typed')
        combined=expand_ku_server(source,19191,json_mode='typed',pg_read='typed')
        self.assertIn('struct Envelope',combined)
        self.assertIn('query_one_typed',combined)

    def test_normal_and_tuned_flags_do_not_weaken_arithmetic(self):
        self.assertEqual(native_flags('ku','normal',None,Path('/profiles')),['-O2'])
        self.assertEqual(native_flags('cpp','normal',None,Path('/profiles')),['-O3','-DNDEBUG'])
        for stack in ('ku','cpp'):
            for variant in ('normal','o2','o3','thin','generate','use'):
                flags=native_flags(stack,variant,Path('/profile'),Path('/profiles'))
                self.assertFalse(any('fast-math' in x or 'wrapv' in x or 'march=native' in x for x in flags))
                if variant=='thin':
                    self.assertIn('-Wl,--thinlto-jobs=2',flags)


class HttpProfileTrainingTests(unittest.TestCase):
    """Synthetic transports and receipts only: no socket, service or compiler."""

    def test_reserved_http_recipes_have_different_business_inputs_and_stable_batch(self):
        train, held = (profile_training.http_recipe(mode) for mode in ('train', 'validate'))
        for key in ('payload', 'cpu_units', 'seed', 'db_key', 'rid_prefix', 'ascii_json'):
            self.assertNotEqual(train['input'][key], held['input'][key])
        self.assertEqual(list(train['input']['payload']), ['counter','data'])
        self.assertEqual(list(held['input']['payload']), ['data','counter'])
        self.assertNotEqual(identity(train['input']), identity(held['input']))
        self.assertEqual(train['seconds_per_workload'], 2)
        self.assertEqual(train['rates'], {'ok':2000,'json_object':500,'cpu':80,'db':200})
        self.assertEqual(sum(train['rates'].values())*2,5560)
        self.assertGreater(held['input']['db_key'],train['rates']['db']*2)
        self.assertIn('shared frozen',train['normal_driver_scope'])
        self.assertEqual(plan('train')['kind'], 'serial_batch_compiler_train')

    def test_explicit_error_contracts_rst_and_following_health_are_separate(self):
        from urllib.parse import parse_qs, urlsplit
        recipe = profile_training.http_recipe('train')
        class Response:
            def __init__(self, status, body): self.status,self.body = status,body
            def begin(self): pass
            def read(self, maximum): return self.body
        for stack in ('ku','rust'):
            expected_error=recipe['negative_expected_status']['invalid_json'][stack]
            class Connection:
                def __init__(self,*args,**kwargs): self.sock=self
                def connect(self): pass
                def sendall(self,value): pass
                def setsockopt(self,*args): pass
                def close(self): pass
                def request(self,method,path,body=None,headers=None): self.path,self.body=path,body
                def getresponse(self):
                    path=urlsplit(self.path); query=parse_qs(path.query); rid=query['rid'][0]
                    if self.body==b'{': return Response(expected_error,b'explicit handler error')
                    if path.path=='/ok': value='ok:'+rid
                    elif path.path=='/work':
                        value=rid+':'+format(expected({'iterations':len(query['work'][0])*1000,'seed':len(query['seed'][0])}),'08d')
                    elif path.path=='/db':
                        key=int(query['key'][0]); value={'id':key,'score':key*17,'label':f'item-{key}-中文🙂','rid':rid}
                    else:
                        source=json.loads(self.body);value={'id':rid,'counter':source['counter']+1,'data':source['data']}
                    return Response(200,json.dumps(value).encode() if isinstance(value,dict) else value.encode())
            rows=[]
            with patch.object(profile_training.http.client,'HTTPConnection',Connection), \
                 patch.object(profile_training.http.client,'HTTPResponse',return_value=Response(400,b'framing')), \
                 patch.object(profile_training.time,'sleep'):
                profile_training.http_probes('never-contacted:1',recipe,stack,rows)
            self.assertEqual(len(rows),8)
            self.assertEqual([r['actual_status'] for r in rows if r.get('expected_negative')],[expected_error,400])
            self.assertFalse(rows[-2]['correct'])
            self.assertTrue(rows[-2]['request_fully_sent'])
            self.assertTrue(rows[-1]['after_errors_and_cancel'])

    def test_http_normal_exit_precedes_merge_and_use_requires_http_reserved_inputs(self):
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory);binary=root/'server';binary.write_bytes(b'\x7fELF'+bytes(60))
            source=root/'source';source.write_text('frozen source')
            build={'stack':'ku','variant':'generate','passed':True,'source_files_unchanged':True,
                'source_fingerprint':'f'*64,'source_inputs':{str(source):compile_variants.digest(source)},
                'executable_artifacts':{str(binary):{'sha256':compile_variants.digest(binary)}},
                'compiler_version':'synthetic clang target=aarch64-linux','compiler_launcher_sha256':'c'*64}
            build_path=root/'build.json';build_path.write_text(json.dumps(build))
            manifest={'stacks':{'ku':{'command':[str(binary)],'compiler_variant':{
                'stack':'ku','variant':'generate','receipt':str(build_path),'source_fingerprint':'f'*64}}},
                'client':{'command':['not-executed-driver']}}
            manifest_path=root/'manifest.json';manifest_path.write_text(json.dumps(manifest));trace=[]
            class Harness:
                def __init__(self,*args):
                    self.manifest=copy.deepcopy(manifest);self.uid=os.getuid();self.gid=os.getgid()
                    self.protocol=Path(profile_training.__file__).with_name('protocol.json')
                    self.identity={};self.db_domain=types.SimpleNamespace(path=root/'owned-db');self.service=None
                def start(self,*args,**kwargs):
                    path=root/'private-service';path.mkdir()
                    def stop():
                        trace.append('normal-exit');path.rmdir()
                        return {'forced':False,'cgroup_removed':True,'exit_code':0}
                    self.service=types.SimpleNamespace(address='unused:1',startup={'command':[str(binary)]},
                        domain=types.SimpleNamespace(path=path),metrics=lambda:{},stop=stop)
                    return self.service
                def client(self,service,kind,rate,duration,output,**kwargs):
                    trace.append('driver-'+kind)
                    planned=rate*duration
                    return {'valid_driver':True,'ledger':{'planned_reconciled':True,'correct_total':planned,
                        'planned':planned,'results':{'correct':planned}},'configuration':{'workload':kind}}
                def close(self):
                    trace.append('close')
                    if self.service and self.service.domain.path.exists(): self.service.stop()
                    return []
            def merge(spec,env,tools,output,stack,result):
                self.assertIn('normal-exit',trace);trace.append('merge')
                profile=output/'merged.profdata';profile.write_bytes(b'synthetic parsed profile')
                result.update(profile_path=str(profile),profile_sha256=compile_variants.digest(profile),
                              profile_bytes=profile.stat().st_size,profile_complete=True)
            with patch.object(profile_training,'Harness',Harness),patch.object(profile_training,'http_probes'), \
                 patch.object(profile_training,'quiet_metrics',return_value=True), \
                 patch.object(profile_training,'merge_profile',side_effect=merge) as merger:
                training=profile_training.run_http(manifest_path,root/'train','ku','train',root/'tools')
                self.assertTrue(training['passed']);self.assertEqual(merger.call_count,1)
                self.assertEqual(training['protocol_sha256'],training['base_protocol_sha256'])
                self.assertIsNone(training['heldout_new_inputs'])
                self.assertNotEqual(json.loads((root/'train/corpora/object.json').read_text()),
                                    profile_training.http_recipe('validate')['input']['payload'])
                self.assertEqual(profile_input(root/'train/receipt.json','ku','f'*64)[0],(root/'train/merged.profdata').resolve())
                manifest['stacks']['ku']['compiler_variant']['variant']='use'
                manifest_path.write_text(json.dumps(manifest));build.update(variant='use',training_receipt=training)
                build_path.write_text(json.dumps(build))
                held=profile_training.run_http(manifest_path,root/'held','ku','validate',root/'tools')
                self.assertTrue(held['passed']);self.assertFalse(held['profile_complete'])
                self.assertEqual(held['heldout_new_inputs']['db_key'],997)
                self.assertEqual(merger.call_count,1)
                broken={'valid_driver':True,'ledger':{'planned_reconciled':True,'correct_total':19,
                    'planned':20,'results':{'correct':19,'server_error':1}},'configuration':{}}
                with patch.object(Harness,'client',return_value=broken), self.assertRaisesRegex(ValueError,'not entirely correct'):
                    profile_training.run_http(manifest_path,root/'failed','ku','validate',root/'tools')
                failed=json.loads((root/'failed/receipt.json').read_text())
                self.assertFalse(failed['passed']);self.assertFalse(failed['all_business_results_correct'])
                self.assertFalse((root/'private-service').exists());self.assertEqual(merger.call_count,1)
                build['training_receipt']['transport']='batch';build_path.write_text(json.dumps(build))
                with self.assertRaisesRegex(ValueError,'not batch training'):
                    profile_training.run_http(manifest_path,root/'wrong','ku','validate',root/'tools')


if __name__=='__main__':
    unittest.main()
