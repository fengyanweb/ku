"""Profile receipts must not turn failed/seen-input training into accepted PGO."""
import copy
import hashlib
import json
from pathlib import Path
import tempfile
import unittest
from compile_variants import profile_input, native_flags
from profile_training import PLANS, expected, identity, plan, parse_output
from build_linux import expand_ku_server


class ProfileReceiptTests(unittest.TestCase):
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
