//! JSON owners held by real source-lowered Task frames, never a private JSON ABI.
#[path = "support/native_allocation_harness.rs"]
mod native_allocation_harness;
#[allow(dead_code)]
#[path = "support/native_pg_harness.rs"]
mod native_harness;
#[path = "support/native_task_ledger.rs"]
mod native_task_ledger;

use ku::{backend::c, checker::Checker, ir::task_lower, lexer::Lexer, parser::Parser};
use native_allocation_harness::ALLOCATION_HOOK;
use native_harness::{
    compile_harness, run_bounded, TempDir, NATIVE_THREAD_LIFECYCLE_HARNESS, RUN_LIMITS, RUN_TIMEOUT,
};
use native_task_ledger::{LEDGER_LOCK, LOCKED_ALLOCATIONS};
use std::{fs, process::Command};

fn replace_once(source: String, needle: &str, replacement: &str) -> String {
    // Every entry point may embed a checkout-CRLF C file. Normalize here, not
    // only in generated(), because raw-IR probes also emit C directly.
    let source = source.replace("\r\n", "\n");
    assert_eq!(
        source.matches(needle).count(),
        1,
        "JSON test anchor: {needle}"
    );
    source.replacen(needle, replacement, 1)
}

fn lowered(source: &str) -> task_lower::NativeTaskProgram {
    let mut ast = Parser::new(Lexer::new(source).lex().expect("JSON source lexes"))
        .parse_program()
        .expect("JSON source parses");
    for name in ["std:json", "std:time"] {
        ast.items.insert(
            0,
            ku::ast::Item::Module(ku::ast::ModuleDecl {
                name: name.into(),
                span: Default::default(),
            }),
        );
    }
    Checker::new()
        .check(&ast)
        .unwrap_or_else(|error| panic!("{error}\n{source}"));
    task_lower::lower_program(&ast).unwrap_or_else(|error| panic!("{error}\n{source}"))
}

fn generated(source: &str) -> String {
    // Embedded C may use checkout CRLF; diagnostic anchors use LF.
    c::generate_native_task_c_source(&lowered(source), &Default::default())
        .unwrap()
        .replace("\r\n", "\n")
}

fn with_ledger(source: String) -> String {
    let instrument=format!("{NATIVE_THREAD_LIFECYCLE_HARNESS}\n{LEDGER_LOCK}\n{ALLOCATION_HOOK}\n{LOCKED_ALLOCATIONS}\n");
    let source = replace_once(
        source,
        "typedef struct KuString {",
        &format!("{instrument}typedef struct KuString {{"),
    );
    let mut source = replace_once(
        source,
        "int main(void) {",
        "static int fixture_source_main(void) {",
    );
    source.push_str(
        r#"
int main(void) {
  CHECK(fixture_source_main()==0);
  FixtureLedger ledger=fixture_ledger();
  CHECK(!ledger.allocations && !ledger.bytes && !ledger.overflow);
  CHECK(!ku_task_root_driver.resident && !ku_task_root_driver.reserved_bytes);
  return 0;
}
"#,
    );
    source
}

