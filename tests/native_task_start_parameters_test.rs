//! Multiple Owned Start parameters and the verified IR's sparse bit-63 boundary.
//! Slot renumbering is an IR-only probe, not a new source-language capability.
#[path = "support/native_allocation_harness.rs"]
mod native_allocation_harness;
#[allow(dead_code)]
#[path = "support/native_pg_harness.rs"]
mod native_harness;
#[path = "support/native_task_ledger.rs"]
mod native_task_ledger;

use ku::{
    backend::c,
    checker::Checker,
    ir::{
        task::{self, SlotId, TaskFunction, TaskOp, TaskSlot, TaskSlotType, TaskTerminator},
        task_lower, IrType,
    },
    lexer::Lexer,
    parser::Parser,
};
use native_allocation_harness::ALLOCATION_HOOK;
use native_harness::{
    compile_harness, run_bounded, TempDir, NATIVE_THREAD_LIFECYCLE_HARNESS, RUN_LIMITS, RUN_TIMEOUT,
};
use native_task_ledger::{LEDGER_LOCK, LOCKED_ALLOCATIONS};
use std::{fs, process::Command};

fn replace_once(source: String, anchor: &str, replacement: &str) -> String {
    assert_eq!(
        source.matches(anchor).count(),
        1,
        "parameter hook: {anchor}"
    );
    source.replacen(anchor, replacement, 1)
}

fn move_to_slot_63(function: &mut TaskFunction, from: SlotId) {
    assert!(function.slots.len() < 64 && from.0 < function.slots.len());
    let moved = function.slots[from.0].clone();
    let filler = TaskSlot {
        ty: TaskSlotType::Value {
            ty: IrType::Int,
            borrowed: false,
        },
    };
    function.slots[from.0] = filler.clone();
    function.slots.resize(64, filler);
    function.slots[63] = moved;
    let map = |slot: &mut SlotId| {
        if *slot == from {
            *slot = SlotId(63);
        }
    };
    for parameter in &mut function.parameters {
        map(parameter);
    }
    for state in &mut function.states {
        for operation in &mut state.operations {
            match operation {
                TaskOp::ScopeExitBegin { .. }
                | TaskOp::ScopeExitEnd { .. }
                | TaskOp::FinallyEnter { .. }
                | TaskOp::FinallyLeave { .. }
                | TaskOp::FinallyUnwindBegin
                | TaskOp::FinallyUnwindEnd => {}
                TaskOp::ScopeEnter { tasks, .. } => {
                    for task in tasks {
                        map(task);
                    }
                }
                TaskOp::ArrayMake { dst, elements } => {
                    map(dst);
                    for element in elements {
                        map(element);
                    }
                }
                TaskOp::Init { dst, .. } | TaskOp::InitCallable { dst, .. } => map(dst),
                TaskOp::Copy { dst, src }
                | TaskOp::CloneCallable { dst, src }
                | TaskOp::ArrayClone { dst, src }
                | TaskOp::ArrayLen { dst, src }
                | TaskOp::Move { dst, src }
                | TaskOp::WrapOk { dst, src }
                | TaskOp::WrapError { dst, src }
                | TaskOp::MakeError { dst, message: src }
                | TaskOp::ErrorField { dst, src, .. }
                | TaskOp::Unary { dst, src, .. } => {
                    map(dst);
                    map(src);
                }
                TaskOp::ArrayTake { dst, array, index } => {
                    map(dst);
                    map(array);
                    map(index);
                }
                TaskOp::Binary {
                    dst, left, right, ..
                } => {
                    map(dst);
                    map(left);
                    map(right);
                }
                TaskOp::Read { slot }
                | TaskOp::CheckForLimit { slot }
                | TaskOp::Drop { slot }
                | TaskOp::DropIfInit { slot } => map(slot),
                TaskOp::Start { dst, arguments, .. } => {
                    map(dst);
                    for argument in arguments {
                        map(argument);
                    }
                }
                TaskOp::StartIndirect {
                    dst,
                    callee,
                    arguments,
                } => {
                    map(dst);
                    map(callee);
                    for argument in arguments {
                        map(argument);
                    }
                }
                TaskOp::Print { value, .. } => map(value),
            }
        }
        match &mut state.terminator {
            TaskTerminator::Branch { condition, .. } => map(condition),
            TaskTerminator::Await { task, dst, .. } => {
                map(task);
                map(dst);
            }
            TaskTerminator::Timer { millis, dst, .. } => {
                map(millis);
                map(dst);
            }
            TaskTerminator::BlockingWrite {
                path, content, dst, ..
            } => {
                map(path);
                map(content);
                map(dst);
            }
            TaskTerminator::BlockingRead { path, dst, .. } => {
                map(path);
                map(dst);
            }
            TaskTerminator::TryResult {
                src,
                ok_value,
                err_result,
                ..
            } => {
                map(src);
                map(ok_value);
                map(err_result);
            }
            TaskTerminator::Complete { value } | TaskTerminator::Exit { value } => map(value),
            TaskTerminator::Jump { .. }
            | TaskTerminator::Suspend { .. }
            | TaskTerminator::ScopeDrain { .. }
            | TaskTerminator::Terminate => {}
        }
    }
}

#[test]
fn native_task_start_multiple_owned_sparse_slot63_restore_and_owner_release_execute_in_c() {
    run_owned_sparse_start(false);
}

#[test]
fn native_task_indirect_start_owned_inputs_restore_and_invalid_identity_do_not_consume() {
    run_owned_sparse_start(true);
}

#[test]
fn native_task_direct_flat_array_parameters_preflight_and_sparse_rollback_execute_in_c() {
    let source = r#"
async fn Inspect(numbers: [int], flags: [bool], texts: [str]): int! {
    return ok(numbers.len()+flags.len()+texts.len())
}
async fn main(): null! { return ok(null) }
"#;
    let ast = Parser::new(Lexer::new(source).lex().unwrap())
        .parse_program()
        .unwrap();
    Checker::new().check(&ast).unwrap();
    let mut native = task_lower::lower_program(&ast).unwrap();
    move_to_slot_63(&mut native.tasks.functions[0], SlotId(2));
    let plan = task::verify_and_plan(&native.tasks, Default::default()).unwrap();
    assert!(plan.functions[0].slots.contains(&SlotId(63)));
    let generated = c::generate_native_task_c_source(&native, &Default::default()).unwrap();
    assert!(!generated.contains("run_source") && !generated.contains("const SOURCE"));
    let instrumentation = format!("{NATIVE_THREAD_LIFECYCLE_HARNESS}\n{LEDGER_LOCK}\n{ALLOCATION_HOOK}\n{LOCKED_ALLOCATIONS}\nstatic int array_fail_calloc;\nstatic void* array_calloc(size_t count,size_t bytes) {{ if (array_fail_calloc) {{ array_fail_calloc=0; return NULL; }} return fixture_calloc(count,bytes); }}\n#undef calloc\n#define calloc array_calloc\n");
    let generated = replace_once(
        generated,
        "typedef struct KuString {",
        &format!("{instrumentation}typedef struct KuString {{"),
    );
    let generated = replace_once(generated, "static uint32_t ku_task_driver_commit(", "static uint32_t ku_task_driver_commit(const KuTaskDriverTicketV1*,KuTaskControlOwnerV1*,uint32_t,uint64_t);\nstatic uint32_t array_real_commit(");
    let mut generated = replace_once(
        generated,
        "int main(void) {",
        "static int fixture_unmodified_source_main(void) {",
    );
    generated.push_str(ARRAY_PARAMETER_MAIN);
    let directory = TempDir::new("native-task-array-parameters");
    let path = directory.path().join("program.c");
    fs::write(&path, generated).unwrap();
    let Some(executable) = compile_harness(directory.path(), &path, "program") else {
        assert!(
            std::env::var_os("GITHUB_ACTIONS").is_none(),
            "CI must execute array parameter ownership"
        );
        return;
    };
    fs::remove_file(path).unwrap();
    let output = run_bounded(
        Command::new(executable).current_dir(directory.path()),
        RUN_TIMEOUT,
        RUN_LIMITS,
    )
    .expect("array parameter watchdog");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8(output.stdout).unwrap().replace('\r', ""),
        "task-array-parameters-ok\n"
    );
    assert!(output.stderr.is_empty());
}

