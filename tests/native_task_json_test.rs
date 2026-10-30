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
    assert_eq!(
        source.matches(needle).count(),
        1,
        "JSON test anchor: {needle}"
    );
    source.replacen(needle, replacement, 1)
}

fn generated(source: &str) -> String {
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
    let native =
        task_lower::lower_program(&ast).unwrap_or_else(|error| panic!("{error}\n{source}"));
    c::generate_native_task_c_source(&native, &Default::default()).unwrap()
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
    let source = r#"
import json from "std.json"
import time from "std.time"
async fn tick():int! { time.sleep(2)? return ok(1) }
async fn main():null! {
    decoded=json.decode("{\"counter\":7,\"data\":{\"message\":\"中文😀\",\"items\":[1,true,null]}}")?
    selected=decoded["counter"]?
    counter=selected.as_int()?
    copied=decoded["data"]?
    next=(await tick())?
    data=decoded.take("data")?
    body=json.encode({counter:counter+next,id:"rid",data:data,copy:copied})?
    println(body)
    return ok(null)
}
"#;
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
