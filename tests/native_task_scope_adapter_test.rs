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
    let instrumentation = format!("{instrumentation}\nstatic void fixture_request_ready(void*);\nstatic void fixture_before_publish(void*);\nstatic void fixture_before_close(void*);\nstatic void fixture_after_close(void*,uint32_t);\n");
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
    generated = replace_once(generated,
        "  uint32_t result = ku_task_control_request_cancel(lease, reason, deadline);",
        "  fixture_request_ready(lease->control);\n  uint32_t result = ku_task_control_request_cancel(lease, reason, deadline);");
    generated = replace_once(generated,
        "      ku_task_control_deadline_store(&control->cleanup_deadline_ms, absolute_deadline_ms);",
        "      fixture_before_publish(control);\n      ku_task_control_deadline_store(&control->cleanup_deadline_ms, absolute_deadline_ms);");
    generated = replace_once(generated,
        "    size_t close_phase = ku_task_control_atomic_load(&parent->driver_lease.control->phase);",
        "    fixture_before_close(parent->driver_lease.control);\n    size_t close_phase = ku_task_control_atomic_load(&parent->driver_lease.control->phase);");
    generated = replace_once(generated,
        "status=ku_task_driver_scope_end(&instance->ticket,&instance->scope_token);",
        "status=ku_task_driver_scope_end(&instance->ticket,&instance->scope_token);\n    fixture_after_close(instance,status);");
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
        "101\n202\nscope-cleanup\nscope-cleanup\ntask-scope-adapter-ok\n"
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
enum {
  FIXTURE_REQUEST_ENTERED, FIXTURE_REQUEST_PROCEED,
  FIXTURE_CLOSE_ENTERED, FIXTURE_CLOSE_PROCEED,
  FIXTURE_PUBLISHING, FIXTURE_PUBLISH_PROCEED, FIXTURE_PUBLICATION_EVENTS
};
static KuTestEvent fixture_publication_events[FIXTURE_PUBLICATION_EVENTS];
static unsigned fixture_publication_enabled, fixture_request_calls, fixture_close_calls, fixture_publish_calls;
static uint32_t fixture_close_outcome;
static KuTaskControlV1* fixture_publication_target;

