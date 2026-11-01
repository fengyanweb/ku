from pathlib import Path
import tempfile
import unittest

import http_cost_diagnostic as flow
import pg_cost_diagnostic as pg


ROOT = Path(__file__).resolve().parents[3]


class PgCostDiagnosticTests(unittest.TestCase):
    def source(self):
        # Real current runtime definitions, without invoking the compiler.
        return ("static int ku_pg_connection_is_utf8(PGconn* c) {\nreturn 1;\n}\n" +
                "\n".join((ROOT / "src/backend" / name).read_text() for name in
                            ("c_pg_async_runtime.c", "c_task_pg_runtime.c", "c_task_reactor.c")))

    def record(self):
        names = flow.FLOW_COUNTERS + pg.COUNTERS + flow.FLOW_COUNTER_NAMES[len(flow.FLOW_COUNTERS):]
        record = dict(schema="ku-http-pg-flow-v1", complete=True, overflow=False,
                      counter_names=list(names), threads=[dict(values=[0] * len(names))])
        for name, value in dict(pg_cost_operations=64, pg_cost_sampled_operations=1,
                                pg_cost_step_calls=192, pg_cost_step_sampled_calls=3).items():
            self.set_counter(record, name, value)
        return record

    def set_counter(self, record, name, value):
        record["threads"][0]["values"][record["counter_names"].index(name)] = value

    def test_phase_spans_use_actual_transition_and_sampling_keeps_calls_separate(self):
        source = self.source()
        output = pg.instrument(source)
        self.assertIn("ku_pg_cost_transition(op,(unsigned int)phase);", output)
        self.assertIn("op->ku_cost_sampled=(ku_pg_cost_op_sequence++&63u)==0u;", output)
        self.assertIn("if(!ku_pg_cost_sampling) {ku_pg_cost_phase=next;return;}", output)
        self.assertIn("if(ku_pg_cost_active)ku_flow_count(ku_pg_cost_base", output)
        self.assertLess(output.index("#define PQparameterStatus"),
                        output.index("static int ku_pg_connection_is_utf8"))
        # The actual query/reset FSM body stays byte-identical. Its failure,
        # deadline, drain-to-NULL and reset behavior cannot be bypassed by probes.
        signature = "static int ku_pg_async_step_inner("
        original = source[source.index(signature):source.index("\n}\n", source.index(signature)) + 3]
        self.assertIn(original, output)
        self.assertEqual(output.count("ku_pg_cost_original_ku_task_net_dispatch("), 2)

    def test_changed_or_duplicate_anchors_fail_closed(self):
        source = self.source()
        for changed in (source.replace("typedef struct KuPgAsyncOp {", "typedef struct KuPgAsyncOp{"),
                        source + "\nstatic int ku_pg_connection_is_utf8(PGconn* c) {\n}"):
            with self.subTest(changed=changed[-90:]), self.assertRaises(ValueError):
                pg.instrument(changed)

    def test_default_prefix_and_counter_names_do_not_change(self):
        self.assertEqual(flow._flow_c_prefix(), flow._flow_c_prefix(flow.FLOW_COUNTERS))
        self.assertEqual(len(pg.COUNTERS), len(set(pg.COUNTERS)))
        self.assertLessEqual(len(pg.COUNTERS), 256)
        # Array offsets in the C runtime must match enum order, including calls.
        for phase_index, phase in enumerate(pg.PHASES):
            self.assertEqual(pg.COUNTERS.index(f"pg_cost_{phase}_cpu_ns"),
                             pg.COUNTERS.index("pg_cost_unknown_cpu_ns") + phase_index * 13)

    def test_extension_rejects_invalid_names_collisions_and_excess(self):
        with tempfile.TemporaryDirectory() as directory:
            missing = Path(directory) / "missing.c"
            for counters in (("bad-name",), (flow.FLOW_COUNTERS[0],), ("repeated", "repeated"),
                             tuple(f"counter_{i}" for i in range(257))):
                with self.subTest(counters=counters[:2]), self.assertRaises(ValueError):
                    flow.prepare_ledger_source(missing, Path(directory) / "out.c", extra_counters=counters)

    def test_summary_distinguishes_elapsed_wait_cpu_and_overlapping_dispatch(self):
        record = self.record()
        for name, value in dict(pg_cost_step_cpu_ns=1200, pg_cost_query_read_cpu_ns=700,
                                pg_cost_reset_read_cpu_ns=400,
                                pg_cost_query_read_suspended_wall_ns=1000000,
                                pg_cost_dispatch_cpu_ns=1500,
                                pg_cost_dispatch_outside_step_cpu_ns=300).items():
            self.set_counter(record, name, value)
        result = pg.summarize(record)
        self.assertTrue(result["valid"])
        self.assertEqual(result["phases"]["query_read"]["suspended_wall_ns"], 1000000)
        self.assertEqual(result["phases"]["query_read"]["cpu_ns"], 700)
        self.assertEqual(result["spans"]["dispatch_outside_step"]["cpu_ns"], 300)

    def test_clock_errors_incomplete_banks_and_invalid_phase_closure_are_not_success(self):
        for flag in ("clock_failure", "nested_scope", "clock_regressed"):
            record = self.record()
            self.set_counter(record, "pg_cost_" + flag, 1)
            self.assertFalse(pg.summarize(record)["valid"])
        record = self.record()
        record["complete"] = False
        self.assertFalse(pg.summarize(record)["valid"])
        record = self.record()
        self.set_counter(record, "pg_cost_reset_read_cpu_ns", 1)
        self.assertFalse(pg.summarize(record)["valid"])
        record = self.record()
        record["threads"][0]["values"].pop()
        with self.assertRaises(ValueError):
            pg.summarize(record)
        record = self.record()
        self.set_counter(record, "pg_cost_sampled_operations", 0)
        self.assertFalse(pg.summarize(record)["valid"])


if __name__ == "__main__":
    unittest.main()