fn run_owned_sparse_start(indirect: bool) {
    let source = r#"
async fn Parent(value: str!, extra: str, count: int, flag: bool): str! {
    child = Child(value, extra, count, flag)
    return await child
}
async fn Child(value: str!, extra: str, count: int, flag: bool): str! { return value }
async fn main(): null! { return ok(null) }
"#;
    let source = if indirect {
        source.replace(
            "child = Child(value, extra, count, flag)",
            "selected = Child\n    child = selected(value, extra, count, flag)",
        )
    } else {
        source.into()
    };
    let ast = Parser::new(Lexer::new(&source).lex().unwrap())
        .parse_program()
        .unwrap();
    Checker::new().check(&ast).unwrap();
    let mut native = task_lower::lower_program(&ast).unwrap();
    assert_eq!(native.tasks.functions[0].name, "Parent");
    assert_eq!(native.tasks.functions[1].name, "Child");
    let original_arguments: Vec<_> = native.tasks.functions[0]
        .states
        .iter()
        .flat_map(|state| &state.operations)
        .filter_map(|operation| match operation {
            TaskOp::Start { arguments, .. } | TaskOp::StartIndirect { arguments, .. } => {
                Some(arguments.clone())
            }
            _ => None,
        })
        .collect();
    assert_eq!(original_arguments.len(), 1);
    assert_eq!(original_arguments[0].len(), 4);
    move_to_slot_63(&mut native.tasks.functions[0], original_arguments[0][0]);
    let child_first = native.tasks.functions[1].parameters[0];
    move_to_slot_63(&mut native.tasks.functions[1], child_first);
    assert_eq!(
        native.tasks.functions[1].parameters,
        [SlotId(63), SlotId(1), SlotId(2), SlotId(3)]
    );
    let verified = task::verify_and_plan(&native.tasks, Default::default()).unwrap();
    assert!(verified.functions[0].slots.contains(&SlotId(63)));
    assert!(verified.functions[1].slots.contains(&SlotId(63)));
    let generated = c::generate_native_task_c_source(&native, &Default::default()).unwrap();
    assert!(!generated.contains("run_source") && !generated.contains("const SOURCE"));
    assert!(generated.contains("(UINT64_C(1) << 63)"));
    let instrumentation = format!("static unsigned fixture_invalid_identity;\n{NATIVE_THREAD_LIFECYCLE_HARNESS}\n{LEDGER_LOCK}\n{ALLOCATION_HOOK}\n{LOCKED_ALLOCATIONS}\nstatic void fixture_record_free(void*);\nstatic void fixture_child_cleanup(void*);\nstatic void fixture_child_dispose(void*);\n#undef free\nstatic void fixture_counted_free(void* p) {{ fixture_record_free(p); fixture_free(p); }}\n#define free fixture_counted_free\n");
    let mut generated = replace_once(
        generated,
        "typedef struct KuString {",
        &format!("{instrumentation}typedef struct KuString {{"),
    );
    for (name, prototype) in [
        ("ku_task_driver_reserve_child", "KuTaskDriverV1*,const KuTaskDriverStartChargeV1*,size_t,KuTaskDriverTicketV1*"),
        ("ku_task_driver_commit_child", "const KuTaskDriverTicketV1*,KuTaskControlOwnerV1*,const KuTaskDriverStartChargeV1*,size_t"),
        ("ku_task_driver_transfer_charge", "const KuTaskDriverTicketV1*,const KuTaskDriverTicketV1*,size_t,size_t"),
        ("ku_task_1_try_start_impl", "KuTaskDriverV1*,const KuTaskDriverStartChargeV1*,KuResult_str*,KuString*,int64_t*,bool*,KuTaskHandle_1*"),
    ] {
        generated = replace_once(generated, &format!("static uint32_t {name}("),
            &format!("static uint32_t {name}({prototype});\nstatic uint32_t fixture_real_{name}("));
    }
    generated = replace_once(generated,
        "static uint32_t ku_task_1_cleanup(void* raw, uint32_t reason, KuTaskControlV1* budget) {",
        "static uint32_t ku_task_1_cleanup(void* raw, uint32_t reason, KuTaskControlV1* budget) {\n  fixture_child_cleanup(raw);");
    generated = replace_once(generated,
        "static void ku_task_1_dispose(KuTaskControlV1* control, void* raw) {",
        "static void ku_task_1_dispose(KuTaskControlV1* control, void* raw) {\n  fixture_child_dispose(raw);");
    generated = replace_once(
        generated,
        "int main(void) {",
        "static int fixture_unmodified_source_main(void) {",
    );
    let mut fixture_main = C_MAIN.replace("@EXTRA@", &format!("s_{}", original_arguments[0][1].0));
    if indirect {
        let call = &verified.functions[0].indirect_calls[0];
        let TaskOp::StartIndirect { callee, .. } =
            native.tasks.functions[0].states[call.state.0].operations[call.operation]
        else {
            panic!("verified indirect call")
        };
        let selector = format!("switch (frame->s_{})", callee.0);
        generated = replace_once(
            generated,
            &selector,
            &format!(
                "switch (fixture_invalid_identity ? UINT32_MAX : frame->s_{})",
                callee.0
            ),
        );
        let probe =
            INVALID_ID_CASE.replace("@EXTRA@", &format!("s_{}", original_arguments[0][1].0));
        fixture_main = replace_once(
            fixture_main,
            "int main(void) {",
            &format!("{probe}\nint main(void) {{\n  fixture_invalid_id_case();"),
        );
    }
    generated.push_str(&fixture_main);
    let directory = TempDir::new("native-task-start-parameters");
    let path = directory.path().join("program.c");
    fs::write(&path, generated).unwrap();
    let Some(executable) = compile_harness(directory.path(), &path, "program") else {
        assert!(
            std::env::var_os("GITHUB_ACTIONS").is_none(),
            "CI must execute multi-parameter sparse hosted Start"
        );
        return;
    };
    fs::remove_file(path).unwrap();
    let output = run_bounded(
        Command::new(executable).current_dir(directory.path()),
        RUN_TIMEOUT,
        RUN_LIMITS,
    )
    .expect("parameter fixture obeys the real process watchdog");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8(output.stdout).unwrap().replace('\r', ""),
        "task-start-parameters-ok\n"
    );
    assert!(output.stderr.is_empty());
}

