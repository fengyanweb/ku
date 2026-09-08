//! Real generated normal ScopeDrain/Exit adapter paths, not source scope syntax.
//! Gates postpone real callbacks; they never write phase, owner bits or receipts.
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
    ir::{self, task::*, IrType},
    lexer::Lexer,
    parser::Parser,
};
use native_allocation_harness::ALLOCATION_HOOK;
use native_harness::{
    compile_harness, run_bounded, TempDir, NATIVE_THREAD_LIFECYCLE_HARNESS, RUN_LIMITS, RUN_TIMEOUT,
};
use native_task_ledger::{LEDGER_LOCK, LOCKED_ALLOCATIONS};
use std::{fs, process::Command};

fn result(ty: IrType) -> IrType {
    IrType::Result(Box::new(ty))
}
fn value(ty: IrType) -> TaskSlot {
    TaskSlot {
        ty: TaskSlotType::Value {
            ty,
            borrowed: false,
        },
    }
}
fn task() -> TaskSlot {
    TaskSlot {
        ty: TaskSlotType::Task {
            result: result(IrType::Null),
        },
    }
}
fn state(operations: Vec<TaskOp>, terminator: TaskTerminator) -> TaskState {
    TaskState {
        operations,
        terminator,
    }
}
fn init(dst: usize, number: i64) -> TaskOp {
    TaskOp::Init {
        dst: SlotId(dst),
        value: TaskConstant::Int(number),
    }
}
fn start(dst: usize, argument: usize) -> TaskOp {
    TaskOp::Start {
        dst: SlotId(dst),
        function: TaskFunctionId(1),
        arguments: vec![SlotId(argument)],
    }
}
fn drain(scope: usize, ready: usize) -> TaskTerminator {
    TaskTerminator::ScopeDrain {
        scope: TaskScopeId(scope),
        ready: StateId(ready),
        cleanup: StateId(3),
    }
}
fn print(slot: usize) -> TaskOp {
    TaskOp::Print {
        value: SlotId(slot),
        newline: true,
    }
}
fn frames() -> TaskProgram {
    TaskProgram {
        functions: vec![
            TaskFunction {
                id: TaskFunctionId(0),
                name: "ScopeParent".into(),
                entry: StateId(0),
                parameters: vec![SlotId(0), SlotId(1)],
                result: result(IrType::Str),
                slots: vec![
                    value(result(IrType::Str)),
                    value(IrType::Str),
                    task(),
                    task(),
                    task(),
                    task(),
                    value(IrType::Str),
                    value(IrType::Int),
                    value(IrType::Int),
                    value(IrType::Int),
                    value(IrType::Int),
                    value(IrType::Int),
                ],
                states: vec![
                    state(
                        vec![
                            init(7, 101),
                            init(8, 0),
                            init(9, 1),
                            init(10, 2),
                            init(11, 3),
                            start(2, 8),
                            TaskOp::ScopeEnter {
                                scope: TaskScopeId(0),
                                tasks: vec![SlotId(3), SlotId(4)],
                            },
                            start(3, 9),
                            start(4, 10),
                        ],
                        drain(0, 1),
                    ),
                    state(
                        vec![
                            print(7),
                            init(7, 202),
                            TaskOp::Read { slot: SlotId(2) },
                            TaskOp::ScopeEnter {
                                scope: TaskScopeId(1),
                                tasks: vec![SlotId(5)],
                            },
                            start(5, 11),
                        ],
                        drain(1, 2),
                    ),
                    state(vec![print(7)], TaskTerminator::Exit { value: SlotId(0) }),
                    state(
                        vec![
                            TaskOp::Init {
                                dst: SlotId(6),
                                value: TaskConstant::Str("scope-cleanup".into()),
                            },
                            print(6),
                            TaskOp::DropIfInit { slot: SlotId(6) },
                            TaskOp::DropIfInit { slot: SlotId(1) },
                            TaskOp::DropIfInit { slot: SlotId(0) },
                        ],
                        TaskTerminator::Terminate,
                    ),
                ],
            },
            TaskFunction {
                id: TaskFunctionId(1),
                name: "ScopeChild".into(),
                entry: StateId(0),
                parameters: vec![SlotId(0)],
                result: result(IrType::Null),
                slots: vec![value(IrType::Int), value(result(IrType::Null))],
                states: vec![state(
                    vec![TaskOp::Init {
                        dst: SlotId(1),
                        value: TaskConstant::Ok(Box::new(TaskConstant::Null)),
                    }],
                    TaskTerminator::Complete { value: SlotId(1) },
                )],
            },
        ],
    }
}
fn replace_once(source: String, anchor: &str, replacement: &str) -> String {
    assert_eq!(
        source.matches(anchor).count(),
        1,
        "scope adapter hook: {anchor}"
    );
    source.replacen(anchor, replacement, 1)
}

