#!/usr/bin/env python3
"""Prepare an opt-in Linux PG cost ledger from ordinary generated C.

  python3 pg_cost_diagnostic.py prepare normal.c pg-cost.c --source-commit SHA
  python3 pg_cost_diagnostic.py summarize flow-ledger.json pg-cost-summary.json

This tool never builds or launches a server. Link its output with the same C11
O2 flags and libraries plus --wrap=pthread_create. Keep the ordinary ELF for
scoring and observer controls. CPU spans include probe overhead; suspended
residence is elapsed wall time, not blocked time or database CPU.
"""
from __future__ import annotations

import argparse
import hashlib
import json
from pathlib import Path
import tempfile

import http_cost_diagnostic as flow

PHASES = ("unknown", "acquire", "connect", "prepare", "query_send", "query_flush",
          "query_read", "reset_send", "reset_flush", "reset_read", "done")
CALLS = ("PQsendQueryParams", "PQflush", "PQconsumeInput", "PQisBusy", "PQgetResult",
         "PQsetSingleRowMode", "PQtransactionStatus", "PQstatus", "PQparameterStatus")
COUNTERS = ("pg_cost_operations", "pg_cost_sampled_operations", "pg_cost_clock_failure", "pg_cost_nested_scope", "pg_cost_clock_regressed") + tuple(
    f"pg_cost_{phase}_{metric}" for phase in PHASES
    for metric in ("cpu_ns", "active_wall_ns", "suspended_wall_ns", "segments") + CALLS
) + tuple(f"pg_cost_{span}_{metric}" for span in
          ("step", "dispatch", "dispatch_outside_step", "interest", "submit", "take", "connect_worker")
          for metric in ("cpu_ns", "wall_ns", "calls", "sampled_calls"))
# C identifiers in the shared ledger use lowercase names.
COUNTERS = tuple(name.lower() for name in COUNTERS)


def _once(text: str, old: str, new: str) -> str:
    if text.count(old) != 1:
        raise ValueError(f"PG cost anchor is not unique: {old[:90]!r}")
    return text.replace(old, new, 1)


def _wrap(text: str, signature: str, name: str, arguments: str, span: str,
          result: str = "uint32_t") -> str:
    """Definitions end at column zero; fail closed on unsupported emissions."""
    if text.count(signature) != 1:
        raise ValueError(f"PG cost definition is not unique: {name}")
    start = text.index(signature)
    end = text.index("\n}\n", start) + 3
    original = text[start:end].replace(name + "(", "ku_pg_cost_original_" + name + "(", 1)
    body = (f"\n{signature}\n  static _Thread_local unsigned long long sequence;\n"
            "  int sample=(sequence++&63u)==0u;\n"
            "  unsigned long long cpu=sample?ku_pg_cost_clock(1):0,wall=sample?ku_pg_cost_clock(0):0;\n"
            "  unsigned long long steps=ku_pg_cost_step_calls,step_cpu=ku_pg_cost_step_cpu,step_wall=ku_pg_cost_step_wall;\n")
    if span == "dispatch":
        body += "  int previous_sampling=ku_pg_cost_dispatch_sampling;ku_pg_cost_dispatch_sampling=sample;\n"
    call = f"ku_pg_cost_original_{name}({arguments})"
    body += (f"  {result} result={call};\n" if result != "void" else f"  {call};\n")
    condition = "ku_pg_cost_step_calls!=steps" if span == "dispatch" else "1"
    body += (f"  if({condition}) {{\n"
             f"    ku_flow_count(KUF_PG_COST_{span.upper()}_CALLS);\n")
    if span == "dispatch":
        body += "    ku_flow_count(KUF_PG_COST_DISPATCH_OUTSIDE_STEP_CALLS);\n"
    body += (
             "    if(sample) {\n"
             "      unsigned long long elapsed=ku_pg_cost_elapsed(cpu,ku_pg_cost_clock(1));\n"
             "      unsigned long long elapsed_wall=ku_pg_cost_elapsed(wall,ku_pg_cost_clock(0));\n"
             f"      ku_flow_add(KUF_PG_COST_{span.upper()}_CPU_NS,elapsed);\n"
             f"      ku_flow_add(KUF_PG_COST_{span.upper()}_WALL_NS,elapsed_wall);\n"
             f"      ku_flow_count(KUF_PG_COST_{span.upper()}_SAMPLED_CALLS);\n")
    if span == "dispatch":
        body += ("      ku_flow_add(KUF_PG_COST_DISPATCH_OUTSIDE_STEP_CPU_NS,\n"
                 "          ku_pg_cost_difference(elapsed,ku_pg_cost_step_cpu-step_cpu));\n"
                 "      ku_flow_add(KUF_PG_COST_DISPATCH_OUTSIDE_STEP_WALL_NS,\n"
                 "          ku_pg_cost_difference(elapsed_wall,ku_pg_cost_step_wall-step_wall));\n"
                 "      ku_flow_count(KUF_PG_COST_DISPATCH_OUTSIDE_STEP_SAMPLED_CALLS);\n")
    body += "    }\n  }\n"
    if span == "dispatch":
        body += "  ku_pg_cost_dispatch_sampling=previous_sampling;\n"
    body += ("  return result;\n" if result != "void" else "") + "}\n"
    return text[:start] + original + body + text[end:]