const C_MAIN: &str = r#"
enum { FIXTURE_NORMAL=0, FIXTURE_BYTE_LIMIT=1, FIXTURE_STALE=2, FIXTURE_RELEASE_OWNER=3 };
static unsigned fixture_mode,fixture_reserves,fixture_commits,fixture_restores,fixture_takes;
static unsigned fixture_cleanups,fixture_disposes,fixture_frees[4];
static size_t fixture_buffers,fixture_fixed;
/* Integer identities survive allocation lifetime; never reevaluate dangling
 * pointer values merely to observe a later, independent free. */
static uintptr_t fixture_buffer[4];
static KuResult_str* fixture_value;
static KuString* fixture_extra;
static int64_t* fixture_count;
static bool* fixture_flag;
static KuResult_str fixture_expected_value;
static KuString fixture_expected_extra;
static KuTaskDriverTicketV1 fixture_parent,fixture_child;
static KuTestEvent fixture_entered,fixture_proceed;

static void fixture_record_free(void* pointer) {
  if (!pointer) return;
  uintptr_t identity=(uintptr_t)pointer; /* Pointer is still live before free. */
  for (size_t i=0;i<fixture_buffers;i++) if (identity==fixture_buffer[i]) {
    CHECK(!fixture_frees[i]); fixture_frees[i]++;
  }
}
static void fixture_sum_locked(KuTaskDriverV1* driver) {
  size_t sum=0,count=0;
  for (size_t i=0;i<driver->capacity;i++) {
    KuTaskDriverSlotV1* slot=&driver->slots[i];
    CHECK(slot->charged_bytes<=SIZE_MAX-sum); sum+=slot->charged_bytes;
    if (slot->state!=KU_TASK_DRIVER_FREE) count++; else CHECK(!slot->charged_bytes);
  }
  CHECK(sum==driver->reserved_bytes && count==driver->resident);
}
static void fixture_charge(KuTaskDriverV1* driver,size_t parent_bytes,size_t child_bytes) {
  CHECK(!ku_task_driver_lock(driver)); fixture_sum_locked(driver);
  KuTaskDriverSlotV1* parent=ku_task_driver_find(&fixture_parent);
  KuTaskDriverSlotV1* child=fixture_child.driver ? ku_task_driver_find(&fixture_child) : NULL;
  CHECK(parent && parent->charged_bytes==parent_bytes);
  CHECK(child_bytes ? child && child->charged_bytes==child_bytes : !child);
  CHECK(driver->reserved_bytes==parent_bytes+child_bytes);
  CHECK(!ku_task_driver_unlock(driver));
}
static int fixture_empty_string(KuString value) {
  return !value.ptr && !value.len && !value.capacity && !value.storage;
}
static int fixture_empty_result(KuResult_str value) {
  return !value.ok && fixture_empty_string(value.value) && fixture_empty_string(value.error.domain)
      && fixture_empty_string(value.error.code) && fixture_empty_string(value.error.message);
}
static void fixture_same_string(KuString value,KuString expected) {
  CHECK(value.ptr==expected.ptr && value.len==expected.len && value.capacity==expected.capacity && value.storage==expected.storage);
  if (value.len) CHECK(!memcmp(value.ptr,expected.ptr,value.len));
}
static void fixture_same_result(KuResult_str value,KuResult_str expected) {
  CHECK(value.ok==expected.ok); fixture_same_string(value.value,expected.value);
  fixture_same_string(value.error.domain,expected.error.domain);
  fixture_same_string(value.error.code,expected.error.code);
  fixture_same_string(value.error.message,expected.error.message);
}
static uint32_t ku_task_driver_reserve_child(KuTaskDriverV1* driver,
    const KuTaskDriverStartChargeV1* source,size_t bytes,KuTaskDriverTicketV1* output) {
  CHECK(!fixture_reserves++ && source->owned_bytes==48u && bytes==sizeof(KuTaskInstance_1));
  fixture_parent=source->parent;
  fixture_charge(driver,sizeof(KuTaskInstance_0)+48u,0);
  size_t calls=fixture_ledger().calls;
  uint32_t status=fixture_real_ku_task_driver_reserve_child(driver,source,bytes,output);
  CHECK(fixture_ledger().calls==calls);
  if (fixture_mode==FIXTURE_BYTE_LIMIT) {
    CHECK(status==KU_TASK_DRIVER_LIMIT && !output->driver);
    fixture_charge(driver,sizeof(KuTaskInstance_0)+48u,0);
  } else {
    CHECK(status==KU_TASK_DRIVER_OK); fixture_child=*output;
    fixture_charge(driver,sizeof(KuTaskInstance_0)+48u,bytes);
  }
  return status;
}
static uint32_t ku_task_driver_commit_child(const KuTaskDriverTicketV1* ticket,
    KuTaskControlOwnerV1* owner,const KuTaskDriverStartChargeV1* source,size_t bytes) {
  CHECK(!fixture_commits++ && fixture_empty_result(*fixture_value) && fixture_empty_string(*fixture_extra));
  CHECK(*fixture_count==37 && *fixture_flag);
  KuTaskInstance_0* parent=(KuTaskInstance_0*)source->parent_control;
  KuTaskInstance_1* child=(KuTaskInstance_1*)owner->lease.control;
  CHECK(parent->frame.header.initialized & (UINT64_C(1)<<63));
  CHECK(fixture_empty_result(parent->frame.s_63) && fixture_empty_string(parent->frame.@EXTRA@));
  CHECK(child->frame.header.initialized==((UINT64_C(1)<<63)|UINT64_C(14)));
  fixture_same_result(child->frame.s_63,fixture_expected_value);
  fixture_same_string(child->frame.s_1,fixture_expected_extra);
  CHECK(child->frame.s_2==37 && child->frame.s_3);
  CHECK(ku_task_control_atomic_load(&owner->lease.control->references)==2u);
  fixture_charge(ticket->driver,sizeof(KuTaskInstance_0)+48u,bytes);
  if (fixture_mode==FIXTURE_RELEASE_OWNER) {
    CHECK(ku_test_event_set(&fixture_entered)); CHECK(ku_test_event_wait(&fixture_proceed,2000));
    CHECK(!ku_task_driver_lock(ticket->driver));
    KuTaskDriverSlotV1* live_parent=ku_task_driver_find(&source->parent);
    CHECK(live_parent && live_parent->state==KU_TASK_DRIVER_RUNNING && live_parent->owner_location==KU_TASK_DRIVER_OWNER_DEFERRED);
    CHECK(live_parent->driver_lease.control==&parent->control && live_parent->binding==&parent->control);
    CHECK(ku_task_control_atomic_load(&parent->control.executor)==1u);
    CHECK(!parent->control.frame_destroyed && parent->frame_initialized);
    fixture_sum_locked(ticket->driver); CHECK(!ku_task_driver_unlock(ticket->driver));
  }
  KuTaskDriverStartChargeV1 bad=*source;
  if (fixture_mode==FIXTURE_STALE) {
    bad.parent.generation++;
    /* Only a fault probe: diverge private Copy fields before a genuine rejected
     * commit. Incorrect generic restore would overwrite untouched caller data. */
    child->frame.s_2=-777; child->frame.s_3=false;
  }
  uint32_t status=fixture_real_ku_task_driver_commit_child(ticket,owner,
      fixture_mode==FIXTURE_STALE ? &bad : source,bytes);
  CHECK(status==(fixture_mode==FIXTURE_STALE ? KU_TASK_DRIVER_STALE : KU_TASK_DRIVER_OK));
  fixture_charge(ticket->driver,sizeof(KuTaskInstance_0)+(status==KU_TASK_DRIVER_OK ? 0u : 48u),
      bytes+(status==KU_TASK_DRIVER_OK ? 48u : 0u));
  return status;
}
static uint32_t ku_task_1_try_start_impl(KuTaskDriverV1* driver,
    const KuTaskDriverStartChargeV1* source,KuResult_str* value,KuString* extra,int64_t* count,bool* flag,
    KuTaskHandle_1* output) {
  CHECK(source && source->owned_bytes==48u);
  fixture_value=value; fixture_extra=extra; fixture_count=count; fixture_flag=flag;
  fixture_expected_value=*value; fixture_expected_extra=*extra; /* Never owning copies. */
  CHECK(*count==37 && *flag);
  size_t calls=fixture_ledger().calls;
  uint32_t status=fixture_real_ku_task_1_try_start_impl(driver,source,value,extra,count,flag,output);
  CHECK(fixture_ledger().calls==calls+(fixture_mode==FIXTURE_BYTE_LIMIT ? 0u : 1u));
  CHECK(*count==37 && *flag);
  if (status!=KU_TASK_DRIVER_OK) {
    CHECK(status==(fixture_mode==FIXTURE_BYTE_LIMIT ? KU_TASK_DRIVER_LIMIT : KU_TASK_DRIVER_STALE));
    fixture_same_result(*value,fixture_expected_value); fixture_same_string(*extra,fixture_expected_extra);
    CHECK(!output->owner.lease.control && !output->ticket.driver && !output->ticket.slot && !output->ticket.generation);
    fixture_child=(KuTaskDriverTicketV1){0};
    fixture_charge(driver,sizeof(KuTaskInstance_0)+48u,0);
    CHECK(fixture_ledger().allocations==4u+fixture_buffers);
    CHECK(fixture_ledger().bytes==fixture_fixed+sizeof(KuTaskInstance_0)+48u);
    for (size_t i=0;i<fixture_buffers;i++) CHECK(!fixture_frees[i]);
    fixture_restores++;
  } else {
    CHECK(fixture_empty_result(*value) && fixture_empty_string(*extra));
    CHECK(output->owner.lease.control);
  }
  return status;
}
static uint32_t ku_task_driver_transfer_charge(const KuTaskDriverTicketV1* from,
    const KuTaskDriverTicketV1* to,size_t bytes,size_t minimum) {
  CHECK(!fixture_takes++ && fixture_mode==FIXTURE_NORMAL && bytes==31u && minimum==sizeof(KuTaskInstance_1));
  fixture_charge(from->driver,sizeof(KuTaskInstance_0),sizeof(KuTaskInstance_1)+48u);
  uint32_t status=fixture_real_ku_task_driver_transfer_charge(from,to,bytes,minimum);
  CHECK(status==KU_TASK_DRIVER_OK);
  /* The dropped extra allocation's conservative 17-byte reservation remains
   * with Child until dispose; successful Await only moves its active Result. */
  fixture_charge(from->driver,sizeof(KuTaskInstance_0)+31u,sizeof(KuTaskInstance_1)+17u);
  return status;
}
static void fixture_child_cleanup(void* raw) {
  KuTaskInstance_1* child=(KuTaskInstance_1*)raw; fixture_cleanups++;
  if (fixture_mode==FIXTURE_STALE) {
    CHECK(!child->registered && child->frame_initialized);
    CHECK(!(child->frame.header.initialized & ((UINT64_C(1)<<63)|UINT64_C(2))));
    CHECK((child->frame.header.initialized & UINT64_C(12))==UINT64_C(12));
    CHECK(fixture_empty_result(child->frame.s_63) && fixture_empty_string(child->frame.s_1));
    CHECK(child->frame.s_2==-777 && !child->frame.s_3);
    CHECK(ku_task_control_atomic_load(&child->control.references)==2u);
    CHECK(ku_task_control_atomic_load(&child->control.lifecycle_pin)==1u);
  }
}
static void fixture_child_dispose(void* raw) {
  KuTaskInstance_1* child=(KuTaskInstance_1*)raw; fixture_disposes++;
  CHECK(!child->frame_initialized && !child->payload_initialized);
  CHECK(!ku_task_control_atomic_load(&child->control.references));
  CHECK(!ku_task_control_atomic_load(&child->control.lifecycle_pin));
}
static KuString fixture_owned(size_t bytes,const char* text) {
  CHECK(fixture_buffers<4u && strlen(text)<=bytes);
  uint8_t* pointer=(uint8_t*)malloc(bytes); CHECK(pointer); memcpy(pointer,text,strlen(text));
  fixture_buffer[fixture_buffers++]=(uintptr_t)pointer;
  return (KuString){pointer,strlen(text),bytes,KU_STRING_OWNED};
}
static void fixture_case(unsigned mode,int error) {
  CHECK(!fixture_ledger().allocations && !fixture_ledger().bytes);
  fixture_mode=mode; fixture_reserves=fixture_commits=fixture_restores=fixture_takes=0;
  fixture_cleanups=fixture_disposes=0; fixture_buffers=0;
  memset(fixture_frees,0,sizeof(fixture_frees)); memset(fixture_buffer,0,sizeof(fixture_buffer));
  fixture_parent=(KuTaskDriverTicketV1){0}; fixture_child=(KuTaskDriverTicketV1){0};
  CHECK(ku_test_event_init(&fixture_entered)); CHECK(ku_test_event_init(&fixture_proceed));
  KuTaskDriverV1* driver=(KuTaskDriverV1*)calloc(1,sizeof(*driver));
  KuTaskDriverSlotV1* slots=(KuTaskDriverSlotV1*)calloc(2,sizeof(*slots));
  size_t* ring=(size_t*)calloc(2,sizeof(*ring)); CHECK(driver && slots && ring);
  fixture_fixed=sizeof(*driver)+2u*(sizeof(*slots)+sizeof(*ring));
  size_t limit=fixture_fixed+sizeof(KuTaskInstance_0)+48u+sizeof(KuTaskInstance_1)-(mode==FIXTURE_BYTE_LIMIT ? 1u : 0u);
  uint64_t startup_now=ku_task_driver_now_ms();
  CHECK(startup_now!=UINT64_MAX && startup_now<UINT64_MAX-1000u);
  uint64_t startup_deadline=startup_now+1000u;
  CHECK(ku_task_driver_init(driver,sizeof(*driver),6u,slots,2,ring,2,limit,1u,startup_deadline)==KU_TASK_DRIVER_ABI_MISMATCH);
  CHECK(ku_task_frame_zero_bytes(driver,sizeof(*driver))
      && ku_task_frame_zero_bytes(slots,(2)*sizeof(*slots))
      && ku_task_frame_zero_bytes(ring,(2)*sizeof(*ring)));
  CHECK(ku_task_driver_init(driver,sizeof(*driver),KU_TASK_DRIVER_ABI_VERSION,slots,2,ring,2,limit,1u,startup_deadline)==KU_TASK_DRIVER_OK);
  KuResult_str value={0}; value.ok=!error;
  if (error) value.error=(KuError){fixture_owned(7,"domain"),fixture_owned(11,"code"),fixture_owned(13,"message")};
  else value.value=fixture_owned(31,"payload");
  KuString extra=fixture_owned(17,"extra");
  KuResult_str original=value; /* Only inspected while its allocation is live. */
  uintptr_t extra_identity=(uintptr_t)extra.ptr;
  int64_t count=37; bool flag=true; KuTaskValueV1 root={0};
  CHECK(ku_task_0_start_value(driver,&value,&extra,&count,&flag,&root)==KU_TASK_DRIVER_OK);
  CHECK(root.tag==KU_TASK_VALUE_LIVE && fixture_empty_result(value) && fixture_empty_string(extra) && count==37 && flag);
  uint64_t cleanup_now=ku_task_driver_now_ms();
  CHECK(cleanup_now!=UINT64_MAX && cleanup_now<UINT64_MAX-1000u);
  uint64_t deadline=cleanup_now+1000u;
  if (mode==FIXTURE_RELEASE_OWNER) {
    CHECK(ku_test_event_wait(&fixture_entered,2000));
    CHECK(ku_task_value_drop(&root,deadline)==KU_TASK_DRIVER_OK);
    CHECK(!root.tag && !root.owner.lease.control && !root.ticket.driver);
    /* No observer was retained. Driver's genuine deferred owner, registry and
     * execution references protect the still-running parent across BUILDING. */
    fixture_charge(driver,sizeof(KuTaskInstance_0)+48u,sizeof(KuTaskInstance_1));
    CHECK(ku_test_event_set(&fixture_proceed));
  }
  uint32_t fault=mode==FIXTURE_STALE ? KU_TASK_DRIVER_INTERNAL : KU_TASK_DRIVER_OK;
  CHECK(ku_task_driver_wait_idle(driver,deadline)==fault);
  CHECK(fixture_reserves==1u && fixture_commits==(mode==FIXTURE_BYTE_LIMIT ? 0u : 1u));
  if (mode==FIXTURE_STALE) {
    CHECK(fixture_restores==1u && fixture_disposes==1u && fixture_cleanups==1u);
    /* Observe the real failed resume first; only then request normal cleanup.
     * This does not claim the ABI corruption became an automatic user Result. */
    CHECK(ku_task_driver_request_cancel(&root.ticket,&root.owner.lease,KU_TASK_CONTROL_CANCELLED,deadline)==KU_TASK_CONTROL_OK);
    CHECK(ku_task_driver_wait_idle(driver,deadline)==KU_TASK_DRIVER_INTERNAL);
  }
  KuTaskAdapterOutcomeV1 output={0};
  if (mode==FIXTURE_NORMAL) {
    CHECK(fixture_takes==1u && !fixture_restores && !fixture_cleanups);
    CHECK(ku_task_value_take(&root,NULL,&output)==KU_TASK_CONTROL_OK);
    CHECK(output.result_kind==4u && output.exit_class==KU_TASK_EXIT_USER_RESULT);
    fixture_same_result(output.value.string,original);
    CHECK(fixture_frees[fixture_buffers-1u]==1u); /* extra dropped by real Child */
    CHECK(extra_identity==fixture_buffer[fixture_buffers-1u]);
  } else if (mode==FIXTURE_BYTE_LIMIT) {
    CHECK(fixture_restores==1u && !fixture_disposes && !fixture_cleanups && !fixture_takes);
    CHECK(ku_task_value_take(&root,NULL,&output)==KU_TASK_CONTROL_OK);
    CHECK(output.exit_class==KU_TASK_EXIT_USER_RESULT && !output.value.string.ok);
    CHECK(output.value.string.error.domain.len==4u && !memcmp(output.value.string.error.domain.ptr,"task",4u));
    CHECK(output.value.string.error.code.len==14u && !memcmp(output.value.string.error.code.ptr,"too_many_tasks",14u));
  } else if (mode==FIXTURE_STALE) {
    CHECK(ku_task_value_take(&root,NULL,&output)==KU_TASK_CONTROL_CANCELLED && ku_task_outcome_empty(&output,4u));
  } else CHECK(!fixture_takes && !fixture_restores && fixture_cleanups==1u && fixture_disposes==1u);
  ku_task_outcome_drop(&output);
  if (root.tag) CHECK(ku_task_value_drop(&root,deadline)==KU_TASK_DRIVER_OK);
  CHECK(ku_task_driver_shutdown(driver,deadline)==fault);
  KuTaskDriverSnapshotV1 snapshot={0}; CHECK(ku_task_driver_snapshot(driver,&snapshot)==KU_TASK_DRIVER_OK);
  CHECK(snapshot.fault==fault && !snapshot.resident && !snapshot.reserved_bytes && !snapshot.building && !snapshot.queued && !snapshot.running);
  CHECK(ku_task_driver_join(driver,deadline)==KU_TASK_DRIVER_OK);
  CHECK(driver->worker_target==1u && driver->workers_created==1u && driver->workers_exited==1u
      && driver->workers_joined==1u && driver->workers[0].joined && driver->workers[0].closed);
  CHECK(ku_task_driver_destroy(driver)==KU_TASK_DRIVER_OK);
  free(ring); free(slots); free(driver);
  for (size_t i=0;i<fixture_buffers;i++) CHECK(fixture_frees[i]==1u);
  CHECK(!fixture_ledger().allocations && !fixture_ledger().bytes && !fixture_ledger().overflow);
  CHECK(ku_test_event_destroy(&fixture_entered)); CHECK(ku_test_event_destroy(&fixture_proceed));
}
int main(void) {
  for (int error=0;error<2;error++) for (unsigned mode=FIXTURE_NORMAL;mode<=FIXTURE_RELEASE_OWNER;mode++) fixture_case(mode,error);
  puts("task-start-parameters-ok"); return 0;
}
"#;