#[test]
fn native_task_scope_adapter_normal_sessions_and_timeout_final_handoff_in_c() {
    let ast = Parser::new(
        Lexer::new("fn main(): null! { return ok(null) }")
            .lex()
            .unwrap(),
    )
    .parse_program()
    .unwrap();
    Checker::new().check(&ast).unwrap();
    let sync = ir::lower_program(&ast).unwrap();
    let tasks = frames();
    let plan = verify_and_plan(&tasks, TaskLimits::default()).unwrap();
    assert_eq!(
        plan.functions[0].scopes,
        vec![
            TaskScopeFrame {
                scope: TaskScopeId(0),
                task_mask: 24
            },
            TaskScopeFrame {
                scope: TaskScopeId(1),
                task_mask: 32
            },
        ]
    );
    assert_eq!(plan.functions[0].scope_task_mask, 60);
    assert!(plan.functions[0].slots.contains(&SlotId(7)));
    assert!(plan.functions[0].slots.contains(&SlotId(11)));
    let mut generated = c::generate_task_frame_c_source(&sync, &tasks).unwrap();
    for required in [
        "#define KU_TASK_FRAME_ABI_VERSION 4u",
        "#define KU_TASK_DRIVER_ABI_VERSION 6u",
        "KU_TASK_FRAME_SCOPE_REQUEST = 12u",
    ] {
        assert!(generated.contains(required));
    }
    for forbidden in ["run_source", "const SOURCE"] {
        assert!(!generated.contains(forbidden));
    }
    let ledger = replace_once(
        LOCKED_ALLOCATIONS.to_owned(),
        "ku_perf_free(p);",
        "fixture_record_free(p); ku_perf_free(p);",
    );
    let instrumentation = format!(
        "{NATIVE_THREAD_LIFECYCLE_HARNESS}\n{LEDGER_LOCK}\n{ALLOCATION_HOOK}\nstatic void fixture_record_free(void*);\n{ledger}\nstatic int fixture_parent_hold(void*);\nstatic void fixture_parent_drive(void*,int);\nstatic void fixture_frame_return(void*,uint32_t);\nstatic void fixture_continued(void*,uint64_t);\nstatic uint32_t fixture_child_hold(void*,int);\n"
    );
    generated = replace_once(
        generated,
        "typedef struct KuString {",
        &format!("{instrumentation}typedef struct KuString {{"),
    );
    generated = replace_once(
        generated,
        "static uint64_t ku_task_driver_now_ms(void) {",
        &format!("{CLOCK_HOOK}\nstatic uint64_t fixture_original_now(void) {{"),
    );
    let entry = "static uint32_t ku_task_0_resume(void* raw) {";
    generated = replace_once(
        generated,
        entry,
        &format!("{entry}\n  if (fixture_parent_hold(raw)) return KU_TASK_CONTROL_PENDING;"),
    );
    let drive = "static uint32_t ku_task_frame_0_drive(KuTaskFrame_0* frame, const KuTaskFrameClockV1* clock, int cleanup) {";
    generated = replace_once(
        generated,
        drive,
        &format!("{drive}\n  fixture_parent_drive(frame,cleanup);"),
    );
    let resume = "status = ku_task_frame_0_resume(&instance->frame, sizeof(instance->frame), KU_TASK_FRAME_ABI_VERSION, &clock);";
    generated = replace_once(
        generated,
        resume,
        &format!("{resume}\n    fixture_frame_return(instance,status);"),
    );
    let continued = "status=ku_task_frame_0_scope_continue(&instance->frame,sizeof(instance->frame),KU_TASK_FRAME_ABI_VERSION,descriptor->scope_id);\n  if (status!=KU_TASK_FRAME_OK) return status;";
    generated = replace_once(
        generated,
        continued,
        &format!("{continued}\n  fixture_continued(instance,descriptor->scope_id);"),
    );
    for (entry, cleanup) in [
        ("static uint32_t ku_task_1_resume(void* raw) {", 0),
        ("static uint32_t ku_task_1_cleanup(void* raw, uint32_t reason, KuTaskControlV1* budget) {", 1),
    ] {
        generated = replace_once(generated, entry, &format!("{entry}\n  uint32_t held=fixture_child_hold(raw,{cleanup});\n  if (held!=UINT32_MAX) return held;"));
    }
    generated = replace_once(generated,
        "static uint32_t ku_task_driver_owner_drop_receipt(\n",
        "static uint32_t ku_task_driver_owner_drop_receipt(const KuTaskDriverTicketV1*,KuTaskControlOwnerV1*,uint64_t,KuTaskDriverCleanupReceiptV1*);\nstatic uint32_t fixture_real_owner_drop_receipt(\n");
    generated = replace_once(
        generated,
        "int main(void) {",
        "static int fixture_original_main(void) {",
    );
    generated.push_str(C_MAIN);
    let directory = TempDir::new("native-task-scope-adapter");
    let path = directory.path().join("program.c");
    fs::write(&path, generated).unwrap();
    let Some(executable) = compile_harness(directory.path(), &path, "program") else {
        assert!(
            std::env::var_os("GITHUB_ACTIONS").is_none(),
            "CI must execute the generated scope adapter"
        );
        return;
    };
    fs::remove_file(path).unwrap();
    let output = run_bounded(
        Command::new(executable).current_dir(directory.path()),
        RUN_TIMEOUT,
        RUN_LIMITS,
    )
    .expect("scope adapter must stop under the existing real process watchdog");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8(output.stdout).unwrap().replace('\r', ""),
        "101\n202\nscope-cleanup\ntask-scope-adapter-ok\n"
    );
    assert!(output.stderr.is_empty());
}

const CLOCK_HOOK: &str = r#"
static KuTaskControlDeadlineV1 fixture_clock;
static uint64_t fixture_original_now(void);
static uint64_t ku_task_driver_now_ms(void) {
  uint64_t now=ku_task_control_deadline_load(&fixture_clock);
  return now ? now : fixture_original_now();
}
"#;

const C_MAIN: &str = r#"
/* Virtual time selects deterministic deadline interleavings, not an OS timer
 * timing claim. Event/OS joins retain 2s limits; virtual-clock driver waits
 * additionally rely on the unchanged real 20s process watchdog. */