def instrument(text: str) -> str:
    # Definitions/prototypes are before all use sites. Macro wrappers below
    # count libpq calls without evaluating any argument twice.
    runtime = Path(__file__).with_name("pg_cost_runtime.c.inc").read_text()
    text = _once(text, "static int ku_pg_connection_is_utf8(PGconn* c) {",
                 runtime + "\nstatic int ku_pg_connection_is_utf8(PGconn* c) {")
    text = _once(text, "typedef struct KuPgAsyncOp {", "typedef struct KuPgAsyncOp {\n"
                 "  unsigned long long ku_cost_yield_wall;\n  unsigned int ku_cost_yield_phase,ku_cost_sampled;")
    text = _once(text, "static void ku_pg_async_set_phase(KuPgAsyncOp* op,int phase) {\n",
                 "static void ku_pg_async_set_phase(KuPgAsyncOp* op,int phase) {\n"
                 "  ku_pg_cost_transition(op,(unsigned int)phase);\n")
    text = _once(text, "  p->async_operations++;ku_pg_mutex_unlock(&p->lock);return op;",
                 "  p->async_operations++;ku_pg_mutex_unlock(&p->lock);\n"
                 "  ku_flow_count(KUF_PG_COST_OPERATIONS);\n"
                 "  op->ku_cost_sampled=(ku_pg_cost_op_sequence++&63u)==0u;\n"
                 "  if(op->ku_cost_sampled)ku_flow_count(KUF_PG_COST_SAMPLED_OPERATIONS);\n"
                 "  op->ku_cost_yield_wall=op->ku_cost_sampled?ku_pg_cost_clock(0):0;op->ku_cost_yield_phase=1u;return op;")
    signature = "static int ku_pg_async_step(KuPgAsyncOp* op,unsigned int events,unsigned long long now,int cancelled) {"
    start = text.index(signature)
    end = text.index("\n}\n", start) + 3
    original = text[start:end].replace("ku_pg_async_step(", "ku_pg_cost_original_step(", 1)
    wrapper = signature + r'''
  int sample=op->ku_cost_sampled || ku_pg_cost_dispatch_sampling;
  unsigned long long cpu=sample?ku_pg_cost_clock(1):0,wall=sample?ku_pg_cost_clock(0):0;
  if(ku_pg_cost_active)ku_flow_count(KUF_PG_COST_NESTED_SCOPE);
  if(op->ku_cost_yield_wall)ku_pg_cost_suspend(op->ku_cost_yield_phase,
      ku_pg_cost_elapsed(op->ku_cost_yield_wall,wall));
  ku_pg_cost_sampling=sample;ku_pg_cost_active=op;ku_pg_cost_phase=(unsigned int)op->phase;
  ku_pg_cost_cpu=cpu;ku_pg_cost_wall=wall;
  int done=ku_pg_cost_original_step(op,events,now,cancelled);
  ku_pg_cost_transition(op,(unsigned int)op->phase);
  if(sample) {
    unsigned long long elapsed=ku_pg_cost_elapsed(cpu,ku_pg_cost_clock(1));
    unsigned long long elapsed_wall=ku_pg_cost_elapsed(wall,ku_pg_cost_clock(0));
    ku_flow_add(KUF_PG_COST_STEP_CPU_NS,elapsed);ku_flow_add(KUF_PG_COST_STEP_WALL_NS,elapsed_wall);
    ku_flow_count(KUF_PG_COST_STEP_SAMPLED_CALLS);
    ku_pg_cost_step_cpu+=elapsed;ku_pg_cost_step_wall+=elapsed_wall;
  }
  ku_flow_count(KUF_PG_COST_STEP_CALLS);ku_pg_cost_step_calls++;
  ku_pg_cost_active=NULL;
  op->ku_cost_yield_wall=(!done && op->ku_cost_sampled)?ku_pg_cost_clock(0):0;
  op->ku_cost_yield_phase=(unsigned int)op->phase;
  return done;
}
'''
    text = text[:start] + original + "\n" + wrapper + text[end:]
    wrappers = (
        ("static uint32_t ku_task_net_dispatch(KuTaskBlockingPoolV1* pool,size_t owner,size_t index,uint64_t generation,uint32_t events) {",
         "ku_task_net_dispatch", "pool,owner,index,generation,events", "dispatch", "uint32_t"),
        ("static int ku_pg_async_interest(KuPgAsyncOp* op,int* fd,unsigned int* events,unsigned long long* deadline) {",
         "ku_pg_async_interest", "op,fd,events,deadline", "interest", "int"),
        ("static uint32_t ku_task_blocking_take_pg(const KuTaskDriverTicketV1* ticket,\n    KuTaskBlockingTokenV1* token,KuResult_pg_result* output) {",
         "ku_task_blocking_take_pg", "ticket,token,output", "take", "uint32_t"),
        ("static void ku_task_pg_connect_run(KuTaskBlockingJobV1* job,int cancelled) {",
         "ku_task_pg_connect_run", "job,cancelled", "connect_worker", "void"),
        ("static uint32_t ku_task_blocking_submit_pg_inputs(const KuTaskDriverTicketV1* ticket,\n"
         "    KuTaskControlV1* control,KuTaskBlockingTokenV1* token,\n"
         "    KuPgClient* client,KuString sql,KuArray_str params,const KuValue* typed_params,size_t typed_count,\n"
         "    KuArray_str param_types,KuArray_str result_types,int typed,uint64_t deadline,\n"
         "    void* context,uint32_t (*reconcile)(void*),KuResult_pg_result* output) {",
         "ku_task_blocking_submit_pg_inputs", "ticket,control,token,client,sql,params,typed_params,typed_count,param_types,result_types,typed,deadline,context,reconcile,output", "submit", "uint32_t"),
    )
    for args in wrappers:
        text = _wrap(text, *args)
    return text