const INVALID_ID_CASE: &str = r#"
static uint64_t fixture_actual_now(void* context) {
  (void)context; return ku_task_driver_now_ms();
}
static void fixture_invalid_id_case(void) {
  CHECK(!fixture_ledger().allocations && !fixture_ledger().bytes);
  fixture_buffers=0; fixture_reserves=fixture_commits=fixture_restores=fixture_takes=0;
  memset(fixture_frees,0,sizeof(fixture_frees)); memset(fixture_buffer,0,sizeof(fixture_buffer));
  KuResult_str value={0}; value.ok=true; value.value=fixture_owned(31,"payload");
  KuString extra=fixture_owned(17,"extra");
  KuResult_str expected_value=value; KuString expected_extra=extra; /* Non-owning observations. */
  int64_t count=37; bool flag=true;
  KuTaskFrame_0 frame={0};
  CHECK(ku_task_frame_0_init(&frame,sizeof(frame),KU_TASK_FRAME_ABI_VERSION,&value,&extra,&count,&flag)==KU_TASK_FRAME_OK);
  CHECK(fixture_empty_result(value) && fixture_empty_string(extra));
  KuTaskControlV1 control={0}; ku_task_control_atomic_init(&control.phase,KU_TASK_CONTROL_LIVE);
  KuTaskAdapterHostV1 host={0}; host.control=&control;
  KuTaskFrameClockV1 clock={fixture_actual_now,NULL,&host};
  size_t calls=fixture_ledger().calls;
  /* Fault only the already materialized dispatch selector. No Start, Await,
   * argument move, allocation, or result is replaced by this unit probe. */
  fixture_invalid_identity=1;
  CHECK(ku_task_frame_0_resume(&frame,sizeof(frame),KU_TASK_FRAME_ABI_VERSION,&clock)==KU_TASK_FRAME_INVALID_STATE);
  fixture_invalid_identity=0;
  fixture_same_result(frame.s_63,expected_value);
  fixture_same_string(frame.@EXTRA@,expected_extra);
  CHECK(frame.header.initialized & (UINT64_C(1)<<63));
  CHECK(!frame.header.running && !fixture_reserves && !fixture_commits && !fixture_restores && !fixture_takes);
  CHECK(fixture_ledger().calls==calls && fixture_ledger().allocations==2u && fixture_ledger().bytes==48u);
  uint64_t now=ku_task_driver_now_ms(); CHECK(now!=UINT64_MAX && now<UINT64_MAX-1000u);
  CHECK(ku_task_frame_0_terminate(&frame,sizeof(frame),KU_TASK_FRAME_ABI_VERSION,KU_TASK_FRAME_CANCELLED,now+1000u,&clock)==KU_TASK_FRAME_CANCELLED);
  CHECK(!frame.header.initialized);
  CHECK(ku_task_frame_0_destroy(&frame,sizeof(frame),KU_TASK_FRAME_ABI_VERSION)==KU_TASK_FRAME_OK);
  CHECK(fixture_frees[0]==1u && fixture_frees[1]==1u);
  CHECK(!fixture_ledger().allocations && !fixture_ledger().bytes && !fixture_ledger().overflow);
}
"#;