static void fixture_request_ready(void* raw) {
  if (!fixture_publication_enabled || raw!=fixture_publication_target) return;
  CHECK(!fixture_request_calls++);
  CHECK(ku_task_control_atomic_load(&((KuTaskControlV1*)raw)->phase)==KU_TASK_CONTROL_LIVE);
  /* The real driver wrapper already owns its active registration. No mutex is
   * held here; its unmodified R2 call will run while scope_end holds the mutex. */
  CHECK(ku_test_event_set(&fixture_publication_events[FIXTURE_REQUEST_ENTERED]));
  CHECK(ku_test_event_wait(&fixture_publication_events[FIXTURE_REQUEST_PROCEED],2000u));
}
static void fixture_before_close(void* raw) {
  if (!fixture_publication_enabled || raw!=fixture_publication_target) return;
  CHECK(!fixture_close_calls++);
  CHECK(ku_task_control_atomic_load(&((KuTaskControlV1*)raw)->phase)==KU_TASK_CONTROL_LIVE);
  CHECK(ku_test_event_set(&fixture_publication_events[FIXTURE_CLOSE_ENTERED]));
  CHECK(ku_test_event_wait(&fixture_publication_events[FIXTURE_CLOSE_PROCEED],2000u));
}
static void fixture_after_close(void* raw,uint32_t outcome) {
  if (!fixture_publication_enabled || raw!=fixture_publication_target) return;
  CHECK(outcome==KU_TASK_DRIVER_WAIT_ABORTED && !fixture_closed[0]);
  CHECK(ku_task_control_atomic_load(&((KuTaskControlV1*)raw)->phase)==KU_TASK_CONTROL_PUBLISHING_CANCEL);
  fixture_close_outcome=outcome;
}
static void fixture_before_publish(void* raw) {
  if (!fixture_publication_enabled || raw!=fixture_publication_target) return;
  CHECK(!fixture_publish_calls++);
  CHECK(ku_task_control_atomic_load(&((KuTaskControlV1*)raw)->phase)==KU_TASK_CONTROL_PUBLISHING_CANCEL);
  CHECK(ku_task_control_cleanup_deadline((KuTaskControlV1*)raw)==UINT64_MAX);
  CHECK(ku_test_event_set(&fixture_publication_events[FIXTURE_PUBLISHING]));
  CHECK(ku_test_event_wait(&fixture_publication_events[FIXTURE_PUBLISH_PROCEED],2000u));
}
typedef struct FixturePublicationRequest {
  KuTaskDriverTicketV1 ticket;
  KuTaskControlLeaseV1 lease;
  uint64_t deadline;
  uint32_t status;
} FixturePublicationRequest;
static int fixture_publication_thread(void* raw) {
  FixturePublicationRequest* request=(FixturePublicationRequest*)raw;
  request->status=ku_task_driver_request_cancel(&request->ticket,&request->lease,
      KU_TASK_CONTROL_CANCELLED,request->deadline);
  return 0;
}

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
  fixture_mode=mode==3u ? 2u : mode; fixture_parent=NULL; fixture_transferred=fixture_scope_seen=fixture_staged=fixture_promoted=0;
  fixture_cleanup_drives=0; fixture_cancel_deadline=0;
  fixture_publication_enabled=mode==3u; fixture_publication_target=NULL;
  fixture_request_calls=fixture_close_calls=fixture_publish_calls=0;
  fixture_close_outcome=UINT32_MAX;
  for (size_t i=0;i<FIXTURE_PUBLICATION_EVENTS;i++) CHECK(ku_test_event_init(&fixture_publication_events[i]));
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
  } else if (mode==2u) {
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
  } else {
    CHECK(mode==3u && fixture_publication_enabled);
    KuTaskDriverWaitTokenV1 original_wait=fixture_parent->wait;
    CHECK(original_wait.driver && fixture_parent->scope_mode==1u);
    fixture_cancel_deadline=base+700u;
    FixturePublicationRequest request={0};
    request.ticket=root.ticket; request.deadline=fixture_cancel_deadline; request.status=UINT32_MAX;
    CHECK(ku_task_control_lease_retain(&root.owner.lease,&request.lease)==KU_TASK_CONTROL_OK);
    fixture_publication_target=control;
    KuTestThread thread;
    CHECK(ku_test_thread_start(&thread,fixture_publication_thread,&request));
    CHECK(ku_test_event_wait(&fixture_publication_events[FIXTURE_REQUEST_ENTERED],2000u));
    /* Finish the actual last inner child. The normal adapter can now attempt
     * scope_end, with both real receipts ACKed and the previous wait detached. */
    fixture_release(2u);
    CHECK(ku_test_event_wait(&fixture_publication_events[FIXTURE_CLOSE_ENTERED],2000u));
    CHECK(ku_test_event_set(&fixture_publication_events[FIXTURE_REQUEST_PROCEED]));
    CHECK(ku_test_event_wait(&fixture_publication_events[FIXTURE_PUBLISHING],2000u));
    CHECK(ku_task_control_atomic_load(&control->phase)==KU_TASK_CONTROL_PUBLISHING_CANCEL);
    CHECK(ku_task_control_cleanup_deadline(control)==UINT64_MAX);
    /* Let the original final acquire observe real PUBLISHING. Do not replace
     * that atomic read, scope_end's return, its token or any receipt. */
    CHECK(ku_test_event_set(&fixture_publication_events[FIXTURE_CLOSE_PROCEED]));
    fixture_idle(driver);
    CHECK(fixture_request_calls==1u && fixture_close_calls==1u && fixture_publish_calls==1u);
    CHECK(fixture_close_outcome==KU_TASK_DRIVER_WAIT_ABORTED);
    CHECK(ku_task_control_atomic_load(&control->phase)==KU_TASK_CONTROL_PUBLISHING_CANCEL);
    CHECK(ku_task_control_cleanup_deadline(control)==UINT64_MAX);
    CHECK(fixture_parent->frame.header.status==KU_TASK_FRAME_SCOPE_REQUEST
        && fixture_parent->frame.header.cleanup_state==3u && !fixture_parent->frame.header.result_initialized);
    CHECK(fixture_parent->scope_mode==1u && fixture_parent->scope_expected_mask==6u
        && fixture_parent->scope_issued_mask==6u && !fixture_parent->receipt_mask && !fixture_parent->wait.driver);
    CHECK(fixture_parent->scope_token.epoch==fixture_first_token.epoch
        && fixture_parent->scope_token.parent_generation==fixture_first_token.parent_generation
        && fixture_parent->receipts==fixture_first_span);
    CHECK(fixture_parent->drain_deadline==fixture_scope_deadlines[0] && !fixture_parent->drain_failure);
    CHECK(ku_task_driver_cleanup_receipt_read(&fixture_receipts[1])==KU_TASK_DRIVER_CLEANUP_ACK
        && ku_task_driver_cleanup_receipt_read(&fixture_receipts[2])==KU_TASK_DRIVER_CLEANUP_ACK);
    CHECK(!ku_task_driver_lock(driver));
    KuTaskDriverSlotV1* parked=ku_task_driver_find(&root.ticket);
    CHECK(parked && parked->wrapper_active==1u && parked->scope_session_active
        && !parked->scope_session_final && parked->scope_expected_mask==6u
        && parked->scope_receipts==fixture_first_span
        && parked->scope_wait_deadline==fixture_scope_deadlines[0]);
    CHECK(!ku_task_driver_unlock(driver));
    CHECK(fixture_transferred==6u && !fixture_promoted && !fixture_cleanup_drives
        && !fixture_closed[0] && !fixture_closed[1] && !fixture_drives[1] && !fixture_drives[2]);
    CHECK(!fixture_frees[0] && !fixture_frees[1]);
    fixture_pending(&root);
    KuTaskDriverSnapshotV1 stable=fixture_idle(driver);
    CHECK(fixture_idle(driver).polls==stable.polls); /* No absent-publication busy polling. */
    CHECK(!fixture_cleanup_drives && !fixture_drives[1] && !fixture_closed[0]);
    CHECK(ku_test_event_set(&fixture_publication_events[FIXTURE_PUBLISH_PROCEED]));
    CHECK(ku_test_thread_join(&thread,2000u));
    CHECK(!thread.outcome && request.status==KU_TASK_CONTROL_OK);
    /* The original wrapper publishes D and supplies the actual notification.
     * The real adapter promotes the retained manifest before moving ROOT. */
    CHECK(ku_test_event_wait(&fixture_child_events[2],2000u)); fixture_idle(driver);
    CHECK(fixture_transferred==7u && fixture_promoted==1u && fixture_cleanup_drives==1u);
    CHECK(fixture_frees[0]==1u && fixture_frees[1]==1u && !fixture_staged);
    CHECK(ku_task_control_atomic_load(&control->phase)==KU_TASK_CONTROL_REQUESTED_CANCEL
        && ku_task_control_cleanup_deadline(control)==fixture_cancel_deadline);
    CHECK(fixture_parent->scope_mode==2u && fixture_parent->scope_expected_mask==7u
        && fixture_parent->scope_issued_mask==7u && fixture_parent->receipt_mask==1u);
    CHECK(fixture_parent->scope_token.epoch==fixture_first_token.epoch
        && fixture_parent->receipts==fixture_first_span);
    CHECK(fixture_same_receipt(&fixture_parent->receipts[1],&fixture_receipts[1])
        && fixture_same_receipt(&fixture_parent->receipts[2],&fixture_receipts[2]));
    CHECK(fixture_parent->wait.driver==original_wait.driver
        && fixture_parent->wait.parent_generation==original_wait.parent_generation
        && fixture_parent->wait.epoch>original_wait.epoch);
    CHECK(ku_task_control_cleanup_deadline(fixture_observers[0].control)==fixture_cancel_deadline
        && fixture_parent->drain_deadline==fixture_cancel_deadline);
    CHECK(ku_task_driver_cleanup_receipt_read(&fixture_receipts[0])==KU_TASK_DRIVER_PENDING);
    fixture_pending(&root);
    fixture_release(0u); fixture_idle(driver);
    CHECK(ku_task_driver_cleanup_receipt_read(&fixture_receipts[0])==KU_TASK_DRIVER_CLEANUP_ACK);
    CHECK(ku_task_control_atomic_load(&control->phase)==KU_TASK_CONTROL_CANCELLED
        && ku_task_control_cleanup_deadline(control)==fixture_cancel_deadline);
    CHECK(ku_task_value_take(&root,NULL,&outcome)==KU_TASK_CONTROL_CANCELLED
        && ku_task_outcome_empty(&outcome,4u));
    CHECK(ku_task_driver_wake(&root.ticket)==KU_TASK_DRIVER_OK); fixture_idle(driver);
    CHECK(fixture_cleanup_drives==1u && !fixture_drives[1] && !fixture_drives[2]
        && !fixture_closed[0] && !fixture_closed[1] && !fixture_children[3].driver);
    CHECK(ku_task_control_lease_release(&request.lease)==KU_TASK_CONTROL_OK);
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
  for (size_t i=0;i<FIXTURE_PUBLICATION_EVENTS;i++) CHECK(ku_test_event_destroy(&fixture_publication_events[i]));
}
int main(void) {
  CHECK(KU_TASK_FRAME_ABI_VERSION==4u && KU_TASK_DRIVER_ABI_VERSION==6u);
  ku_task_control_deadline_init(&fixture_clock);
  ku_task_control_atomic_init(&fixture_release_children,0);
  ku_task_control_atomic_init(&fixture_release_continuations,0);
  fixture_case(0u); fixture_case(1u); fixture_case(2u); fixture_case(3u);
  puts("task-scope-adapter-ok"); return 0;
}
"#;