def summarize(record: dict) -> dict:
    """Consume one joined flow record. Never normalize sampled clocks as CPU share."""
    if record.get("schema") != "ku-http-pg-flow-v1":
        raise ValueError("expected one joined flow ledger record")
    names = record.get("counter_names", [])
    if len(set(names)) != len(names) or any(name not in names for name in COUNTERS):
        raise ValueError("PG cost counter identity is missing or duplicated")
    totals = dict.fromkeys(names, 0)
    threads = record.get("threads", [])
    if len(threads) > 256 or len(names) > 1024:
        raise ValueError("bounded flow record limits exceeded")
    for thread in threads:
        values = thread.get("values", [])
        if len(values) != len(names) or any(type(value) is not int or value < 0 for value in values):
            raise ValueError("flow counter bank is malformed")
        for name, value in zip(names, values):
            totals[name] += value
    flags = [name for name in ("clock_failure", "nested_scope", "clock_regressed")
             if totals["pg_cost_" + name]]
    valid = bool(record.get("complete")) and not record.get("overflow") and not flags
    phases = {phase: {metric: totals[f"pg_cost_{phase}_{metric}"]
              for metric in ("cpu_ns", "active_wall_ns", "suspended_wall_ns", "segments")}
              for phase in PHASES}
    calls = {phase: {call: totals[f"pg_cost_{phase}_{call.lower()}"] for call in CALLS}
             for phase in PHASES}
    spans = {span: {metric: totals[f"pg_cost_{span}_{metric}"]
             for metric in ("cpu_ns", "wall_ns", "calls", "sampled_calls")}
             for span in ("step", "dispatch", "dispatch_outside_step", "interest", "submit", "take", "connect_worker")}
    if not (0 < totals["pg_cost_sampled_operations"] <= totals["pg_cost_operations"]
            and 0 < spans["step"]["sampled_calls"] <= spans["step"]["calls"]):
        valid = False
        flags.append("missing_or_inconsistent_samples")
    if any(span["sampled_calls"] > span["calls"] for span in spans.values()):
        valid = False
        flags.append("sample_count_exceeds_calls")
    if sum(item["cpu_ns"] for item in phases.values()) > spans["step"]["cpu_ns"]:
        valid = False
        flags.append("phase_cpu_exceeds_step")
    return dict(schema="ku-pg-cost-summary-v1", diagnostic_only=True, valid=valid,
        invalid_flags=flags, sampling_stride=64, phases=phases, full_libpq_call_counts=calls,
        spans=spans, operations=totals["pg_cost_operations"],
        sampled_operations=totals["pg_cost_sampled_operations"],
        interpretation="Phase clocks are sampled gross CPU including probe cost; suspension is elapsed residence including scheduling. Dispatch includes step. No DB CPU or unbiased population CPU share is inferred.")