const ARRAY_PARAMETER_MAIN: &str = r#"
typedef struct ArrayParameterRuntime {
  KuTaskDriverV1 driver;
  KuTaskDriverSlotV1 slots[1];
  size_t ring[1];
} ArrayParameterRuntime;
typedef struct ArrayParameterInputs {
  KuArray_int numbers;
  KuArray_bool flags;
  KuArray_str texts;
} ArrayParameterInputs;
static ArrayParameterInputs array_expected;
static ArrayParameterInputs* array_source;
static unsigned array_fail_commit,array_commits,array_rejections;
static uintptr_t array_leaf[2];
static uint64_t array_deadline(void) {
  uint64_t now=ku_task_driver_now_ms(); CHECK(now!=UINT64_MAX && now<UINT64_MAX-2000u);
  return now+2000u;
}
static size_t array_bytes(ArrayParameterInputs* input) {
  return input->numbers.capacity*sizeof(*input->numbers.data)
      +input->flags.capacity*sizeof(*input->flags.data)
      +input->texts.capacity*sizeof(*input->texts.data)+32u;
}
static ArrayParameterInputs array_input(void) {
  ArrayParameterInputs input={0};
  input.numbers=(KuArray_int){2,calloc(4,sizeof(int64_t)),4};
  input.flags=(KuArray_bool){2,calloc(4,sizeof(bool)),4};
  input.texts=(KuArray_str){3,calloc(4,sizeof(KuString)),4};
  CHECK(input.numbers.data && input.flags.data && input.texts.data);
  input.numbers.data[0]=17; input.numbers.data[1]=-9;
  input.flags.data[0]=true;
  input.texts.data[0]=(KuString){malloc(31u),7u,31u,KU_STRING_OWNED};
  input.texts.data[1]=(KuString){malloc(1u),0u,0u,KU_STRING_OWNED};
  input.texts.data[2]=ku_string_static((const uint8_t*)"\xe4\xb8\xad",3u);
  CHECK(input.texts.data[0].ptr && input.texts.data[1].ptr);
  memcpy(input.texts.data[0].ptr,"payload",7u);
  array_leaf[0]=(uintptr_t)input.texts.data[0].ptr; array_leaf[1]=(uintptr_t)input.texts.data[1].ptr;
  return input;
}
static void array_unchanged(ArrayParameterInputs* value) {
  CHECK(value->numbers.data==array_expected.numbers.data && value->numbers.len==2u && value->numbers.capacity==4u);
  CHECK(value->flags.data==array_expected.flags.data && value->flags.len==2u && value->flags.capacity==4u);
  CHECK(value->texts.data==array_expected.texts.data && value->texts.len==3u && value->texts.capacity==4u);
  CHECK(value->numbers.data[0]==17 && value->numbers.data[1]==-9);
  CHECK(value->flags.data[0] && !value->flags.data[1]);
  CHECK((uintptr_t)value->texts.data[0].ptr==array_leaf[0] && value->texts.data[0].len==7u
      && value->texts.data[0].capacity==31u && !memcmp(value->texts.data[0].ptr,"payload",7u));
  CHECK((uintptr_t)value->texts.data[1].ptr==array_leaf[1] && !value->texts.data[1].len && !value->texts.data[1].capacity);
}
static KuTaskDriverSnapshotV1 array_snapshot(ArrayParameterRuntime* runtime) {
  KuTaskDriverSnapshotV1 result={0};
  CHECK(ku_task_driver_snapshot(&runtime->driver,&result)==KU_TASK_DRIVER_OK); return result;
}
static void array_idle(ArrayParameterRuntime* runtime) {
  CHECK(ku_task_driver_wait_idle(&runtime->driver,array_deadline())==KU_TASK_DRIVER_OK);
  KuTaskDriverSnapshotV1 snapshot=array_snapshot(runtime);
  CHECK(!snapshot.fault && !snapshot.resident && !snapshot.reserved_bytes && !snapshot.building && !snapshot.queued && !snapshot.running);
}
static void array_init(ArrayParameterRuntime* runtime,size_t budget) {
  memset(runtime,0,sizeof(*runtime));
  size_t fixed=sizeof(runtime->driver)+sizeof(runtime->slots)+sizeof(runtime->ring);
  CHECK(ku_task_driver_init(&runtime->driver,sizeof(runtime->driver),KU_TASK_DRIVER_ABI_VERSION,
      runtime->slots,1u,runtime->ring,1u,fixed+budget,1u,array_deadline())==KU_TASK_DRIVER_OK);
}
static void array_finish(ArrayParameterRuntime* runtime) {
  array_idle(runtime); uint64_t deadline=array_deadline();
  CHECK(ku_task_driver_shutdown(&runtime->driver,deadline)==KU_TASK_DRIVER_OK);
  CHECK(ku_task_driver_join(&runtime->driver,deadline)==KU_TASK_DRIVER_OK);
  CHECK(runtime->driver.workers_created==1u && runtime->driver.workers_joined==1u);
  CHECK(ku_task_driver_destroy(&runtime->driver)==KU_TASK_DRIVER_OK);
  CHECK(!fixture_ledger().allocations && !fixture_ledger().bytes && !fixture_ledger().overflow);
}
static uint32_t ku_task_driver_commit(const KuTaskDriverTicketV1* ticket,
    KuTaskControlOwnerV1* owner,uint32_t mode,uint64_t deadline) {
  CHECK(mode==KU_TASK_DRIVER_START); array_commits++;
  KuTaskInstance_0* instance=(KuTaskInstance_0*)owner->lease.control;
  CHECK(instance->frame.header.initialized==(UINT64_C(3)|(UINT64_C(1)<<63)));
  CHECK(instance->frame.s_0.data==array_expected.numbers.data && instance->frame.s_0.capacity==4u);
  CHECK(instance->frame.s_1.data==array_expected.flags.data && instance->frame.s_1.capacity==4u);
  CHECK(instance->frame.s_63.data==array_expected.texts.data && instance->frame.s_63.capacity==4u);
  CHECK((uintptr_t)instance->frame.s_63.data[0].ptr==array_leaf[0] && (uintptr_t)instance->frame.s_63.data[1].ptr==array_leaf[1]);
  CHECK(!array_source->numbers.data && !array_source->numbers.len && !array_source->numbers.capacity);
  CHECK(!array_source->flags.data && !array_source->flags.len && !array_source->flags.capacity);
  CHECK(!array_source->texts.data && !array_source->texts.len && !array_source->texts.capacity);
  CHECK(!ku_task_driver_lock(ticket->driver));
  KuTaskDriverSlotV1* slot=ku_task_driver_find(ticket);
  CHECK(slot && slot->charged_bytes==sizeof(*instance)+array_bytes(&array_expected));
  CHECK(!ku_task_driver_unlock(ticket->driver));
  KuTaskDriverTicketV1 stale=*ticket; stale.generation++;
  return array_real_commit(array_fail_commit ? &stale : ticket,owner,mode,deadline);
}
/* Capture actual live input storage separately from deliberately malformed raw
 * argument headers. Never dereference a forged candidate vector to inspect it. */