fn scope_loop_frames() -> TaskProgram {
    let mut program = TaskProgram { functions: vec![] };
    program.functions.push(TaskFunction {
        id: TaskFunctionId(0),
        name: "RepeatedScope".into(),
        entry: StateId(0),
        parameters: vec![SlotId(0)],
        result: result(IrType::Null),
        slots: vec![
            value(IrType::Str),
            value(IrType::Int),
            value(IrType::Int),
            value(IrType::Int),
            value(IrType::Bool),
            task(),
            task(),
            value(IrType::Str),
            value(result(IrType::Null)),
            value(IrType::Str),
            value(IrType::Str),
            value(IrType::Int),
            value(IrType::Int), // 12: nonpersistent checked-add temporary
        ],
        states: vec![
            state(
                vec![
                    init(1, 0),
                    init(2, 1),
                    init(3, 2),
                    init(11, -1),
                    TaskOp::Init {
                        dst: SlotId(10),
                        value: TaskConstant::Str("outer".into()),
                    },
                    TaskOp::Start {
                        dst: SlotId(5),
                        function: TaskFunctionId(1),
                        arguments: vec![SlotId(10), SlotId(11)],
                    },
                ],
                TaskTerminator::Jump { target: StateId(1) },
            ),
            state(
                vec![
                    TaskOp::ScopeEnter {
                        scope: TaskScopeId(0),
                        tasks: vec![SlotId(6)],
                    },
                    TaskOp::Init {
                        dst: SlotId(7),
                        value: TaskConstant::Str("round-child".into()),
                    },
                    TaskOp::Init {
                        dst: SlotId(9),
                        value: TaskConstant::Str("round-local".into()),
                    },
                    TaskOp::Start {
                        dst: SlotId(6),
                        function: TaskFunctionId(1),
                        arguments: vec![SlotId(7), SlotId(1)],
                    },
                ],
                TaskTerminator::ScopeDrain {
                    scope: TaskScopeId(0),
                    ready: StateId(2),
                    cleanup: StateId(5),
                },
            ),
            state(
                vec![
                    TaskOp::Drop { slot: SlotId(9) },
                    TaskOp::Binary {
                        dst: SlotId(12),
                        op: TaskBinaryOp::Add,
                        left: SlotId(1),
                        right: SlotId(2),
                    },
                    TaskOp::Copy {
                        dst: SlotId(1),
                        src: SlotId(12),
                    },
                ],
                TaskTerminator::Suspend {
                    resume: StateId(3),
                    cleanup: StateId(5),
                },
            ),
            state(
                vec![TaskOp::Binary {
                    dst: SlotId(4),
                    op: TaskBinaryOp::Less,
                    left: SlotId(1),
                    right: SlotId(3),
                }],
                TaskTerminator::Branch {
                    condition: SlotId(4),
                    then_state: StateId(1),
                    else_state: StateId(4),
                },
            ),
            state(
                vec![TaskOp::Init {
                    dst: SlotId(8),
                    value: TaskConstant::Ok(Box::new(TaskConstant::Null)),
                }],
                TaskTerminator::Exit { value: SlotId(8) },
            ),
            state(
                [0, 7, 8, 9, 10]
                    .map(|slot| TaskOp::DropIfInit { slot: SlotId(slot) })
                    .to_vec(),
                TaskTerminator::Terminate,
            ),
        ],
    });
    program.functions.push(TaskFunction {
        id: TaskFunctionId(1),
        name: "RepeatedChild".into(),
        entry: StateId(0),
        parameters: vec![SlotId(0), SlotId(1)],
        result: result(IrType::Null),
        slots: vec![
            value(IrType::Str),
            value(IrType::Int),
            value(result(IrType::Null)),
        ],
        states: vec![state(
            vec![TaskOp::Init {
                dst: SlotId(2),
                value: TaskConstant::Ok(Box::new(TaskConstant::Null)),
            }],
            TaskTerminator::Exit { value: SlotId(2) },
        )],
    });
    program
}