fn execute(name: &str, source: String) -> String {
    let directory = TempDir::new(name);
    let path = directory.path().join("program.c");
    fs::write(&path, source).unwrap();
    let Some(executable) = compile_harness(directory.path(), &path, "program") else {
        assert!(
            std::env::var_os("GITHUB_ACTIONS").is_none(),
            "CI must execute Task JSON native C"
        );
        return String::new();
    };
    let mut text = String::new();
    for _ in 0..4 {
        let output = run_bounded(
            Command::new(&executable).current_dir(directory.path()),
            RUN_TIMEOUT,
            RUN_LIMITS,
        )
        .unwrap();
        assert!(
            output.status.success(),
            "{name}: {}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        text.push_str(&String::from_utf8(output.stdout).unwrap().replace('\r', ""));
    }
    text
}

#[test]
fn native_task_json_locals_survive_await_and_preserve_get_take_ownership() {
    let source = include_str!("../examples/task_json.ku");
    let generated = generated(source);
    assert!(generated.contains("ku_task_host_json_decode"));
    assert!(generated.contains("ku_task_host_json_take"));
    assert!(generated.contains("ku_task_host_json_build_object"));
    let text = execute("task-json-await", with_ledger(generated));
    if text.is_empty() {
        return;
    }
    assert_eq!(
        text.lines().collect::<Vec<_>>(),
        [r#"{"copy":{"items":[1,true,null],"message":"中文😀"},"counter":8,"data":{"items":[1,true,null],"message":"中文😀"},"id":"rid"}"#;
            4]
    );
}

#[test]
fn native_task_json_errors_and_consuming_accessors_release_every_owner() {
    let source = r#"
import json from "std.json"
async fn main():null! {
    try { bad=json.decode("{\"allocated\":[\"prefix\",1],\"bad\":}")? } catch(error) {println(error.code)}
    root=json.decode("{\"text\":\"中文\",\"bad\":[1,2,3]}")?
    text_value=root["text"]?
    text=text_value.as_str()?
    println(text)
    try { wrong=root.take("bad")? number=wrong.as_int()? } catch(error) {println(error.code)}
    return ok(null)
}
"#;
    let text = execute("task-json-errors", with_ledger(generated(source)));
    if text.is_empty() {
        return;
    }
    let lines = text.lines().collect::<Vec<_>>();
    assert_eq!(lines.len(), 12);
    for chunk in lines.chunks(3) {
        assert_eq!(chunk[0], "parse_error");
        assert_eq!(chunk[1], "中文");
        assert_eq!(chunk[2], "type_mismatch");
    }
}

#[test]
fn native_task_json_dynamic_values_do_not_extend_public_task_outcomes() {
    let source = r#"
import json from "std.json"
import time from "std.time"
async fn main():null! {
    value=json.decode("null")?
    body=json.encode(value)?
    println(body)
    owned=json.decode("{\"text\":\"owned\"}")?
    first=json.encode(owned)?
    time.sleep(1)?
    second=json.encode(owned)?
    println(first)
    println(second)
    return ok(null)
}
"#;
    let generated = generated(source);
    let start = generated
        .find("typedef struct KuTaskAdapterOutcomeV1")
        .unwrap();
    let end = start
        + generated[start..]
            .find("} KuTaskAdapterOutcomeV1;")
            .unwrap();
    assert!(!generated[start..end].contains("KuResult_kuvalue"));
    assert!(!generated.contains("run_source"));
    let text = execute("task-json-null", with_ledger(generated));
    if !text.is_empty() {
        assert_eq!(
            text,
            "null\n{\"text\":\"owned\"}\n{\"text\":\"owned\"}\n".repeat(4)
        );
    }
}

#[test]
fn native_task_unused_json_slots_still_emit_their_storage_abi() {
    use ku::ir::{
        task::{self, TaskSlot, TaskSlotType},
        IrType,
    };

    // Valid raw IR may retain an uninitialized owner slot after dead-code
    // removal. Its layout still needs the type ABI even without a JSON op.
    let ast = Parser::new(
        Lexer::new("async fn main():null! { println(\"unused-json-slot\") return ok(null) }")
            .lex()
            .unwrap(),
    )
    .parse_program()
    .unwrap();
    Checker::new().check(&ast).unwrap();
    let mut native = task_lower::lower_program(&ast).unwrap();
    for ty in [
        IrType::Named("__ku_value".into()),
        IrType::Result(Box::new(IrType::Named("__ku_value".into()))),
    ] {
        native.tasks.functions[0].slots.push(TaskSlot {
            ty: TaskSlotType::Value {
                ty,
                borrowed: false,
            },
        });
    }
    task::verify_and_plan(&native.tasks, Default::default()).unwrap();
    let generated = c::generate_native_task_c_source(&native, &Default::default()).unwrap();
    let text = execute("task-json-unused-slots", with_ledger(generated));
    if !text.is_empty() {
        assert_eq!(text, "unused-json-slot\n".repeat(4));
    }
}

#[test]
fn native_task_json_wait_cancel_and_timeout_retire_trees_and_budget() {
    let source = r#"
import json from "std.json"
import time from "std.time"
async fn held():int! {
    value=json.decode("{\"text\":\"中文😀\",\"tree\":[1,true,null,{\"x\":\"owned\"}]}")?
    time.sleep(60000)?
    body=json.encode(value)?
    println("unexpected completion")
    return ok(body.byte_len())
}
async fn main():null! { return ok(null) }
"#;
    let generated = generated(source);
    let instrumentation = format!("{NATIVE_THREAD_LIFECYCLE_HARNESS}\n{LEDGER_LOCK}\n{ALLOCATION_HOOK}\n{LOCKED_ALLOCATIONS}\n");
    let generated = replace_once(
        generated,
        "typedef struct KuString {",
        &format!("{instrumentation}typedef struct KuString {{"),
    );
    let generated = replace_once(generated,"static uint64_t ku_task_driver_now_ms(void) {","static uint64_t ku_task_driver_now_ms(void) { return 1000u; }\nstatic uint64_t fixture_real_clock(void) {");
    let mut generated = replace_once(
        generated,
        "int main(void) {",
        "static int fixture_unused_source_main(void) {",
    );
    generated.push_str(r#"
static void fixture_wake(void* raw,size_t owner) { (void)raw;(void)owner; }
static void fixture_case(uint32_t reason) {
  KuTaskDriverV1 driver={0};KuTaskDriverSlotV1 slots[2]={0};size_t ring[2]={0};
  size_t fixed=ku_task_driver_fixed_storage_bytes(2u);
  uint64_t next=UINT64_MAX,deadline=3000u;KuTaskValueV1 task={0};
  CHECK(ku_task_driver_init_external(&driver,sizeof(driver),KU_TASK_DRIVER_ABI_VERSION,
      slots,2u,ring,2u,fixed+sizeof(KuTaskInstance_0)+(1u<<20),1u,deadline,NULL,fixture_wake)==KU_TASK_DRIVER_OK);
  CHECK(ku_task_0_start_value(&driver,&task)==KU_TASK_DRIVER_OK);
  for(size_t turn=0;turn<128u && !driver.timers;turn++)
    CHECK(ku_task_driver_pump(&driver,0u,1u,&next)<=KU_TASK_DRIVER_PENDING);
  CHECK(driver.resident==1u && driver.timers==1u);
  CHECK(driver.reserved_bytes>sizeof(KuTaskInstance_0));
  CHECK(ku_task_control_status(&task.owner.lease)==KU_TASK_CONTROL_PENDING);
  FixtureLedger held=fixture_ledger();CHECK(held.bytes>sizeof(KuTaskInstance_0));
  /* The real HTTP/Task owner supplies the cancellation reason and original
   * absolute cleanup deadline through this same driver API. No frame status,
   * JSON owner, timer state or cleanup implementation is replaced here. */
  CHECK(ku_task_driver_request_cancel(&task.ticket,&task.owner.lease,reason,deadline)==KU_TASK_CONTROL_OK);
  CHECK(ku_task_control_cleanup_deadline(task.owner.lease.control)==deadline);
  for(size_t turn=0;turn<512u && ku_task_control_status(&task.owner.lease)==KU_TASK_CONTROL_PENDING;turn++)
    CHECK(ku_task_driver_pump(&driver,0u,1u,&next)<=KU_TASK_DRIVER_PENDING);
  CHECK(ku_task_control_status(&task.owner.lease)==reason);
  CHECK(ku_task_control_cleanup_deadline(task.owner.lease.control)==deadline);
  CHECK(ku_task_value_drop(&task,deadline)==KU_TASK_DRIVER_OK);
  CHECK(ku_task_driver_request_close(&driver,deadline)==KU_TASK_DRIVER_OK);
  for(size_t turn=0;turn<512u && driver.workers_exited!=1u;turn++)
    CHECK(ku_task_driver_pump(&driver,0u,32u,&next)<=KU_TASK_DRIVER_PENDING);
  CHECK(!driver.resident && !driver.running && !driver.queued && !driver.timers && !driver.reserved_bytes && !driver.fault);
  CHECK(ku_task_driver_join(&driver,deadline)==KU_TASK_DRIVER_OK);
  CHECK(ku_task_driver_destroy(&driver)==KU_TASK_DRIVER_OK);
  FixtureLedger empty=fixture_ledger();CHECK(!empty.allocations && !empty.bytes && !empty.overflow);
}
int main(void) {
  for(size_t round=0;round<4u;round++) {
    fixture_case(KU_TASK_CONTROL_CANCELLED);
    fixture_case(KU_TASK_CONTROL_TIMED_OUT);
  }
  puts("task-json-cancel-retirement-ok");return 0;
}
"#);
    let text = execute("task-json-cancel-retirement", generated);
    if !text.is_empty() {
        assert_eq!(text, "task-json-cancel-retirement-ok\n".repeat(4));
    }
}

#[test]
fn native_task_json_cancel_during_prepaid_decode_keeps_charge_until_real_retirement() {
    use ku::ir::task::TaskOp;

    let source = r#"
import json from "std.json"
import time from "std.time"
async fn held(text:str):null! {
    value=json.decode(text)?
    time.sleep(60000)?
    body=json.encode(value)?
    println(body)
    return ok(null)
}
async fn main():null! { return ok(null) }
"#;
    let native = lowered(source);
    assert_eq!(native.tasks.functions[0].name, "held");
    let result_slot = native.tasks.functions[0]
        .states
        .iter()
        .flat_map(|state| &state.operations)
        .find_map(|operation| match operation {
            TaskOp::JsonDecode { dst, .. } => Some(dst.0),
            _ => None,
        })
        .expect("real decode destination");
    // Exercise the checkout-dependent embedded-C newline shape on every host,
    // including Unix. Probe placement must survive CRLF without changing C.
    let generated = c::generate_native_task_c_source(&native, &Default::default())
        .unwrap()
        .replace("\r\n", "\n")
        .replace('\n', "\r\n");
    let allocations=LOCKED_ALLOCATIONS
        .replace("static void* fixture_malloc(size_t size) { fixture_alloc_lock();", "static void* fixture_malloc(size_t size) { fixture_json_allocation_boundary(); fixture_alloc_lock();")
        .replace("static void* fixture_calloc(size_t count,size_t size) { fixture_alloc_lock();", "static void* fixture_calloc(size_t count,size_t size) { fixture_json_allocation_boundary(); fixture_alloc_lock();")
        .replace("static void* fixture_realloc(void* old,size_t size) { fixture_alloc_lock();", "static void* fixture_realloc(void* old,size_t size) { if(size)fixture_json_allocation_boundary(); fixture_alloc_lock();");
    assert_eq!(
        allocations
            .matches("fixture_json_allocation_boundary();")
            .count(),
        3
    );
    let instrumentation=format!("{NATIVE_THREAD_LIFECYCLE_HARNESS}\n{LEDGER_LOCK}\n{ALLOCATION_HOOK}\nstatic void fixture_json_allocation_boundary(void);\nstatic void fixture_decode_prepaid(void*);\nstatic void fixture_decode_returned(void*,const void*);\n{allocations}\n");
    let generated = replace_once(
        generated,
        "typedef struct KuString {",
        &format!("{instrumentation}typedef struct KuString {{"),
    );
    // Observe the old parser on both sides of the real allocating call. The
    // reservation, parser, Result and frame publication remain unchanged.
    let generated=replace_once(generated,
        "if(ku_task_json_parse_reservation(text,&bytes) || ku_task_json_reserve(host,bytes))return ku_task_json_limit();\n  return ku_json_parse(text);",
        "if(ku_task_json_parse_reservation(text,&bytes) || ku_task_json_reserve(host,bytes))return ku_task_json_limit();\n  fixture_decode_prepaid(host);\n  KuResult_kuvalue parsed=ku_json_parse(text);\n  fixture_decode_returned(host,&parsed);\n  return parsed;");
    let generated=replace_once(generated,"static uint64_t ku_task_driver_now_ms(void) {","static uint64_t ku_task_driver_now_ms(void) { return 1000u; }\nstatic uint64_t fixture_real_clock(void) {");
    let mut generated = replace_once(
        generated,
        "int main(void) {",
        "static int fixture_unused_source_main(void) {",
    );
    let checks=r#"
static KuTaskValueV1* fixture_owner;
static KuTaskAdapterHostV1* fixture_decode_host;
static size_t fixture_prepaid_charge,fixture_prepaid_total;
static unsigned fixture_allocations_seen,fixture_cancel_calls,fixture_returns;
static uint32_t fixture_reason;
static int fixture_decode_in_flight,fixture_valid;
static void fixture_wake(void* raw,size_t owner) { (void)raw;(void)owner; }
static KuTaskDriverSlotV1* fixture_running_decode(void) {
  KuTaskAdapterHostV1* host=fixture_decode_host;CHECK(host && fixture_owner);
  CHECK(host->ticket->driver==fixture_owner->ticket.driver && host->ticket->slot==fixture_owner->ticket.slot
      && host->ticket->generation==fixture_owner->ticket.generation);
  KuTaskDriverV1* driver=host->ticket->driver;
  KuTaskDriverSlotV1* slot=ku_task_driver_find(host->ticket);
  CHECK(slot && slot->state==KU_TASK_DRIVER_RUNNING && driver->running==1u && driver->resident==1u);
  KuTaskInstance_0* instance=(KuTaskInstance_0*)host->allocation_context;
  CHECK(instance->frame_initialized && !instance->frame.header.result_initialized);
  CHECK(!(instance->frame.header.initialized & (UINT64_C(1)<<@RESULT_SLOT@)));
  CHECK(!instance->frame.s_@RESULT_SLOT@.ok && instance->frame.s_@RESULT_SLOT@.value.tag==KU_NULL);
  return slot;
}
static void fixture_decode_prepaid(void* raw) {
  CHECK(!fixture_decode_in_flight && !fixture_cancel_calls && !fixture_returns);
  fixture_decode_host=(KuTaskAdapterHostV1*)raw;
  KuTaskDriverSlotV1* slot=fixture_running_decode();
  CHECK(ku_task_control_status(&fixture_owner->owner.lease)==KU_TASK_CONTROL_PENDING);
  CHECK(ku_task_control_atomic_load(&fixture_decode_host->control->phase)==KU_TASK_CONTROL_LIVE);
  fixture_prepaid_charge=slot->charged_bytes;
  fixture_prepaid_total=fixture_decode_host->ticket->driver->reserved_bytes;
  CHECK(fixture_prepaid_charge>sizeof(KuTaskInstance_0) && fixture_prepaid_total==fixture_prepaid_charge);
  fixture_decode_in_flight=1;
}
static void fixture_json_allocation_boundary(void) {
  if(!fixture_decode_in_flight)return;
  KuTaskDriverSlotV1* slot=fixture_running_decode();
  CHECK(slot->charged_bytes==fixture_prepaid_charge
      && fixture_decode_host->ticket->driver->reserved_bytes==fixture_prepaid_total);
  fixture_allocations_seen++;
  if(!fixture_cancel_calls) {
    /* This callback executes before the first actual JSON allocation and
     * outside both the driver and allocation-ledger locks. It publishes the
     * same owner cancellation as a concurrent event-loop thread would. */
    CHECK(ku_task_driver_request_cancel(&fixture_owner->ticket,&fixture_owner->owner.lease,
        fixture_reason,3000u)==KU_TASK_CONTROL_OK);
    fixture_cancel_calls++;
  }
  CHECK(ku_task_control_status(&fixture_owner->owner.lease)==KU_TASK_CONTROL_PENDING);
  CHECK(ku_task_control_cleanup_deadline(fixture_owner->owner.lease.control)==3000u);
  CHECK(slot->charged_bytes==fixture_prepaid_charge
      && fixture_decode_host->ticket->driver->reserved_bytes==fixture_prepaid_total);
}
static void fixture_decode_returned(void* raw,const void* payload) {
  CHECK(raw==fixture_decode_host && fixture_decode_in_flight);
  KuTaskDriverSlotV1* slot=fixture_running_decode();
  const KuResult_kuvalue* parsed=(const KuResult_kuvalue*)payload;
  CHECK(parsed->ok==(fixture_valid!=0));
  CHECK(fixture_allocations_seen && fixture_cancel_calls==1u && !fixture_returns);
  CHECK(ku_task_control_status(&fixture_owner->owner.lease)==KU_TASK_CONTROL_PENDING);
  CHECK(slot->charged_bytes==fixture_prepaid_charge
      && fixture_decode_host->ticket->driver->reserved_bytes==fixture_prepaid_total);
  fixture_decode_in_flight=0;fixture_returns++;
}
static void fixture_case(uint32_t reason,int valid) {
  KuTaskDriverV1 driver={0};KuTaskDriverSlotV1 slots[2]={0};size_t ring[2]={0};
  size_t fixed=ku_task_driver_fixed_storage_bytes(2u);
  uint64_t next=UINT64_MAX,deadline=3000u;KuTaskValueV1 task={0};
  fixture_reason=reason;fixture_valid=valid;fixture_owner=&task;fixture_decode_host=NULL;
  fixture_prepaid_charge=fixture_prepaid_total=0u;fixture_allocations_seen=fixture_cancel_calls=fixture_returns=0u;
  CHECK(!fixture_decode_in_flight);
  CHECK(ku_task_driver_init_external(&driver,sizeof(driver),KU_TASK_DRIVER_ABI_VERSION,
      slots,2u,ring,2u,fixed+sizeof(KuTaskInstance_0)+(1u<<20),1u,deadline,NULL,fixture_wake)==KU_TASK_DRIVER_OK);
  const char* input=valid ? "{\"tree\":[\"中文😀\",1,true,null,{\"x\":\"owned\"}]}" : "{\"tree\":[\"allocated prefix\",{\"x\":\"owned\"}],\"bad\":}";
  KuString text=ku_string_static((const uint8_t*)input,strlen(input));
  CHECK(ku_task_0_start_value(&driver,&text,&task)==KU_TASK_DRIVER_OK && !text.ptr);
  for(size_t turn=0;turn<512u && ku_task_control_status(&task.owner.lease)==KU_TASK_CONTROL_PENDING;turn++)
    CHECK(ku_task_driver_pump(&driver,0u,1u,&next)<=KU_TASK_DRIVER_PENDING);
  CHECK(fixture_cancel_calls==1u && fixture_returns==1u && !fixture_decode_in_flight);
  CHECK(ku_task_control_status(&task.owner.lease)==reason);
  CHECK(ku_task_control_cleanup_deadline(task.owner.lease.control)==deadline);
  KuTaskAdapterOutcomeV1 outcome={0};
  CHECK(ku_task_value_take(&task,NULL,&outcome)==reason);
  CHECK(!outcome.result_kind && !outcome.value.null_value.ok);
  CHECK(ku_task_value_drop(&task,deadline)==KU_TASK_DRIVER_OK);
  CHECK(ku_task_driver_request_close(&driver,deadline)==KU_TASK_DRIVER_OK);
  for(size_t turn=0;turn<512u && driver.workers_exited!=1u;turn++)
    CHECK(ku_task_driver_pump(&driver,0u,32u,&next)<=KU_TASK_DRIVER_PENDING);
  CHECK(!driver.resident && !driver.running && !driver.queued && !driver.timers && !driver.reserved_bytes && !driver.fault);
  CHECK(ku_task_driver_join(&driver,deadline)==KU_TASK_DRIVER_OK);
  CHECK(ku_task_driver_destroy(&driver)==KU_TASK_DRIVER_OK);
  FixtureLedger empty=fixture_ledger();CHECK(!empty.allocations && !empty.bytes && !empty.overflow);
  fixture_owner=NULL;fixture_decode_host=NULL;
}
int main(void) {
  for(size_t round=0;round<4u;round++)
    for(int valid=0;valid<2;valid++) {
      fixture_case(KU_TASK_CONTROL_CANCELLED,valid);
      fixture_case(KU_TASK_CONTROL_TIMED_OUT,valid);
    }
  puts("task-json-unpublished-cancel-ok");return 0;
}
"#.replace("@RESULT_SLOT@",&result_slot.to_string());
    generated.push_str(&checks);
    let text = execute("task-json-unpublished-cancel", generated);
    if !text.is_empty() {
        assert_eq!(text, "task-json-unpublished-cancel-ok\n".repeat(4));
    }
}
