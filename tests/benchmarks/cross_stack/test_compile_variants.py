"""Profile receipts must not turn failed/seen-input training into accepted PGO."""
import copy
import hashlib
import json
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch
import batch
import compile_variants
from compile_variants import profile_input, native_flags
from pg_variants import derive
from profile_training import PLANS, expected, identity, plan, parse_output
from build_linux import expand_ku_server


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
        prefix='    decoded = json.decode(req.body)?\n    counter = decoded["counter"]?.as_int()?\n    data = decoded["data"]?\n'
        self.assertIn(prefix,encode_body(response))
        self.assertNotIn('json.encode(',encode_body(response))
        self.assertIn('return ok(http.json(',encode_body(response))
        self.assertEqual(response.split('fn main():',1)[1],current.split('fn main():',1)[1])
        direct=expand_ku_server(source,19191,'direct','response')
        self.assertIn('while (text.len() < 8)',direct)
        self.assertIn('async fn work_response(',direct)
        self.assertEqual(encode_body(direct),encode_body(response))
        with self.assertRaises(RuntimeError):
            expand_ku_server(source.replace('    return ok(http.text(body))','    return ok(http.text("changed"))'),19191,json_mode='response')

    def test_normal_and_tuned_flags_do_not_weaken_arithmetic(self):
        self.assertEqual(native_flags('ku','normal',None,Path('/profiles')),['-O2'])
        self.assertEqual(native_flags('cpp','normal',None,Path('/profiles')),['-O3','-DNDEBUG'])
        for stack in ('ku','cpp'):
            for variant in ('normal','o2','o3','thin','generate','use'):
                flags=native_flags(stack,variant,Path('/profile'),Path('/profiles'))
                self.assertFalse(any('fast-math' in x or 'wrapv' in x or 'march=native' in x for x in flags))
                if variant=='thin':
                    self.assertIn('-Wl,--thinlto-jobs=2',flags)


if __name__=='__main__':
    unittest.main()