#[test]
fn native_task_scope_adapter_same_scope_two_rounds_and_second_round_cancel_in_c() {
    // Internal IR only. This requires the separately reviewed Suspend-only
    // cycle rule; no source loop syntax or verifier bypass is introduced here.
    let ast = Parser::new(
        Lexer::new("fn main(): null! { return ok(null) }")
            .lex()
            .unwrap(),
    )
    .parse_program()
    .unwrap();
    Checker::new().check(&ast).unwrap();
    let sync = ir::lower_program(&ast).unwrap();
    let tasks = scope_loop_frames();
    let plan = verify_and_plan(&tasks, TaskLimits::default()).unwrap();
    assert_eq!(
        plan.functions[0].scopes,
        vec![TaskScopeFrame {
            scope: TaskScopeId(0),
            task_mask: 1 << 6,
        }]
    );
    assert_eq!(plan.functions[0].scope_task_mask, (1 << 5) | (1 << 6));
    for slot in [0, 1, 2, 3, 5, 6, 7, 9] {
        assert!(plan.functions[0].slots.contains(&SlotId(slot)));
    }
    let mut generated = c::generate_task_frame_c_source(&sync, &tasks).unwrap();
    assert!(generated.contains("#define KU_TASK_FRAME_ABI_VERSION 4u"));
    assert!(generated.contains("#define KU_TASK_DRIVER_ABI_VERSION 6u"));
    for forbidden in ["run_source", "const SOURCE"] {
        assert!(!generated.contains(forbidden));
    }
    let ledger = replace_once(
        LOCKED_ALLOCATIONS.to_owned(),
        "ku_perf_free(p);",
        "loop_record_free(p); ku_perf_free(p);",
    );
    let instrumentation = format!(
        "{NATIVE_THREAD_LIFECYCLE_HARNESS}\n{LEDGER_LOCK}\n{ALLOCATION_HOOK}\n\
         static void loop_record_free(void*);\n{ledger}\n\
         static int loop_parent_hold(void*);\nstatic void loop_started(void*);\n\
         static void loop_latch(void*);\nstatic void loop_continued(void*,uint64_t);\n\
         static uint32_t loop_child_hold(void*,int);\nstatic void loop_string_drop(void*);\n"
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
    let parent = "static uint32_t ku_task_0_resume(void* raw) {";
    generated = replace_once(
        generated,
        parent,
        &format!("{parent}\n  if (loop_parent_hold(raw)) return KU_TASK_CONTROL_PENDING;"),
    );
    let started = "  { uint32_t started=ku_task_1_start_hosted((const KuTaskAdapterHostV1*)clock->host, &frame->s_7, &frame->s_1, &frame->s_6);\n  if (started!=KU_TASK_DRIVER_OK) { frame->header.running=0; return KU_TASK_FRAME_INVALID_STATE; } }";
    generated = replace_once(
        generated,
        started,
        &format!("{started}\n  loop_started(frame);"),
    );
    generated = replace_once(
        generated,
        "ku_task_state_3:;",
        "ku_task_state_3:;\n  loop_latch(frame);",
    );
    let continued = "status=ku_task_frame_0_scope_continue(&instance->frame,sizeof(instance->frame),KU_TASK_FRAME_ABI_VERSION,descriptor->scope_id);\n  if (status!=KU_TASK_FRAME_OK) return status;";
    generated = replace_once(
        generated,
        continued,
        &format!("{continued}\n  loop_continued(instance,descriptor->scope_id);"),
    );
    for (entry, cleanup) in [
        ("static uint32_t ku_task_1_resume(void* raw) {", 0),
        ("static uint32_t ku_task_1_cleanup(void* raw, uint32_t reason, KuTaskControlV1* budget) {", 1),
    ] {
        generated = replace_once(generated, entry, &format!(
            "{entry}\n  uint32_t held=loop_child_hold(raw,{cleanup});\n  if (held!=UINT32_MAX) return held;"));
    }
    generated = replace_once(
        generated,
        "static void ku_string_drop(KuString* value) {",
        "static void ku_string_drop(KuString* value) {\n  loop_string_drop(value);",
    );
    generated = replace_once(generated, "static uint32_t ku_task_driver_owner_drop_receipt(\n",
        "static uint32_t ku_task_driver_owner_drop_receipt(const KuTaskDriverTicketV1*,KuTaskControlOwnerV1*,uint64_t,KuTaskDriverCleanupReceiptV1*);\nstatic uint32_t loop_real_owner_drop_receipt(\n");
    generated = replace_once(
        generated,
        "int main(void) {",
        "static int loop_original_main(void) {",
    );
    generated.push_str(SCOPE_LOOP_C_MAIN);
    let directory = TempDir::new("native-task-scope-loop");
    let path = directory.path().join("program.c");
    fs::write(&path, generated).unwrap();
    let Some(executable) = compile_harness(directory.path(), &path, "program") else {
        assert!(
            std::env::var_os("GITHUB_ACTIONS").is_none(),
            "CI must execute real repeated-scope adapter paths"
        );
        return;
    };
    fs::remove_file(path).unwrap();
    let output = run_bounded(
        Command::new(executable).current_dir(directory.path()),
        RUN_TIMEOUT,
        RUN_LIMITS,
    )
    .expect("scope rounds must finish under the existing process watchdog");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8(output.stdout).unwrap().replace('\r', ""),
        "task-scope-loop-ok\n"
    );
    assert!(output.stderr.is_empty());
}

const SCOPE_LOOP_C_MAIN: &str = r#"
/* Fake time chooses D comparisons, not timer responsiveness. Each event and
 * OS join retains 2s; frozen-clock wait_idle/shutdown also have the existing
 * real 20s process watchdog. No scope/phase/receipt/ACK is fabricated. */
static unsigned loop_mode,loop_starts,loop_closed,loop_latches,loop_local_drops;
static unsigned loop_child_drops[3],loop_transfers[3],loop_held_free;
static uintptr_t loop_held_id;
static KuAtomicRefcount loop_release,loop_allow;
static KuTestEvent loop_parked[3],loop_closed_event[2];
static KuTaskInstance_0* loop_parent;
static KuTaskInstance_1* loop_instances[3];
static KuTaskDriverTicketV1 loop_children[3];
static KuTaskControlLeaseV1 loop_observers[3];
static KuTaskDriverCleanupReceiptV1 loop_receipts[3];
static KuTaskDriverScopeTokenV1 loop_tokens[2];
static KuTaskDriverWaitTokenV1 loop_waits[2];
static uint64_t loop_deadlines[2],loop_cancel_deadline;