def prepare(source: Path, destination: Path, source_commit: str) -> dict:
    source, destination = Path(source), Path(destination)
    identity = destination.with_suffix(destination.suffix + ".identity.json")
    if destination.exists() or identity.exists() or source.resolve() == destination.resolve():
        raise FileExistsError("choose a fresh diagnostic destination")
    with tempfile.TemporaryDirectory(prefix="ku-pg-cost-") as temporary:
        ledger = Path(temporary) / "ledger.c"
        receipt = flow.prepare_ledger_source(source, ledger, source_commit,
                                             extra_counters=COUNTERS)
        output = instrument(ledger.read_text())
        staged = Path(temporary) / "cost.c"
        staged.write_text(output)
        roles = Path(temporary) / "roles.c"
        flow.prepare_source(staged, roles, source_commit)
        final = roles.read_bytes()
    receipt["base_ledger_c_sha256"] = receipt.pop("ledger_c_sha256")
    receipt.update(schema="ku-pg-cost-emission-v1", counter_names=receipt["counter_names"],
        diagnostic_c_sha256=hashlib.sha256(final).hexdigest(),
        required_next_step="compile with original C11 O2 flags/runtime libraries and --wrap=pthread_create",
        phases=PHASES, libpq_calls=CALLS, sampling_stride=64,
        observer_control="ordinary 200/s short window -> sampled probe -> ordinary; same duration and original controls, no capacity run",
        limitations=["Base flow counters count calls; only the additional sampled PG cost counters measure gross clock spans."] + receipt["limitations"][1:] + [
            "Linux only. CLOCK_THREAD_CPUTIME_ID is active thread CPU; CLOCK_MONOTONIC is elapsed wall time.",
            "Phase CPU is split at actual phase transitions within each step, not attributed by entry phase.",
            "Suspended residence includes socket wait, owner scheduling and bridge work; never label it blocked I/O or DB CPU.",
            "Dispatch includes step; dispatch_outside_step subtracts nested step CPU and must not be added to dispatch.",
            "Dispatch covers validated calls that enter PG step; stale/no-step dispatch and reactor snapshot overhead are outside it.",
            "Counters cover process lifetime, including warmup. Query and reset each retain original send/drain/check behavior.",
            "Fixed 1/64 operation sampling per creating thread; bridge spans sample independently at stride 64 and force nested step timing for subtraction. Systematic samples are not an unbiased CPU-share estimator.",
            "Extra clocks/counters change timing and allocation size. Compare independent ordinary controls; no performance adoption from this ELF.",
            "Separate Rust default and strict reset runs externally; Ku always retains reset. No SQL/parameter payload is logged.",
            "source_commit is caller supplied; the original generated C SHA256 is the authoritative input identity.",
        ])
    destination.parent.mkdir(parents=True, exist_ok=True)
    destination.write_bytes(final)
    flow.write_json(identity, receipt)
    return receipt


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)
    emission = commands.add_parser("prepare")
    emission.add_argument("source", type=Path)
    emission.add_argument("destination", type=Path)
    emission.add_argument("--source-commit", required=True)
    summary = commands.add_parser("summarize")
    summary.add_argument("source", type=Path)
    summary.add_argument("destination", type=Path)
    args = parser.parse_args()
    if args.command == "summarize":
        if args.destination.exists():
            raise FileExistsError("preserve prior summary; choose a fresh destination")
        with args.source.open("rb") as stream:
            raw = stream.read(4 * 1024 * 1024 + 1)
        if len(raw) > 4 * 1024 * 1024:
            raise ValueError("flow ledger exceeds 4MiB bound")
        record = json.loads(raw)
        if "records" in record:
            if len(record["records"]) != 1:
                raise ValueError("exactly one lifecycle record is required")
            record = record["records"][0]
        result = summarize(record)
        flow.write_json(args.destination, result)
        print(json.dumps({"schema": result["schema"], "valid": result["valid"]}))
        return
    receipt = prepare(args.source, args.destination, args.source_commit)
    print(json.dumps({key: receipt[key] for key in ("schema", "diagnostic_only", "diagnostic_c_sha256")}))


if __name__ == "__main__":
    main()
