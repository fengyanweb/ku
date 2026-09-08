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
                TaskOp::ScopeEnter { tasks, .. } => {
                    for task in tasks {
                        map(task);
                    }
                }
                TaskOp::Init { dst, .. } => map(dst),
                TaskOp::Copy { dst, src }
                | TaskOp::Move { dst, src }
                | TaskOp::WrapOk { dst, src }
                | TaskOp::Unary { dst, src, .. } => {
                    map(dst);
                    map(src);
                }
                TaskOp::Binary {
                    dst, left, right, ..
                } => {
                    map(dst);
                    map(left);
                    map(right);
                }
                TaskOp::Read { slot } | TaskOp::Drop { slot } | TaskOp::DropIfInit { slot } => {
                    map(slot)
                }
                TaskOp::Start { dst, arguments, .. } => {
                    map(dst);
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
    let source = r#"
async fn Parent(value: str!, extra: str, count: int, flag: bool): str! {
    child = Child(value, extra, count, flag)
    return await child
}
async fn Child(value: str!, extra: str, count: int, flag: bool): str! { return value }
async fn main(): null! { return ok(null) }
"#;
    let ast = Parser::new(Lexer::new(source).lex().unwrap())
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
            TaskOp::Start { arguments, .. } => Some(arguments.clone()),
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
    let instrumentation = format!("{NATIVE_THREAD_LIFECYCLE_HARNESS}\n{LEDGER_LOCK}\n{ALLOCATION_HOOK}\n{LOCKED_ALLOCATIONS}\nstatic void fixture_record_free(void*);\nstatic void fixture_child_cleanup(void*);\nstatic void fixture_child_dispose(void*);\n#undef free\nstatic void fixture_counted_free(void* p) {{ fixture_record_free(p); fixture_free(p); }}\n#define free fixture_counted_free\n");
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
    generated.push_str(&C_MAIN.replace("@EXTRA@", &format!("s_{}", original_arguments[0][1].0)));
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
  CHECK(ku_task_driver_init(driver,sizeof(*driver),KU_TASK_DRIVER_ABI_VERSION,slots,2,ring,2,limit)==KU_TASK_DRIVER_OK);
  KuResult_str value={0}; value.ok=!error;
  if (error) value.error=(KuError){fixture_owned(7,"domain"),fixture_owned(11,"code"),fixture_owned(13,"message")};
  else value.value=fixture_owned(31,"payload");
  KuString extra=fixture_owned(17,"extra");
  KuResult_str original=value; /* Only inspected while its allocation is live. */
  uintptr_t extra_identity=(uintptr_t)extra.ptr;
  int64_t count=37; bool flag=true; KuTaskValueV1 root={0};
  CHECK(ku_task_0_start_value(driver,&value,&extra,&count,&flag,&root)==KU_TASK_DRIVER_OK);
  CHECK(root.tag==KU_TASK_VALUE_LIVE && fixture_empty_result(value) && fixture_empty_string(extra) && count==37 && flag);
  uint64_t deadline=ku_task_driver_now_ms()+1000u;
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
#if defined(_WIN32)
  CHECK(WaitForSingleObject(driver->thread,2000)==WAIT_OBJECT_0);
#else
  alarm(2);
#endif
  CHECK(ku_task_driver_destroy(driver)==KU_TASK_DRIVER_OK);
#if !defined(_WIN32)
  alarm(0);
#endif
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