static unsigned fixture_mode, fixture_transferred, fixture_scope_seen, fixture_staged;
static unsigned fixture_drives[3], fixture_closed[2], fixture_promoted;
static unsigned fixture_cleanup_drives;
static uint64_t fixture_cancel_deadline;
static uintptr_t fixture_ids[2];
static unsigned fixture_frees[2];
static KuAtomicRefcount fixture_release_children, fixture_release_continuations;
static KuTestEvent fixture_child_events[3], fixture_close_events[2];
static KuTaskInstance_0* fixture_parent;
static KuTaskDriverTicketV1 fixture_children[4];
static KuTaskControlLeaseV1 fixture_observers[4];
static KuTaskDriverCleanupReceiptV1 fixture_receipts[4];
static uint64_t fixture_child_deadlines[4], fixture_scope_deadlines[2];
static KuTaskDriverScopeTokenV1 fixture_first_token;
static const KuTaskDriverCleanupReceiptV1* fixture_first_span;

static int fixture_empty_string(KuString value) {
  return !value.ptr && !value.len && !value.capacity && !value.storage;
}
static int fixture_empty_result(KuResult_str value) {
  return !value.ok && fixture_empty_string(value.value)
      && fixture_empty_string(value.error.domain) && fixture_empty_string(value.error.code)
      && fixture_empty_string(value.error.message);
}
static int fixture_same_receipt(const KuTaskDriverCleanupReceiptV1* a,const KuTaskDriverCleanupReceiptV1* b) {
  return a->driver==b->driver && a->slot==b->slot && a->generation==b->generation && a->abi_version==b->abi_version;
}
static void fixture_record_free(void* pointer) {
  if (!pointer) return;
  for (size_t i=0;i<2u;i++) if (fixture_ids[i] && fixture_ids[i]==(uintptr_t)pointer) {
    CHECK(!fixture_frees[i]++);
    CHECK(fixture_transferred==(fixture_mode ? 7u : 15u));
    if (fixture_mode) CHECK(fixture_promoted==1u);
    else CHECK(fixture_staged==1u && fixture_closed[0]==1u && fixture_closed[1]==1u);
  }
}
static KuString fixture_owned(unsigned id,size_t capacity,const char* text) {
  size_t length=strlen(text); CHECK(id<2u && !fixture_ids[id] && length<=capacity);
  uint8_t* pointer=(uint8_t*)malloc(capacity); CHECK(pointer);
  memcpy(pointer,text,length); fixture_ids[id]=(uintptr_t)pointer;
  KuString result={pointer,length,capacity,KU_STRING_OWNED}; return result;
}
static unsigned fixture_register_child(KuTaskInstance_1* child) {
  int64_t raw=child->frame.s_0; CHECK(raw>=0 && raw<4);
  unsigned index=(unsigned)raw;
  if (!fixture_children[index].driver) {
    fixture_children[index]=child->ticket;
    KuTaskDriverV1* driver=child->ticket.driver;
    CHECK(!ku_task_driver_lock(driver));
    KuTaskDriverSlotV1* slot=ku_task_driver_find(&child->ticket);
    CHECK(slot && slot->binding==&child->control && slot->driver_lease.control==&child->control);
    CHECK(ku_task_control_lease_retain(&slot->driver_lease,&fixture_observers[index])==KU_TASK_CONTROL_OK);
    CHECK(!ku_task_driver_unlock(driver));
  }
  return index;
}
static int fixture_parent_hold(void* raw) {
  KuTaskInstance_0* parent=(KuTaskInstance_0*)raw;
  if (!fixture_parent) fixture_parent=parent;
  CHECK(fixture_parent==parent);
  uint32_t state=parent->frame.header.state;
  if (parent->frame.header.status==KU_TASK_FRAME_PENDING && state>=1u && state<=2u
      && fixture_closed[state-1u] && !(ku_task_control_atomic_load(&fixture_release_continuations)&((size_t)1u<<state))) {
    CHECK(ku_task_driver_set_intent(&parent->ticket,KU_TASK_DRIVER_WAIT)==KU_TASK_DRIVER_OK);
    return 1;
  }
  return 0;
}
static void fixture_parent_drive(void* raw,int cleanup) {
  KuTaskFrame_0* frame=(KuTaskFrame_0*)raw;
  if (cleanup) {
    CHECK(fixture_mode==2u && frame->header.state==3u && !fixture_cleanup_drives++);
    CHECK(!(frame->header.initialized&UINT64_C(60)));
    CHECK(fixture_transferred==7u && fixture_promoted==1u);
    return;
  }
  CHECK(!cleanup && frame->header.state<3u);
  CHECK(!fixture_drives[frame->header.state]++);
  if (frame->header.state) CHECK(fixture_closed[frame->header.state-1u]==1u);
}
static void fixture_frame_return(void* raw,uint32_t status) {
  KuTaskInstance_0* parent=(KuTaskInstance_0*)raw;
  if (status==KU_TASK_FRAME_SCOPE_REQUEST) {
    CHECK(parent->frame.header.state<2u && !parent->frame.header.result_initialized);
    fixture_scope_seen |= 1u<<parent->frame.header.state;
    CHECK(!fixture_frees[0] && !fixture_frees[1]);
  } else if (status==KU_TASK_FRAME_EXIT_STAGED) {
    CHECK(!fixture_mode && !fixture_staged++ && fixture_closed[0] && fixture_closed[1]);
    CHECK(fixture_transferred==14u && (parent->frame.header.initialized&UINT64_C(4)));
    CHECK(parent->frame.header.result_initialized && fixture_empty_result(parent->frame.s_0));
    CHECK(parent->frame.result.ok && (uintptr_t)parent->frame.result.value.ptr==fixture_ids[0]);
    CHECK(!fixture_frees[0] && !fixture_frees[1]);
  }
}
static void fixture_continued(void* raw,uint64_t scope) {
  KuTaskInstance_0* parent=(KuTaskInstance_0*)raw;
  CHECK(!fixture_mode && scope<2u && !fixture_closed[scope]++);
  CHECK(parent->frame.header.status==KU_TASK_FRAME_PENDING && parent->frame.header.state==scope+1u);
  CHECK(!fixture_drives[scope+1u]); /* No ready operations in this poll. */
  CHECK(parent->scope_mode==1u && !parent->scope_token.driver);
  CHECK(parent->scope_issued_mask==parent->scope_expected_mask && !parent->receipt_mask && !parent->wait.driver);
  CHECK(parent->frame.s_2.tag==KU_TASK_VALUE_LIVE && parent->frame.s_2.owner.lease.control);
  CHECK(ku_task_control_atomic_load(&fixture_observers[0].control->phase)==KU_TASK_CONTROL_LIVE);
  CHECK(!fixture_frees[0] && !fixture_frees[1]);
  KuTaskDriverV1* driver=parent->ticket.driver;
  CHECK(!ku_task_driver_lock(driver));
  KuTaskDriverSlotV1* slot=ku_task_driver_find(&parent->ticket);
  CHECK(slot && !slot->scope_session_active && !slot->scope_wait_started && !slot->scope_receipts);
  CHECK(slot->scope_wait_deadline==UINT64_MAX);
  CHECK(!ku_task_driver_unlock(driver));
  CHECK(ku_test_event_set(&fixture_close_events[scope]));
}
static uint32_t fixture_child_hold(void* raw,int cleanup) {
  KuTaskInstance_1* child=(KuTaskInstance_1*)raw;
  unsigned index=fixture_register_child(child);
  if (cleanup && (ku_task_control_atomic_load(&fixture_release_children)&((size_t)1u<<index))) {
    CHECK(ku_task_control_cleanup_deadline(&child->control)<=fixture_child_deadlines[index]);
    return UINT32_MAX;
  }
  CHECK(ku_task_driver_set_intent(&child->ticket,KU_TASK_DRIVER_WAIT)==KU_TASK_DRIVER_OK);
  if (cleanup && index!=1u) {
    unsigned event=index==2u ? 0u : index==3u ? 1u : 2u;
    CHECK(ku_test_event_set(&fixture_child_events[event]));
  }
  return KU_TASK_CONTROL_PENDING;
}
static uint32_t ku_task_driver_owner_drop_receipt(const KuTaskDriverTicketV1* ticket,
    KuTaskControlOwnerV1* owner,uint64_t deadline,KuTaskDriverCleanupReceiptV1* receipt) {
  KuTaskInstance_1* child=(KuTaskInstance_1*)owner->lease.control;
  unsigned index=fixture_register_child(child);
  KuTaskInstance_0* parent=fixture_parent;
  CHECK(parent && !fixture_receipts[index].driver && receipt==&parent->receipts[index]);
  CHECK(ticket->driver==fixture_children[index].driver && ticket->slot==fixture_children[index].slot
      && ticket->generation==fixture_children[index].generation);
  if (index==1u || index==2u) {
    CHECK(parent->scope_mode==1u && parent->scope_expected_mask==6u);
    CHECK(parent->scope_token.scope_id==0u && parent->scope_token.driver==ticket->driver);
    if (!fixture_first_token.driver) {
      fixture_first_token=parent->scope_token; fixture_first_span=parent->receipts;
      fixture_scope_deadlines[0]=deadline;
    }
    CHECK(deadline==fixture_scope_deadlines[0]);
  } else if (index==3u) {
    CHECK(!fixture_mode && parent->scope_mode==1u && parent->scope_expected_mask==8u);
    CHECK(parent->scope_token.scope_id==1u && parent->scope_token.epoch>fixture_first_token.epoch);
    fixture_scope_deadlines[1]=deadline;
    CHECK(deadline>fixture_scope_deadlines[0]);
  } else if (fixture_mode) {
    CHECK(parent->scope_mode==2u && parent->scope_expected_mask==7u && !fixture_promoted++);
    CHECK(parent->scope_token.driver==fixture_first_token.driver && parent->scope_token.scope_id==0u
        && parent->scope_token.epoch==fixture_first_token.epoch
        && parent->scope_token.parent_generation==fixture_first_token.parent_generation);
    CHECK(parent->receipts==fixture_first_span && parent->scope_issued_mask==6u);
    CHECK(fixture_same_receipt(&parent->receipts[1],&fixture_receipts[1]));
    CHECK(fixture_same_receipt(&parent->receipts[2],&fixture_receipts[2]));
    if (fixture_mode==1u) {
      CHECK(deadline==fixture_scope_deadlines[0]);
      CHECK(parent->frame.header.status==KU_TASK_FRAME_EXIT_STAGED && parent->frame.header.result_initialized);
      CHECK(parent->frame.header.exit_class==KU_TASK_EXIT_RUNTIME_FAILURE);
      CHECK(parent->frame.header.has_exit_deadline && parent->frame.header.exit_deadline==deadline);
    } else {
      CHECK(fixture_mode==2u && deadline==fixture_cancel_deadline);
      CHECK(parent->frame.header.status==KU_TASK_FRAME_SCOPE_REQUEST
          && parent->frame.header.cleanup_state==3u && !parent->frame.header.result_initialized);
      CHECK(ku_task_control_atomic_load(&parent->control.phase)==KU_TASK_CONTROL_REQUESTED_CANCEL);
    }
    CHECK(!fixture_frees[0] && !fixture_frees[1]);
  } else {
    CHECK(!parent->scope_mode && fixture_staged && fixture_closed[0] && fixture_closed[1]);
    CHECK(deadline>fixture_scope_deadlines[1]);
  }
  if (parent->scope_mode) {
    CHECK(!ku_task_driver_lock(ticket->driver));
    KuTaskDriverSlotV1* slot=ku_task_driver_find(&parent->ticket);
    CHECK(slot && slot->scope_receipts==parent->receipts
        && slot->scope_expected_mask==parent->scope_expected_mask
        && (slot->scope_expected_mask&(UINT64_C(1)<<index)));
    CHECK(slot->scope_session_final==(parent->scope_mode==2u));
    if (fixture_mode && index==0u) CHECK(ku_task_driver_cleanup_receipt_status_locked(ticket->driver,
        fixture_receipts[1].slot,fixture_receipts[1].generation)==KU_TASK_DRIVER_CLEANUP_ACK);
    CHECK(!ku_task_driver_unlock(ticket->driver));
  }
  uint32_t status=fixture_real_owner_drop_receipt(ticket,owner,deadline,receipt);
  CHECK(status==KU_TASK_DRIVER_OK && !owner->lease.control);
  CHECK(receipt->driver==ticket->driver && receipt->slot==ticket->slot && receipt->generation==ticket->generation);
  fixture_receipts[index]=*receipt; fixture_child_deadlines[index]=deadline;
  fixture_alloc_lock(); CHECK(!(fixture_transferred&(1u<<index))); fixture_transferred|=1u<<index; fixture_alloc_unlock();
  return status;
}
static KuTaskDriverSnapshotV1 fixture_idle(KuTaskDriverV1* driver) {
  CHECK(ku_task_driver_wait_idle(driver,ku_task_driver_now_ms()+2000u)==KU_TASK_DRIVER_OK);
  KuTaskDriverSnapshotV1 snapshot={0};
  CHECK(ku_task_driver_snapshot(driver,&snapshot)==KU_TASK_DRIVER_OK);
  CHECK(!snapshot.fault && !snapshot.running && !snapshot.queued && !snapshot.building);
  CHECK(snapshot.worker_waiting || snapshot.worker_exited);
  CHECK(!ku_task_driver_lock(driver));
  size_t bytes=0;
  for (size_t i=0;i<driver->capacity;i++) {
    CHECK(driver->slots[i].state!=KU_TASK_DRIVER_FAULTED);
    CHECK(driver->slots[i].charged_bytes<=SIZE_MAX-bytes); bytes+=driver->slots[i].charged_bytes;
  }
  CHECK(bytes==driver->reserved_bytes && !driver->clock_fault);
  CHECK(!ku_task_driver_unlock(driver));
  return snapshot;
}
static void fixture_release(unsigned index) {
  CHECK(index<4u && fixture_children[index].driver);
  ku_task_control_atomic_store(&fixture_release_children,
      ku_task_control_atomic_load(&fixture_release_children)|((size_t)1u<<index));
  CHECK(ku_task_driver_wake(&fixture_children[index])==KU_TASK_DRIVER_OK);
}
static void fixture_pending(KuTaskValueV1* root) {
  KuTaskAdapterOutcomeV1 outcome={0};
  CHECK(ku_task_value_take(root,NULL,&outcome)==KU_TASK_CONTROL_PENDING);
  CHECK(ku_task_outcome_empty(&outcome,4u));
  fixture_idle(root->ticket.driver);
}
static void fixture_timeout_payload(const KuTaskAdapterOutcomeV1* outcome,uint64_t deadline) {
  CHECK(outcome->result_kind==4u && outcome->exit_class==KU_TASK_EXIT_RUNTIME_FAILURE);
  CHECK(outcome->has_cleanup_deadline && outcome->cleanup_deadline==deadline);
  CHECK(!outcome->value.string.ok && fixture_empty_string(outcome->value.string.value));
  KuError error=outcome->value.string.error;
  CHECK(error.domain.len==4u && !memcmp(error.domain.ptr,"task",4u));
  CHECK(error.code.len==16u && !memcmp(error.code.ptr,"shutdown_timeout",16u));
}
static void fixture_case(unsigned mode) {
  CHECK(!fixture_ledger().allocations && !fixture_ledger().bytes);
  fixture_mode=mode; fixture_parent=NULL; fixture_transferred=fixture_scope_seen=fixture_staged=fixture_promoted=0;
  fixture_cleanup_drives=0; fixture_cancel_deadline=0;
  memset(fixture_drives,0,sizeof(fixture_drives)); memset(fixture_closed,0,sizeof(fixture_closed));
  memset(fixture_ids,0,sizeof(fixture_ids)); memset(fixture_frees,0,sizeof(fixture_frees));
  memset(fixture_children,0,sizeof(fixture_children)); memset(fixture_observers,0,sizeof(fixture_observers));
  memset(fixture_receipts,0,sizeof(fixture_receipts)); memset(fixture_child_deadlines,0,sizeof(fixture_child_deadlines));
  memset(fixture_scope_deadlines,0,sizeof(fixture_scope_deadlines)); fixture_first_token=(KuTaskDriverScopeTokenV1){0}; fixture_first_span=NULL;
  ku_task_control_atomic_store(&fixture_release_children,2u); /* First inner child may really ACK. */
  ku_task_control_atomic_store(&fixture_release_continuations,0);
  for (size_t i=0;i<3u;i++) CHECK(ku_test_event_init(&fixture_child_events[i]));
  for (size_t i=0;i<2u;i++) CHECK(ku_test_event_init(&fixture_close_events[i]));
  uint64_t base=ku_test_real_now_ms()+100000u;
  ku_task_control_deadline_store(&fixture_clock,base);
  KuTaskDriverV1* driver=(KuTaskDriverV1*)calloc(1,sizeof(*driver));
  KuTaskDriverSlotV1* slots=(KuTaskDriverSlotV1*)calloc(5,sizeof(*slots));
  size_t* ring=(size_t*)calloc(5,sizeof(*ring)); CHECK(driver && slots && ring);
  size_t fixed=sizeof(*driver)+5u*(sizeof(*slots)+sizeof(*ring));
  CHECK(ku_task_driver_init(driver,sizeof(*driver),KU_TASK_DRIVER_ABI_VERSION,slots,5,ring,5,
      fixed+sizeof(KuTaskInstance_0)+4u*sizeof(KuTaskInstance_1)+256u)==KU_TASK_DRIVER_OK);
  KuResult_str returned={0}; returned.ok=true; returned.value=fixture_owned(0u,19u,"payload");
  KuString held=fixture_owned(1u,17u,"remaining");
  KuTaskValueV1 root={0};
  CHECK(ku_task_0_start_value(driver,&returned,&held,&root)==KU_TASK_DRIVER_OK && root.tag==KU_TASK_VALUE_LIVE);
  CHECK(fixture_empty_result(returned) && fixture_empty_string(held));
  CHECK(ku_test_event_wait(&fixture_child_events[0],2000)); fixture_idle(driver);
  KuTaskControlV1* control=root.owner.lease.control;
  CHECK(fixture_parent==(KuTaskInstance_0*)control && fixture_scope_seen==1u && fixture_transferred==6u);
  CHECK(fixture_scope_deadlines[0]==base+1000u && fixture_first_span==fixture_parent->receipts);
  CHECK(ku_task_driver_cleanup_receipt_read(&fixture_receipts[1])==KU_TASK_DRIVER_CLEANUP_ACK);
  CHECK(ku_task_driver_cleanup_receipt_read(&fixture_receipts[2])==KU_TASK_DRIVER_PENDING);
  CHECK(fixture_parent->scope_expected_mask==6u && fixture_parent->scope_issued_mask==6u && fixture_parent->receipt_mask==4u);
  CHECK(fixture_parent->frame.s_2.tag==KU_TASK_VALUE_LIVE && fixture_parent->frame.s_2.owner.lease.control);
  CHECK(ku_task_control_atomic_load(&fixture_observers[0].control->phase)==KU_TASK_CONTROL_LIVE);
  CHECK(fixture_drives[0]==1u && !fixture_drives[1] && !fixture_closed[0]);
  fixture_pending(&root);
  for (unsigned repeat=0;repeat<2u;repeat++) {
    CHECK(ku_task_driver_wake(&root.ticket)==KU_TASK_DRIVER_OK); fixture_idle(driver);
    CHECK(fixture_drives[0]==1u && !fixture_drives[1] && fixture_transferred==6u && !fixture_closed[0]);
  }
  KuTaskAdapterOutcomeV1 outcome={0};
  if (!mode) {
    fixture_release(2u);
    CHECK(ku_test_event_wait(&fixture_close_events[0],2000)); fixture_idle(driver);
    CHECK(fixture_closed[0]==1u && !fixture_drives[1] && !fixture_parent->scope_mode && !fixture_parent->drain_started);
    CHECK(!fixture_parent->scope_token.driver && !fixture_parent->receipt_mask);
    for (size_t i=0;i<4u;i++) CHECK(ku_task_driver_scope_receipt_empty(&fixture_parent->receipts[i]));
    ku_task_control_deadline_store(&fixture_clock,base+200u);
    ku_task_control_atomic_store(&fixture_release_continuations,2u);
    CHECK(ku_task_driver_wake(&root.ticket)==KU_TASK_DRIVER_OK);
    CHECK(ku_test_event_wait(&fixture_child_events[1],2000)); fixture_idle(driver);
    CHECK(fixture_scope_seen==3u && fixture_drives[1]==1u && !fixture_drives[2]);
    CHECK(fixture_scope_deadlines[1]==base+1200u && fixture_transferred==14u);
    CHECK(ku_task_control_atomic_load(&fixture_observers[0].control->phase)==KU_TASK_CONTROL_LIVE);
    CHECK(!fixture_frees[0] && !fixture_frees[1]);
    fixture_pending(&root);
    fixture_release(3u);
    CHECK(ku_test_event_wait(&fixture_close_events[1],2000)); fixture_idle(driver);
    CHECK(fixture_closed[1]==1u && !fixture_drives[2] && !fixture_parent->drain_started);
    ku_task_control_deadline_store(&fixture_clock,base+400u);
    ku_task_control_atomic_store(&fixture_release_continuations,6u);
    CHECK(ku_task_driver_wake(&root.ticket)==KU_TASK_DRIVER_OK);
    CHECK(ku_test_event_wait(&fixture_child_events[2],2000)); fixture_idle(driver);
    CHECK(fixture_staged==1u && fixture_drives[2]==1u && fixture_transferred==15u);
    CHECK(fixture_child_deadlines[0]==base+1400u && !fixture_frees[0] && fixture_frees[1]==1u);
    CHECK(fixture_parent->payload_initialized && fixture_parent->frame.header.status==KU_TASK_FRAME_READY);
    CHECK(ku_task_control_atomic_load(&control->phase)==KU_TASK_CONTROL_LIVE);
    fixture_pending(&root); /* Private Result cannot publish before outer ACK. */
    fixture_release(0u); fixture_idle(driver);
    CHECK(ku_task_control_atomic_load(&control->phase)==KU_TASK_CONTROL_COMPLETED);
    CHECK(ku_task_value_take(&root,NULL,&outcome)==KU_TASK_CONTROL_OK);
    CHECK(outcome.result_kind==4u && outcome.exit_class==KU_TASK_EXIT_USER_RESULT
        && !outcome.has_cleanup_deadline && !outcome.cleanup_deadline);
    CHECK(outcome.value.string.ok && (uintptr_t)outcome.value.string.value.ptr==fixture_ids[0]);
  } else if (mode==1u) {
    ku_task_control_deadline_store(&fixture_clock,fixture_scope_deadlines[0]);
    CHECK(ku_task_driver_wake(&root.ticket)==KU_TASK_DRIVER_OK);
    CHECK(ku_test_event_wait(&fixture_child_events[2],2000)); fixture_idle(driver);
    CHECK(fixture_promoted==1u && fixture_transferred==7u && !fixture_closed[0] && !fixture_closed[1]);
    CHECK(fixture_drives[0]==1u && !fixture_drives[1] && !fixture_drives[2] && !fixture_staged);
    CHECK(fixture_frees[0]==1u && fixture_frees[1]==1u && !fixture_children[3].driver);
    CHECK(ku_task_control_atomic_load(&control->phase)==KU_TASK_CONTROL_FAILED);
    CHECK(ku_task_driver_cleanup_receipt_read(&fixture_receipts[2])==KU_TASK_DRIVER_PENDING);
    CHECK(ku_task_driver_cleanup_receipt_read(&fixture_receipts[0])==KU_TASK_DRIVER_PENDING);
    CHECK(ku_task_value_take(&root,NULL,&outcome)==KU_TASK_CONTROL_OK);
    fixture_timeout_payload(&outcome,fixture_scope_deadlines[0]);
    fixture_idle(driver);
    fixture_release(2u); fixture_release(0u); fixture_idle(driver);
    CHECK(ku_task_driver_cleanup_receipt_read(&fixture_receipts[2])==KU_TASK_DRIVER_CLEANUP_ACK);
    CHECK(ku_task_driver_cleanup_receipt_read(&fixture_receipts[0])==KU_TASK_DRIVER_CLEANUP_ACK);
    CHECK(ku_task_control_atomic_load(&control->phase)==KU_TASK_CONTROL_FAILED);
    fixture_timeout_payload(&outcome,fixture_scope_deadlines[0]);
    CHECK(!fixture_drives[1] && !fixture_drives[2] && !fixture_closed[0]);
  } else {
    CHECK(mode==2u);
    KuTaskDriverWaitTokenV1 original_wait=fixture_parent->wait;
    CHECK(original_wait.driver && fixture_parent->scope_mode==1u);
    fixture_cancel_deadline=base+700u;
    CHECK(ku_task_driver_request_cancel(&root.ticket,&root.owner.lease,
        KU_TASK_CONTROL_CANCELLED,fixture_cancel_deadline)==KU_TASK_CONTROL_OK);
    /* This real outer-child cleanup event follows actual FINAL registration,
     * owner transfer and the parent's original cancellation CFG. */
    CHECK(ku_test_event_wait(&fixture_child_events[2],2000)); fixture_idle(driver);
    CHECK(fixture_transferred==7u && fixture_promoted==1u && fixture_cleanup_drives==1u);
    CHECK(fixture_frees[0]==1u && fixture_frees[1]==1u && !fixture_staged);
    CHECK(fixture_parent->scope_mode==2u && fixture_parent->scope_expected_mask==7u
        && fixture_parent->scope_issued_mask==7u);
    CHECK(fixture_parent->scope_token.epoch==fixture_first_token.epoch
        && fixture_parent->scope_token.parent_generation==fixture_first_token.parent_generation
        && fixture_parent->receipts==fixture_first_span);
    CHECK(fixture_parent->wait.driver==original_wait.driver
        && fixture_parent->wait.parent_generation==original_wait.parent_generation
        && fixture_parent->wait.epoch==original_wait.epoch);
    CHECK(fixture_same_receipt(&fixture_parent->receipts[1],&fixture_receipts[1])
        && fixture_same_receipt(&fixture_parent->receipts[2],&fixture_receipts[2]));
    CHECK(ku_task_driver_cleanup_receipt_read(&fixture_receipts[1])==KU_TASK_DRIVER_CLEANUP_ACK);
    CHECK(ku_task_driver_cleanup_receipt_read(&fixture_receipts[2])==KU_TASK_DRIVER_PENDING);
    CHECK(ku_task_driver_cleanup_receipt_read(&fixture_receipts[0])==KU_TASK_DRIVER_PENDING);
    CHECK(ku_task_control_atomic_load(&control->phase)==KU_TASK_CONTROL_REQUESTED_CANCEL);
    for (size_t i=0;i<3u;i+=2u)
      CHECK(ku_task_control_cleanup_deadline(fixture_observers[i].control)==fixture_cancel_deadline);
    fixture_pending(&root);
    /* The opposite reason may shorten D, but cannot replace the first winner.
     * There is no virtual clock advance, fake phase, receipt or ACK write. */
    fixture_cancel_deadline=base+500u;
    CHECK(ku_task_driver_request_cancel(&root.ticket,&root.owner.lease,
        KU_TASK_CONTROL_TIMED_OUT,fixture_cancel_deadline)==KU_TASK_CONTROL_OK);
    fixture_idle(driver);
    CHECK(ku_task_control_atomic_load(&control->phase)==KU_TASK_CONTROL_REQUESTED_CANCEL
        && ku_task_control_cleanup_deadline(control)==fixture_cancel_deadline);
    CHECK(fixture_parent->drain_deadline==fixture_cancel_deadline);
    for (size_t i=0;i<3u;i+=2u) {
      CHECK(ku_task_control_atomic_load(&fixture_observers[i].control->phase)==KU_TASK_CONTROL_REQUESTED_CANCEL);
      CHECK(ku_task_control_cleanup_deadline(fixture_observers[i].control)==fixture_cancel_deadline);
    }
    CHECK(!ku_task_driver_lock(driver));
    KuTaskDriverSlotV1* active=ku_task_driver_find(&root.ticket);
    CHECK(active && active->scope_session_final && active->scope_expected_mask==7u
        && active->scope_receipts==fixture_first_span && active->scope_wait_deadline==fixture_cancel_deadline);
    CHECK(!ku_task_driver_unlock(driver));
    CHECK(fixture_cleanup_drives==1u && !fixture_drives[1] && !fixture_drives[2]);
    fixture_pending(&root);
    /* Real child callbacks finally execute and publish real durable ACKs. */
    fixture_release(2u); fixture_release(0u); fixture_idle(driver);
    CHECK(ku_task_driver_cleanup_receipt_read(&fixture_receipts[2])==KU_TASK_DRIVER_CLEANUP_ACK
        && ku_task_driver_cleanup_receipt_read(&fixture_receipts[0])==KU_TASK_DRIVER_CLEANUP_ACK);
    CHECK(ku_task_control_atomic_load(&control->phase)==KU_TASK_CONTROL_CANCELLED
        && ku_task_control_cleanup_deadline(control)==fixture_cancel_deadline);
    CHECK(ku_task_value_take(&root,NULL,&outcome)==KU_TASK_CONTROL_CANCELLED
        && ku_task_outcome_empty(&outcome,4u));
    CHECK(ku_task_driver_wake(&root.ticket)==KU_TASK_DRIVER_OK); fixture_idle(driver);
    CHECK(ku_task_control_atomic_load(&control->phase)==KU_TASK_CONTROL_CANCELLED);
    CHECK(fixture_cleanup_drives==1u && !fixture_drives[1] && !fixture_drives[2]
        && !fixture_closed[0] && !fixture_closed[1] && !fixture_children[3].driver);
  }
  fixture_idle(driver);
  CHECK(control->frame_destroyed && !fixture_parent->frame_initialized);
  CHECK(fixture_parent->frame.header.status==KU_TASK_FRAME_DESTROYED && !fixture_parent->frame.header.initialized);
  CHECK(!fixture_parent->frame.header.result_initialized && !fixture_parent->payload_initialized);
  CHECK(fixture_empty_result(fixture_parent->payload) && fixture_empty_result(fixture_parent->frame.result));
  ku_task_outcome_drop(&outcome); CHECK(ku_task_outcome_empty(&outcome,4u));
  CHECK(fixture_frees[0]==1u && fixture_frees[1]==1u);
  for (size_t i=0;i<4u;i++) if (fixture_observers[i].control) {
    CHECK(fixture_observers[i].control->frame_destroyed);
    CHECK(!ku_task_control_atomic_load(&fixture_observers[i].control->lifecycle_pin));
    CHECK(ku_task_control_lease_release(&fixture_observers[i])==KU_TASK_CONTROL_OK);
  }
  uint64_t finish_deadline=ku_task_driver_now_ms()+2000u;
  CHECK(ku_task_value_drop(&root,finish_deadline)==KU_TASK_DRIVER_OK);
  CHECK(ku_task_driver_shutdown(driver,finish_deadline)==KU_TASK_DRIVER_OK);
  KuTaskDriverSnapshotV1 empty=fixture_idle(driver);
  CHECK(!empty.resident && !empty.reserved_bytes && !empty.queued && !empty.running && !empty.building);
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
  CHECK(!fixture_ledger().allocations && !fixture_ledger().bytes && !fixture_ledger().overflow);
  for (size_t i=0;i<3u;i++) CHECK(ku_test_event_destroy(&fixture_child_events[i]));
  for (size_t i=0;i<2u;i++) CHECK(ku_test_event_destroy(&fixture_close_events[i]));
}
int main(void) {
  CHECK(KU_TASK_FRAME_ABI_VERSION==4u && KU_TASK_DRIVER_ABI_VERSION==6u);
  ku_task_control_deadline_init(&fixture_clock);
  ku_task_control_atomic_init(&fixture_release_children,0);
  ku_task_control_atomic_init(&fixture_release_continuations,0);
  fixture_case(0u); fixture_case(1u); fixture_case(2u);
  puts("task-scope-adapter-ok"); return 0;
}
"#;