static void array_reject(ArrayParameterRuntime* runtime,KuArray_int* numbers,
    KuArray_bool* flags,KuArray_str* texts,unsigned variant) {
  KuArray_int before_numbers=*numbers; KuArray_bool before_flags=*flags; KuArray_str before_texts=*texts;
  int64_t saved_numbers[4]; bool saved_flags[4]; KuString saved_texts[4];
  memcpy(saved_numbers,array_expected.numbers.data,sizeof(saved_numbers));
  memcpy(saved_flags,array_expected.flags.data,sizeof(saved_flags));
  memcpy(saved_texts,array_expected.texts.data,sizeof(saved_texts));
  FixtureLedger ledger=fixture_ledger(); KuTaskDriverSnapshotV1 before=array_snapshot(runtime);
  KuTaskHandle_0 output={0}; KuTaskValueV1 value={0}; uint32_t status;
  KuTaskDriverTicketV1 ticket={&runtime->driver,0,0}; KuTaskControlV1 control={0};
  KuTaskDriverTicketV1 original_ticket=ticket;
  KuTaskAdapterHostV1 host={&ticket,&control,NULL,sizeof(KuTaskInstance_0)};
  /* The mapped ticket is deliberately only sizeof(ticket), smaller than one
   * KuString. Known parent overlap must reject before scanning a vector leaf. */
  KuArray_str ticket_vector={variant==12 ? 2u : 1u,(KuString*)&ticket,variant==12 ? 2u : 1u};
  if (variant==1) status=ku_task_0_try_start(&runtime->driver,numbers,flags,texts,(KuTaskHandle_0*)numbers);
  else if (variant==2) status=ku_task_0_try_start(&runtime->driver,numbers,flags,texts,(KuTaskHandle_0*)numbers->data);
  else if (variant==3) status=ku_task_0_try_start(&runtime->driver,numbers,flags,texts,(KuTaskHandle_0*)texts->data[0].ptr);
  else if (variant==4) status=ku_task_0_try_start_impl(&runtime->driver,(const KuTaskDriverStartChargeV1*)texts->data,numbers,flags,texts,&output);
  else if (variant==5) status=ku_task_0_start_hosted((const KuTaskAdapterHostV1*)numbers->data,numbers,flags,texts,&value);
  else if (variant==6) { host.ticket=(const KuTaskDriverTicketV1*)numbers->data; status=ku_task_0_start_hosted(&host,numbers,flags,texts,&value); }
  else if (variant==7) { host.control=(KuTaskControlV1*)texts->data[0].ptr; status=ku_task_0_start_hosted(&host,numbers,flags,texts,&value); }
  else if (variant==8) status=ku_task_0_try_start(&runtime->driver,numbers,flags,texts,(KuTaskHandle_0*)&texts->data[3]);
  else if (variant==9) status=ku_task_0_try_start(&runtime->driver,numbers,(KuArray_bool*)numbers,texts,&output);
  /* This is the genuine one-byte malloc owned by an empty string. Do not pad
   * it into a readable ticket merely to make premature parent reads harmless. */
  else if (variant==10) { host.ticket=(const KuTaskDriverTicketV1*)texts->data[1].ptr; status=ku_task_0_start_hosted(&host,numbers,flags,texts,&value); }
  else if (variant==11 || variant==12) status=ku_task_0_start_hosted(&host,numbers,flags,&ticket_vector,&value);
  else status=ku_task_0_try_start(&runtime->driver,numbers,flags,texts,&output);
  CHECK(status==KU_TASK_DRIVER_INVALID_ARGUMENT);
  CHECK(numbers->data==before_numbers.data && numbers->len==before_numbers.len && numbers->capacity==before_numbers.capacity);
  CHECK(flags->data==before_flags.data && flags->len==before_flags.len && flags->capacity==before_flags.capacity);
  CHECK(texts->data==before_texts.data && texts->len==before_texts.len && texts->capacity==before_texts.capacity);
  CHECK(!memcmp(saved_numbers,array_expected.numbers.data,sizeof(saved_numbers)));
  CHECK(!memcmp(saved_flags,array_expected.flags.data,sizeof(saved_flags)));
  CHECK(!memcmp(saved_texts,array_expected.texts.data,sizeof(saved_texts)));
  CHECK(!output.owner.lease.control && !output.ticket.driver && !value.tag && !value.owner.lease.control && !value.ticket.driver);
  CHECK(ticket.driver==original_ticket.driver && ticket.slot==original_ticket.slot && ticket.generation==original_ticket.generation);
  CHECK(ticket_vector.data==(KuString*)&ticket && ticket_vector.len==(variant==12 ? 2u : 1u)
      && ticket_vector.capacity==(variant==12 ? 2u : 1u));
  FixtureLedger after_ledger=fixture_ledger(); KuTaskDriverSnapshotV1 after=array_snapshot(runtime);
  CHECK(after_ledger.calls==ledger.calls && after_ledger.allocations==ledger.allocations && after_ledger.bytes==ledger.bytes);
  CHECK(after.resident==before.resident && after.building==before.building && after.reserved_bytes==before.reserved_bytes);
  CHECK(after.queued==before.queued && after.running==before.running && after.fault==before.fault);
  CHECK(runtime->slots[0].state==KU_TASK_DRIVER_FREE && !runtime->slots[0].generation);
  array_rejections++;
}
static void array_preflight(ArrayParameterRuntime* runtime,ArrayParameterInputs* input) {
  KuArray_int bad=input->numbers;
  bad.len=5u; array_reject(runtime,&bad,&input->flags,&input->texts,0);
  bad=input->numbers; bad.capacity=65u; array_reject(runtime,&bad,&input->flags,&input->texts,0);
  bad=input->numbers; bad.capacity=SIZE_MAX; array_reject(runtime,&bad,&input->flags,&input->texts,0);
  bad=input->numbers; bad.data=NULL; array_reject(runtime,&bad,&input->flags,&input->texts,0);
  bad=input->numbers; bad.len=bad.capacity=0u; array_reject(runtime,&bad,&input->flags,&input->texts,0);
  bad=input->numbers; bad.data=(int64_t*)((uint8_t*)input->numbers.data+1u); array_reject(runtime,&bad,&input->flags,&input->texts,0);
  bad=input->numbers; bad.data=(int64_t*)(uintptr_t)(UINTPTR_MAX-7u); bad.capacity=2u; array_reject(runtime,&bad,&input->flags,&input->texts,0);
  bad=input->numbers; bad.data=(int64_t*)&input->flags; array_reject(runtime,&bad,&input->flags,&input->texts,0);
  bad=input->numbers; bad.data=(int64_t*)runtime->slots; array_reject(runtime,&bad,&input->flags,&input->texts,0);
  KuArray_str bad_texts=input->texts;
  bad_texts.data=(KuString*)((uint8_t*)input->texts.data+1u); array_reject(runtime,&input->numbers,&input->flags,&bad_texts,0);
  bad_texts=input->texts; bad_texts.data=(KuString*)runtime->slots; array_reject(runtime,&input->numbers,&input->flags,&bad_texts,0);
  KuString saved=input->texts.data[0];
  input->texts.data[0].len=32u; array_reject(runtime,&input->numbers,&input->flags,&input->texts,0);
  input->texts.data[0]=saved; input->texts.data[0].ptr=(uint8_t*)(uintptr_t)UINTPTR_MAX;
  input->texts.data[0].len=1u; input->texts.data[0].capacity=2u; array_reject(runtime,&input->numbers,&input->flags,&input->texts,0);
  input->texts.data[0]=saved; input->texts.data[0].ptr=(uint8_t*)&runtime->driver;
  array_reject(runtime,&input->numbers,&input->flags,&input->texts,0); input->texts.data[0]=saved;
  input->texts.data[0].ptr=(uint8_t*)&input->flags;
  array_reject(runtime,&input->numbers,&input->flags,&input->texts,0); input->texts.data[0]=saved;
  input->texts.data[0].ptr=(uint8_t*)input->texts.data;
  array_reject(runtime,&input->numbers,&input->flags,&input->texts,0); input->texts.data[0]=saved;
  KuString second=input->texts.data[1]; input->texts.data[1]=saved;
  array_reject(runtime,&input->numbers,&input->flags,&input->texts,0); input->texts.data[1]=second;
  for (unsigned variant=1;variant<=12u;variant++) array_reject(runtime,&input->numbers,&input->flags,&input->texts,variant);
  array_unchanged(input); CHECK(array_rejections==29u);
}
static void array_case(unsigned mode) {
  CHECK(!fixture_ledger().allocations && !fixture_ledger().bytes);
  array_fail_calloc=array_fail_commit=array_commits=0;
  ArrayParameterInputs input=array_input(); array_expected=input; array_source=&input;
  ArrayParameterRuntime runtime; array_init(&runtime,sizeof(KuTaskInstance_0)+array_bytes(&input));
  if (!mode) array_preflight(&runtime,&input);
  FixtureLedger baseline=fixture_ledger(); KuTaskHandle_0 handle={0};
  if (mode==1u) array_fail_calloc=1;
  if (mode==2u) array_fail_commit=1;
  uint32_t status=ku_task_0_try_start(&runtime.driver,&input.numbers,&input.flags,&input.texts,&handle);
  CHECK(!array_fail_calloc); array_fail_commit=0;
  if (mode) {
    CHECK(status==(mode==1u ? KU_TASK_ADAPTER_OUT_OF_MEMORY : KU_TASK_DRIVER_STALE));
    array_unchanged(&input); CHECK(!handle.owner.lease.control && !handle.ticket.driver);
    array_idle(&runtime); FixtureLedger after=fixture_ledger();
    CHECK(after.allocations==baseline.allocations && after.bytes==baseline.bytes);
    CHECK(after.calls==baseline.calls+(mode==2u ? 1u : 0u));
    CHECK(array_commits==(mode==2u ? 1u : 0u));
    ku_array_drop_int(&input.numbers); ku_array_drop_bool(&input.flags); ku_array_drop_str(&input.texts);
  } else {
    CHECK(status==KU_TASK_DRIVER_OK && array_commits==1u);
    CHECK(!input.numbers.data && !input.flags.data && !input.texts.data);
    CHECK(ku_task_driver_wait_result(&handle.ticket,&handle.owner.lease,array_deadline())==KU_TASK_DRIVER_WAIT_READY);
    CHECK(ku_task_driver_wait_idle(&runtime.driver,array_deadline())==KU_TASK_DRIVER_OK);
    KuTaskAdapterOutcomeV1 output={0}; CHECK(ku_task_0_take(&handle,&output)==KU_TASK_CONTROL_OK);
    CHECK(output.result_kind==1u && output.value.integer.ok && output.value.integer.value==7);
    ku_task_outcome_drop(&output); CHECK(ku_task_0_drop(&handle,array_deadline())==KU_TASK_DRIVER_OK);
  }
  array_finish(&runtime);
}
static void array_rejected_value(unsigned mode) {
  CHECK(!fixture_ledger().allocations && !fixture_ledger().bytes);
  array_fail_calloc=array_fail_commit=array_commits=0;
  ArrayParameterInputs input=array_input(); array_expected=input; array_source=&input;
  ArrayParameterRuntime runtime;
  size_t budget=sizeof(KuTaskInstance_0)+array_bytes(&input)-(mode==0u ? 1u : 0u);
  array_init(&runtime,budget);
  if (mode==1u) CHECK(ku_task_driver_shutdown(&runtime.driver,array_deadline())==KU_TASK_DRIVER_OK);
  if (mode==2u) array_fail_calloc=1u;
  FixtureLedger before=fixture_ledger(); KuTaskValueV1 value={0};
  CHECK(ku_task_0_start_value(&runtime.driver,&input.numbers,&input.flags,&input.texts,&value)==KU_TASK_DRIVER_OK);
  CHECK(!array_fail_calloc && !array_commits);
  CHECK(value.tag==KU_TASK_VALUE_INLINE_FAILED && value.result_kind==1u && !value.function_id
      && value.rejection_code==(mode==0u ? KU_TASK_DRIVER_LIMIT : mode==1u ? KU_TASK_DRIVER_CLOSED : KU_TASK_ADAPTER_OUT_OF_MEMORY));
  CHECK(!value.owner.lease.control && !value.ticket.driver);
  CHECK(!input.numbers.data && !input.numbers.len && !input.numbers.capacity);
  CHECK(!input.flags.data && !input.flags.len && !input.flags.capacity);
  CHECK(!input.texts.data && !input.texts.len && !input.texts.capacity);
  CHECK(fixture_ledger().calls==before.calls && !fixture_ledger().allocations && !fixture_ledger().bytes);
  KuTaskDriverSnapshotV1 snapshot=array_snapshot(&runtime);
  CHECK(!snapshot.resident && !snapshot.reserved_bytes && !snapshot.building && !snapshot.queued && !snapshot.running);
  CHECK(ku_task_value_drop(&value,array_deadline())==KU_TASK_DRIVER_OK);
  array_finish(&runtime);
}
int main(void) {
  for (unsigned mode=0;mode<3u;mode++) array_case(mode);
  for (unsigned mode=0;mode<3u;mode++) array_rejected_value(mode);
  puts("task-array-parameters-ok"); return 0;
}
"#;