static int loop_empty_string(KuString value) {
  return !value.ptr && !value.len && !value.capacity && !value.storage;
}
static int loop_empty_result(KuResult_null value) {
  return !value.ok && !value.value && loop_empty_string(value.error.domain)
      && loop_empty_string(value.error.code) && loop_empty_string(value.error.message);
}
static void loop_record_free(void* pointer) {
  if (pointer && (uintptr_t)pointer==loop_held_id) {
    CHECK(!loop_held_free++);
    CHECK(loop_transfers[2]==1u); /* Outer owner handed off before Value free. */
  }
}
static void loop_string_drop(void* raw) {
  if (!raw) return;
  if (loop_parent && raw==&loop_parent->frame.s_9
      && !loop_empty_string(loop_parent->frame.s_9)) {
    CHECK(loop_local_drops<2u);
    if (loop_mode && loop_starts==2u) CHECK(loop_transfers[2]==1u);
    else CHECK(loop_closed==loop_local_drops+1u);
    loop_local_drops++;
  }
  for (size_t i=0;i<3u;i++) if (loop_instances[i] && raw==&loop_instances[i]->frame.s_0
      && !loop_empty_string(loop_instances[i]->frame.s_0)) {
    CHECK(!loop_child_drops[i]++ && loop_transfers[i]==1u);
  }
}
static unsigned loop_register(KuTaskInstance_1* child) {
  int64_t id=child->frame.s_1; CHECK(id>=-1 && id<=1);
  unsigned index=id<0 ? 2u : (unsigned)id;
  if (!loop_children[index].driver) {
    loop_children[index]=child->ticket; loop_instances[index]=child;
    KuTaskDriverV1* driver=child->ticket.driver;
    CHECK(!ku_task_driver_lock(driver));
    KuTaskDriverSlotV1* slot=ku_task_driver_find(&child->ticket);
    CHECK(slot && slot->binding==&child->control && slot->driver_lease.control==&child->control);
    CHECK(ku_task_control_lease_retain(&slot->driver_lease,&loop_observers[index])==KU_TASK_CONTROL_OK);
    CHECK(!ku_task_driver_unlock(driver));
  }
  CHECK(loop_instances[index]==child); return index;
}
static int loop_parent_hold(void* raw) {
  KuTaskInstance_0* parent=(KuTaskInstance_0*)raw;
  if (!loop_parent) loop_parent=parent;
  CHECK(loop_parent==parent);
  if (parent->frame.header.status==KU_TASK_FRAME_PENDING && parent->frame.header.state==2u
      && ku_task_control_atomic_load(&loop_allow)<loop_closed) {
    CHECK(ku_task_driver_set_intent(&parent->ticket,KU_TASK_DRIVER_WAIT)==KU_TASK_DRIVER_OK);
    return 1;
  }
  return 0;
}
static void loop_started(void* raw) {
  KuTaskFrame_0* frame=(KuTaskFrame_0*)raw;
  CHECK(loop_parent && frame==&loop_parent->frame && loop_starts<2u);
  CHECK(frame->s_1==(int64_t)loop_starts && loop_closed==loop_starts
      && loop_latches==loop_starts && loop_local_drops==loop_starts);
  CHECK(!loop_held_free && !loop_empty_string(frame->s_9) && loop_empty_string(frame->s_7));
  CHECK(frame->s_5.tag==KU_TASK_VALUE_LIVE
      && ku_task_control_atomic_load(&frame->s_5.owner.lease.control->phase)==KU_TASK_CONTROL_LIVE);
  CHECK(frame->s_6.tag==KU_TASK_VALUE_LIVE);
  unsigned index=loop_register((KuTaskInstance_1*)frame->s_6.owner.lease.control);
  CHECK(index==loop_starts);
  if (index) {
    CHECK(loop_children[1].slot==loop_children[0].slot
        && loop_children[1].generation>loop_children[0].generation);
    CHECK(ku_task_driver_cleanup_receipt_read(&loop_receipts[0])==KU_TASK_DRIVER_CLEANUP_ACK);
  }
  loop_starts++;
}
static void loop_latch(void* raw) {
  KuTaskFrame_0* frame=(KuTaskFrame_0*)raw;
  CHECK(frame==&loop_parent->frame && loop_latches<2u);
  CHECK(frame->s_1==(int64_t)(loop_latches+1u) && loop_closed==loop_latches+1u);
  CHECK(loop_local_drops==loop_closed && loop_empty_string(frame->s_9)
      && !(frame->header.initialized&(UINT64_C(1)<<9)));
  CHECK(!loop_held_free); loop_latches++;
}
static uint32_t loop_child_hold(void* raw,int cleanup) {
  KuTaskInstance_1* child=(KuTaskInstance_1*)raw;
  unsigned index=loop_register(child);
  if (cleanup && (ku_task_control_atomic_load(&loop_release)&((size_t)1u<<index)))
    return UINT32_MAX;
  CHECK(ku_task_driver_set_intent(&child->ticket,KU_TASK_DRIVER_WAIT)==KU_TASK_DRIVER_OK);
  if (cleanup) CHECK(ku_test_event_set(&loop_parked[index]));
  return KU_TASK_CONTROL_PENDING;
}
static uint32_t ku_task_driver_owner_drop_receipt(const KuTaskDriverTicketV1* ticket,
    KuTaskControlOwnerV1* owner,uint64_t deadline,KuTaskDriverCleanupReceiptV1* receipt) {
  unsigned index=loop_register((KuTaskInstance_1*)owner->lease.control);
  CHECK(!loop_transfers[index] && loop_parent);
  CHECK(receipt==&loop_parent->receipts[index==2u ? 0u : 1u]);
  if (index<2u) {
    CHECK(loop_parent->scope_mode==1u && loop_parent->scope_expected_mask==2u);
    loop_tokens[index]=loop_parent->scope_token; loop_deadlines[index]=deadline;
    CHECK(loop_tokens[index].scope_id==0u && loop_tokens[index].driver==ticket->driver);
    CHECK(!ku_task_driver_lock(ticket->driver));
    KuTaskDriverSlotV1* registered=ku_task_driver_find(&loop_parent->ticket);
    CHECK(registered && registered->scope_session_active && !registered->scope_session_final
        && registered->scope_receipts==loop_parent->receipts && registered->scope_expected_mask==2u
        && registered->scope_session_epoch==loop_tokens[index].epoch);
    CHECK(!ku_task_driver_unlock(ticket->driver));
    if (index) {
      CHECK(loop_tokens[1].epoch>loop_tokens[0].epoch
          && loop_tokens[1].parent_generation==loop_tokens[0].parent_generation);
      CHECK(deadline>loop_deadlines[0]);
      KuTaskDriverScopeTokenV1 old=loop_tokens[0];
      CHECK(ku_task_driver_scope_end(&loop_parent->ticket,&old)==KU_TASK_DRIVER_STALE);
      CHECK(old.driver==loop_tokens[0].driver && old.epoch==loop_tokens[0].epoch);
      KuTaskDriverWaitTokenV1 rejected={0};
      CHECK(ku_task_driver_cleanup_wait_arm(&loop_parent->ticket,&loop_receipts[0],deadline,
          &rejected)==KU_TASK_DRIVER_INVALID_ARGUMENT && !rejected.driver);
    }
  } else if (loop_mode) {
    CHECK(loop_parent->scope_mode==2u && loop_parent->scope_expected_mask==3u
        && loop_parent->scope_token.epoch==loop_tokens[1].epoch);
    CHECK(deadline==loop_cancel_deadline && loop_closed==1u);
    CHECK(loop_parent->receipts[1].generation==loop_receipts[1].generation);
  } else CHECK(loop_closed==2u && !loop_parent->scope_mode);
  uint32_t status=loop_real_owner_drop_receipt(ticket,owner,deadline,receipt);
  CHECK(status==KU_TASK_DRIVER_OK && !owner->lease.control);
  loop_receipts[index]=*receipt; loop_transfers[index]++;
  if (index==1u) {
    KuTaskControlV1* control=loop_observers[1].control;
    uint64_t before=ku_task_control_cleanup_deadline(control);
    CHECK(before==deadline);
    CHECK(ku_task_driver_cancel_receipt(&loop_receipts[0],deadline-500u)==KU_TASK_DRIVER_CLEANUP_ACK);
    CHECK(ku_task_control_cleanup_deadline(control)==before);
  }
  return status;
}
static void loop_continued(void* raw,uint64_t scope) {
  KuTaskInstance_0* parent=(KuTaskInstance_0*)raw;
  unsigned index=loop_closed; CHECK(parent==loop_parent && scope==0u && index<2u);
  CHECK(!loop_mode || !index);
  CHECK(loop_starts==index+1u && loop_latches==index && loop_local_drops==index);
  CHECK(parent->frame.header.status==KU_TASK_FRAME_PENDING && parent->frame.header.state==2u);
  CHECK(!parent->scope_token.driver && !parent->receipt_mask && !parent->wait.driver);
  CHECK(parent->scope_issued_mask==2u && parent->scope_expected_mask==2u);
  CHECK(ku_task_driver_cleanup_receipt_read(&loop_receipts[index])==KU_TASK_DRIVER_CLEANUP_ACK);
  CHECK(loop_child_drops[index]==1u && loop_observers[index].control->frame_destroyed
      && loop_empty_string(loop_instances[index]->frame.s_0));
  KuTaskDriverWaitSnapshotV1 untouched={0}; untouched.state=77u;
  CHECK(ku_task_driver_wait_read(&loop_waits[index],&untouched)==KU_TASK_DRIVER_STALE && untouched.state==77u);
  CHECK(!loop_held_free && loop_transfers[2]==0u);
  KuTaskDriverV1* driver=parent->ticket.driver;
  CHECK(!ku_task_driver_lock(driver));
  KuTaskDriverSlotV1* slot=ku_task_driver_find(&parent->ticket);
  CHECK(slot && !slot->scope_session_active && !slot->scope_receipts
      && !slot->scope_wait_started && slot->scope_wait_deadline==UINT64_MAX);
  CHECK(!ku_task_driver_unlock(driver));
  loop_closed++; CHECK(ku_test_event_set(&loop_closed_event[index]));
}
static KuTaskDriverSnapshotV1 loop_idle(KuTaskDriverV1* driver) {
  CHECK(ku_task_driver_wait_idle(driver,ku_task_driver_now_ms()+2000u)==KU_TASK_DRIVER_OK);
  KuTaskDriverSnapshotV1 out={0};
  CHECK(ku_task_driver_snapshot(driver,&out)==KU_TASK_DRIVER_OK);
  CHECK(!out.running && !out.queued && !out.building && !out.fault && !out.clock_fault);
  CHECK(out.worker_waiting || out.worker_exited); return out;
}
static void loop_release_child(unsigned index) {
  ku_task_control_atomic_store(&loop_release,
      ku_task_control_atomic_load(&loop_release)|((size_t)1u<<index));
  CHECK(ku_task_driver_wake(&loop_children[index])==KU_TASK_DRIVER_OK);
}
static void loop_pending(KuTaskValueV1* root) {
  KuTaskAdapterOutcomeV1 outcome={0};
  CHECK(ku_task_value_take(root,NULL,&outcome)==KU_TASK_CONTROL_PENDING);
  CHECK(ku_task_outcome_empty(&outcome,3u)); loop_idle(root->ticket.driver);
}
static void loop_capture_wait(unsigned index) {
  loop_waits[index]=loop_parent->wait;
  CHECK(loop_waits[index].driver && loop_parent->scope_token.epoch==loop_tokens[index].epoch);
  if (index) CHECK(loop_waits[1].epoch>loop_waits[0].epoch);
  KuTaskDriverWaitSnapshotV1 wait={0};
  CHECK(ku_task_driver_wait_read(&loop_waits[index],&wait)==KU_TASK_DRIVER_OK);
  CHECK(wait.kind==KU_TASK_DRIVER_WAIT_KIND_CLEANUP_ACK && wait.state==KU_TASK_DRIVER_WAIT_ARMED
      && wait.child_slot==loop_children[index].slot && wait.child_generation==loop_children[index].generation);
}
static void loop_case(unsigned mode) {
  CHECK(!fixture_ledger().allocations && !fixture_ledger().bytes);
  loop_mode=mode; loop_starts=loop_closed=loop_latches=loop_local_drops=loop_held_free=0;
  loop_parent=NULL; loop_held_id=0; loop_cancel_deadline=0;
  memset(loop_instances,0,sizeof(loop_instances)); memset(loop_children,0,sizeof(loop_children));
  memset(loop_observers,0,sizeof(loop_observers)); memset(loop_receipts,0,sizeof(loop_receipts));
  memset(loop_tokens,0,sizeof(loop_tokens)); memset(loop_waits,0,sizeof(loop_waits));
  memset(loop_deadlines,0,sizeof(loop_deadlines)); memset(loop_transfers,0,sizeof(loop_transfers));
  memset(loop_child_drops,0,sizeof(loop_child_drops));
  ku_task_control_atomic_store(&loop_release,0); ku_task_control_atomic_store(&loop_allow,0);
  for (size_t i=0;i<3u;i++) CHECK(ku_test_event_init(&loop_parked[i]));
  for (size_t i=0;i<2u;i++) CHECK(ku_test_event_init(&loop_closed_event[i]));
  uint64_t base=ku_test_real_now_ms()+100000u;
  ku_task_control_deadline_store(&fixture_clock,base);
  KuTaskDriverV1* driver=(KuTaskDriverV1*)calloc(1,sizeof(*driver));
  KuTaskDriverSlotV1* slots=(KuTaskDriverSlotV1*)calloc(3,sizeof(*slots));
  size_t* ring=(size_t*)calloc(3,sizeof(*ring)); CHECK(driver && slots && ring);
  size_t fixed=sizeof(*driver)+3u*(sizeof(*slots)+sizeof(*ring));
  CHECK(ku_task_driver_init(driver,sizeof(*driver),KU_TASK_DRIVER_ABI_VERSION,slots,3,ring,3,
      fixed+sizeof(KuTaskInstance_0)+2u*sizeof(KuTaskInstance_1)+37u)==KU_TASK_DRIVER_OK);
  uint8_t* bytes=(uint8_t*)malloc(37u); CHECK(bytes); memcpy(bytes,"carried",7u);
  loop_held_id=(uintptr_t)bytes; KuString carried={bytes,7u,37u,KU_STRING_OWNED};
  KuTaskValueV1 root={0};
  CHECK(ku_task_0_start_value(driver,&carried,&root)==KU_TASK_DRIVER_OK && root.tag==KU_TASK_VALUE_LIVE);
  CHECK(loop_empty_string(carried));
  CHECK(ku_test_event_wait(&loop_parked[0],2000u)); loop_idle(driver);
  CHECK(loop_starts==1u && !loop_closed && !loop_latches && !loop_held_free);
  CHECK(loop_deadlines[0]==base+1000u); loop_capture_wait(0u); loop_pending(&root);
  for (unsigned repeat=0;repeat<3u;repeat++) {
    CHECK(ku_task_driver_wake(&root.ticket)==KU_TASK_DRIVER_OK); loop_idle(driver);
    CHECK(loop_starts==1u && loop_parent->wait.epoch==loop_waits[0].epoch
        && loop_parent->scope_token.epoch==loop_tokens[0].epoch && !loop_local_drops);
  }
  loop_release_child(0u);
  CHECK(ku_test_event_wait(&loop_closed_event[0],2000u)); loop_idle(driver);
  CHECK(loop_closed==1u && !loop_latches && !loop_parent->scope_mode && !loop_parent->drain_started);
  for (size_t i=0;i<2u;i++) CHECK(ku_task_driver_scope_receipt_empty(&loop_parent->receipts[i]));
  CHECK(!loop_local_drops && !loop_held_free);
  /* Real observer release physically returns the only free child slot before
   * permitting the generated latch/Suspend/backedge, forcing ABA reuse. */
  loop_instances[0]=NULL;
  CHECK(ku_task_control_lease_release(&loop_observers[0])==KU_TASK_CONTROL_OK);
  CHECK(!ku_task_driver_lock(driver));
  CHECK(!ku_task_driver_find(&loop_children[0]) && driver->resident==2u);
  CHECK(!ku_task_driver_unlock(driver));
  ku_task_control_deadline_store(&fixture_clock,base+200u);
  ku_task_control_atomic_store(&loop_allow,1u);
  CHECK(ku_task_driver_wake(&root.ticket)==KU_TASK_DRIVER_OK);
  CHECK(ku_test_event_wait(&loop_parked[1],2000u)); loop_idle(driver);
  CHECK(loop_starts==2u && loop_closed==1u && loop_latches==1u && loop_local_drops==1u);
  CHECK(loop_deadlines[1]==base+1200u && !loop_held_free);
  loop_capture_wait(1u); loop_pending(&root);
  KuTaskDriverWaitSnapshotV1 old_snapshot={0}; old_snapshot.state=91u;
  CHECK(ku_task_driver_wait_read(&loop_waits[0],&old_snapshot)==KU_TASK_DRIVER_STALE
      && old_snapshot.state==91u);
  KuTaskDriverWaitTokenV1 old_wait=loop_waits[0];
  CHECK(ku_task_driver_wait_detach(&old_wait)==KU_TASK_DRIVER_STALE);
  CHECK(loop_parent->wait.epoch==loop_waits[1].epoch);
  KuTaskControlV1* control=root.owner.lease.control;
  KuTaskAdapterOutcomeV1 outcome={0};
  if (!mode) {
    loop_release_child(1u);
    CHECK(ku_test_event_wait(&loop_closed_event[1],2000u)); loop_idle(driver);
    CHECK(loop_closed==2u && loop_latches==1u && loop_local_drops==1u && !loop_held_free);
    ku_task_control_deadline_store(&fixture_clock,base+400u);
    ku_task_control_atomic_store(&loop_allow,2u);
    CHECK(ku_task_driver_wake(&root.ticket)==KU_TASK_DRIVER_OK);
    CHECK(ku_test_event_wait(&loop_parked[2],2000u)); loop_idle(driver);
    CHECK(loop_latches==2u && loop_local_drops==2u && loop_held_free==1u);
    CHECK(loop_parent->payload_initialized && !loop_parent->frame.header.result_initialized);
    CHECK(ku_task_control_atomic_load(&control->phase)==KU_TASK_CONTROL_LIVE);
    loop_pending(&root); loop_release_child(2u); loop_idle(driver);
    CHECK(ku_task_control_atomic_load(&control->phase)==KU_TASK_CONTROL_COMPLETED);
    CHECK(ku_task_value_take(&root,NULL,&outcome)==KU_TASK_CONTROL_OK);
    CHECK(outcome.result_kind==3u && outcome.exit_class==KU_TASK_EXIT_USER_RESULT
        && outcome.value.null_value.ok && !outcome.has_cleanup_deadline && !outcome.cleanup_deadline);
  } else {
    loop_cancel_deadline=base+700u;
    CHECK(ku_task_driver_request_cancel(&root.ticket,&root.owner.lease,KU_TASK_CONTROL_CANCELLED,
        loop_cancel_deadline)==KU_TASK_CONTROL_OK);
    CHECK(ku_test_event_wait(&loop_parked[2],2000u)); loop_idle(driver);
    CHECK(loop_parent->scope_mode==2u && loop_parent->scope_expected_mask==3u
        && loop_parent->scope_token.epoch==loop_tokens[1].epoch);
    CHECK(loop_starts==2u && loop_closed==1u && loop_latches==1u
        && loop_local_drops==2u && loop_held_free==1u);
    loop_pending(&root);
    loop_cancel_deadline=base+600u;
    CHECK(ku_task_driver_request_cancel(&root.ticket,&root.owner.lease,KU_TASK_CONTROL_TIMED_OUT,
        loop_cancel_deadline)==KU_TASK_CONTROL_OK); loop_idle(driver);
    CHECK(ku_task_driver_request_cancel(&root.ticket,&root.owner.lease,KU_TASK_CONTROL_TIMED_OUT,
        base+900u)==KU_TASK_CONTROL_OK); loop_idle(driver);
    CHECK(ku_task_control_atomic_load(&control->phase)==KU_TASK_CONTROL_REQUESTED_CANCEL
        && ku_task_control_cleanup_deadline(control)==loop_cancel_deadline
        && loop_parent->drain_deadline==loop_cancel_deadline);
    for (size_t i=1;i<3u;i++) CHECK(ku_task_control_cleanup_deadline(loop_observers[i].control)==loop_cancel_deadline
        && ku_task_control_atomic_load(&loop_observers[i].control->phase)==KU_TASK_CONTROL_REQUESTED_CANCEL);
    CHECK(loop_parent->scope_token.epoch==loop_tokens[1].epoch && loop_parent->wait.epoch==loop_waits[1].epoch);
    loop_release_child(1u); loop_release_child(2u); loop_idle(driver);
    CHECK(ku_task_control_atomic_load(&control->phase)==KU_TASK_CONTROL_CANCELLED);
    CHECK(ku_task_value_take(&root,NULL,&outcome)==KU_TASK_CONTROL_CANCELLED
        && ku_task_outcome_empty(&outcome,3u));
    CHECK(ku_task_control_cleanup_deadline(control)==loop_cancel_deadline);
  }
  loop_idle(driver);
  CHECK(loop_starts==2u && loop_local_drops==2u && loop_held_free==1u);
  CHECK(control->frame_destroyed && !loop_parent->frame_initialized
      && !loop_parent->frame.header.initialized && !loop_parent->frame.header.result_initialized
      && !loop_parent->payload_initialized);
  CHECK(loop_empty_result(loop_parent->frame.result) && loop_empty_result(loop_parent->payload));
  for (size_t i=0;i<3u;i++) {
    CHECK(loop_child_drops[i]==1u && loop_transfers[i]==1u);
    CHECK(ku_task_driver_cleanup_receipt_read(&loop_receipts[i])==KU_TASK_DRIVER_CLEANUP_ACK);
    if (loop_observers[i].control) {
      CHECK(loop_observers[i].control->frame_destroyed
          && !ku_task_control_atomic_load(&loop_observers[i].control->lifecycle_pin));
      loop_instances[i]=NULL;
      CHECK(ku_task_control_lease_release(&loop_observers[i])==KU_TASK_CONTROL_OK);
    }
  }
  ku_task_outcome_drop(&outcome); CHECK(ku_task_outcome_empty(&outcome,3u));
  loop_parent=NULL;
  uint64_t finish=ku_task_driver_now_ms()+1000u;
  CHECK(ku_task_value_drop(&root,finish)==KU_TASK_DRIVER_OK);
  CHECK(ku_task_driver_shutdown(driver,finish)==KU_TASK_DRIVER_OK);
  KuTaskDriverSnapshotV1 empty=loop_idle(driver);
  CHECK(!empty.resident && !empty.reserved_bytes);
#if defined(_WIN32)
  CHECK(WaitForSingleObject(driver->thread,2000u)==WAIT_OBJECT_0);
#else
  alarm(2);
#endif
  CHECK(ku_task_driver_destroy(driver)==KU_TASK_DRIVER_OK);
#if !defined(_WIN32)
  alarm(0);
#endif
  free(ring); free(slots); free(driver);
  CHECK(!fixture_ledger().allocations && !fixture_ledger().bytes && !fixture_ledger().overflow);
  for (size_t i=0;i<3u;i++) CHECK(ku_test_event_destroy(&loop_parked[i]));
  for (size_t i=0;i<2u;i++) CHECK(ku_test_event_destroy(&loop_closed_event[i]));
}
int main(void) {
  CHECK(KU_TASK_FRAME_ABI_VERSION==4u && KU_TASK_DRIVER_ABI_VERSION==6u && KU_TASK_CONTROL_ABI_VERSION==2u);
  ku_task_control_deadline_init(&fixture_clock);
  ku_task_control_atomic_init(&loop_release,0); ku_task_control_atomic_init(&loop_allow,0);
  loop_case(0u); loop_case(1u); puts("task-scope-loop-ok"); return 0;
}
"#;
