//! Actual generated empty ScopeDrain adapters; this does not admit source If.
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

fn value(ty: IrType) -> TaskSlot {
    TaskSlot {
        ty: TaskSlotType::Value {
            ty,
            borrowed: false,
        },
    }
}
fn result(ty: IrType) -> IrType {
    IrType::Result(Box::new(ty))
}
fn state(operations: Vec<TaskOp>, terminator: TaskTerminator) -> TaskState {
    TaskState {
        operations,
        terminator,
    }
}
fn frames(root_child: bool) -> TaskProgram {
    let mut slots = vec![
        value(result(IrType::Str)),
        value(IrType::Str),
        value(IrType::Int),
    ];
    let mut enter = vec![TaskOp::Init {
        dst: SlotId(2),
        value: TaskConstant::Int(42),
    }];
    if root_child {
        slots.push(TaskSlot {
            ty: TaskSlotType::Task {
                result: result(IrType::Null),
            },
        });
        enter.push(TaskOp::Start {
            dst: SlotId(3),
            function: TaskFunctionId(1),
            arguments: vec![],
        });
    }
    enter.push(TaskOp::ScopeEnter {
        scope: TaskScopeId(0),
        tasks: vec![],
    });
    TaskProgram {
        functions: vec![
            TaskFunction {
                id: TaskFunctionId(0),
                name: "EmptyScopeParent".into(),
                entry: StateId(0),
                parameters: vec![SlotId(0), SlotId(1)],
                slots,
                result: result(IrType::Str),
                states: vec![
                    state(
                        enter,
                        TaskTerminator::ScopeDrain {
                            scope: TaskScopeId(0),
                            ready: StateId(1),
                            cleanup: StateId(2),
                        },
                    ),
                    state(
                        vec![TaskOp::Print {
                            value: SlotId(2),
                            newline: true,
                        }],
                        TaskTerminator::Exit { value: SlotId(0) },
                    ),
                    state(
                        vec![
                            TaskOp::DropIfInit { slot: SlotId(1) },
                            TaskOp::DropIfInit { slot: SlotId(0) },
                        ],
                        TaskTerminator::Terminate,
                    ),
                ],
            },
            TaskFunction {
                id: TaskFunctionId(1),
                name: "EmptyScopeChild".into(),
                entry: StateId(0),
                parameters: vec![],
                slots: vec![value(result(IrType::Null))],
                result: result(IrType::Null),
                states: vec![state(
                    vec![TaskOp::Init {
                        dst: SlotId(0),
                        value: TaskConstant::Ok(Box::new(TaskConstant::Null)),
                    }],
                    TaskTerminator::Complete { value: SlotId(0) },
                )],
            },
        ],
    }
}
fn replace_once(source: String, anchor: &str, replacement: &str) -> String {
    assert_eq!(
        source.matches(anchor).count(),
        1,
        "empty scope hook: {anchor}"
    );
    source.replacen(anchor, replacement, 1)
}

#[test]
fn native_task_empty_scope_adapter_without_tasks_and_with_unconsumed_root_task_in_c() {
    let ast = Parser::new(
        Lexer::new("fn main(): null! { return ok(null) }")
            .lex()
            .unwrap(),
    )
    .parse_program()
    .unwrap();
    Checker::new().check(&ast).unwrap();
    let sync = ir::lower_program(&ast).unwrap();
    for root_child in [false, true] {
        let tasks = frames(root_child);
        let plan = verify_and_plan(&tasks, TaskLimits::default()).unwrap();
        assert_eq!(
            plan.functions[0].scopes,
            vec![TaskScopeFrame {
                scope: TaskScopeId(0),
                task_mask: 0
            }]
        );
        assert_eq!(
            plan.functions[0].scope_task_mask,
            if root_child { 8 } else { 0 }
        );
        assert!(plan.functions[0].hosted && plan.functions[0].exit_bridge);
        assert!(
            plan.functions[0].slots.contains(&SlotId(2)),
            "Copy crosses the actual ScopeRequest boundary"
        );
        let mut generated = c::generate_task_frame_c_source(&sync, &tasks).unwrap();
        for required in [
            "#define KU_TASK_FRAME_ABI_VERSION 4u",
            "#define KU_TASK_CONTROL_ABI_VERSION 2u",
            "#define KU_TASK_DRIVER_ABI_VERSION 6u",
            "KU_TASK_FRAME_SCOPE_REQUEST = 12u",
            "static uint32_t ku_task_0_scope_resume(",
        ] {
            assert!(generated.contains(required));
        }
        for forbidden in [
            "run_source",
            "const SOURCE",
            "KuTaskDriverCleanupReceiptV1 receipts[0];",
        ] {
            assert!(!generated.contains(forbidden));
        }
        let ledger = replace_once(
            LOCKED_ALLOCATIONS.to_owned(),
            "ku_perf_free(p);",
            "fixture_record_free(p); ku_perf_free(p);",
        );
        let instrumentation = format!(
            "{NATIVE_THREAD_LIFECYCLE_HARNESS}\n{LEDGER_LOCK}\n{ALLOCATION_HOOK}\nstatic void fixture_record_free(void*);\n{ledger}\nstatic int fixture_parent_pause(void*);\nstatic void fixture_drive(void*,int);\nstatic void fixture_returned(void*,uint32_t);\nstatic void fixture_continued(void*,uint64_t);\nstatic uint32_t fixture_child_hold(void*,int);\n"
        );
        generated = replace_once(
            generated,
            "typedef struct KuString {",
            &format!("{instrumentation}typedef struct KuString {{"),
        );
        let entry = "static uint32_t ku_task_0_resume(void* raw) {";
        generated = replace_once(
            generated,
            entry,
            &format!("{entry}\n  if (fixture_parent_pause(raw)) return KU_TASK_CONTROL_PENDING;"),
        );
        let drive = "static uint32_t ku_task_frame_0_drive(KuTaskFrame_0* frame, const KuTaskFrameClockV1* clock, int cleanup) {";
        generated = replace_once(
            generated,
            drive,
            &format!("{drive}\n  fixture_drive(frame,cleanup);"),
        );
        let resume = "status = ku_task_frame_0_resume(&instance->frame, sizeof(instance->frame), KU_TASK_FRAME_ABI_VERSION, &clock);";
        generated = replace_once(
            generated,
            resume,
            &format!("{resume}\n    fixture_returned(instance,status);"),
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
        generated.push_str(&format!(
            "\n#define FIXTURE_ROOT_TASK {}\n",
            u8::from(root_child)
        ));
        generated.push_str(C_MAIN);
        let directory = TempDir::new("native-task-empty-scope");
        let path = directory.path().join("program.c");
        fs::write(&path, generated).unwrap();
        let Some(executable) = compile_harness(directory.path(), &path, "program") else {
            assert!(
                std::env::var_os("GITHUB_ACTIONS").is_none(),
                "CI must execute the generated empty scope adapter"
            );
            return;
        };
        fs::remove_file(path).unwrap();
        let output = run_bounded(
            Command::new(executable).current_dir(directory.path()),
            RUN_TIMEOUT,
            RUN_LIMITS,
        )
        .expect("empty scope adapter must stop within the unchanged real process watchdog");
        assert!(
            output.status.success(),
            "root_child={root_child}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            String::from_utf8(output.stdout).unwrap().replace('\r', ""),
            "42\nempty-scope-adapter-ok\n"
        );
        assert!(output.stderr.is_empty());
    }
}

const C_MAIN: &str = r#"
/* Real monotonic clock throughout; events and worker joins keep their 2s
 * bounds, with the existing 20s process watchdog as the outer safety net. */
static KuAtomicRefcount fixture_release_parent, fixture_release_child;
static KuTestEvent fixture_closed_event, fixture_child_cleanup_event;
static KuTaskInstance_0* fixture_parent;
static KuTaskInstance_1* fixture_child;
static KuTaskControlLeaseV1 fixture_child_observer;
static KuTaskDriverCleanupReceiptV1 fixture_child_receipt;
static uint64_t fixture_child_deadline;
static unsigned fixture_drives[2], fixture_closed, fixture_requests, fixture_handoffs;
static uintptr_t fixture_ids[2];
static unsigned fixture_frees[2];
static FixtureLedger fixture_scope_ledger;

static int fixture_empty_string(KuString value) {
  return !value.ptr && !value.len && !value.capacity && !value.storage;
}
static int fixture_empty_result(KuResult_str value) {
  return !value.ok && fixture_empty_string(value.value) && fixture_empty_string(value.error.domain)
      && fixture_empty_string(value.error.code) && fixture_empty_string(value.error.message);
}
static void fixture_record_free(void* pointer) {
  if (!pointer) return;
  for (size_t i=0;i<2u;i++) if (fixture_ids[i]==(uintptr_t)pointer) {
    CHECK(!fixture_frees[i]++);
    CHECK(fixture_closed==1u && fixture_drives[1]==1u);
    CHECK(fixture_handoffs==(unsigned)FIXTURE_ROOT_TASK);
  }
}
static KuString fixture_owned(unsigned id,const char* text) {
  size_t length=strlen(text), capacity=length+7u; CHECK(id<2u);
  uint8_t* pointer=(uint8_t*)malloc(capacity); CHECK(pointer);
  memcpy(pointer,text,length); fixture_ids[id]=(uintptr_t)pointer;
  KuString value={pointer,length,capacity,KU_STRING_OWNED}; return value;
}
static int fixture_parent_pause(void* raw) {
  KuTaskInstance_0* parent=(KuTaskInstance_0*)raw;
  if (!fixture_parent) fixture_parent=parent;
  CHECK(fixture_parent==parent);
  if (fixture_closed && parent->frame.header.status==KU_TASK_FRAME_PENDING
      && parent->frame.header.state==1u && !ku_task_control_atomic_load(&fixture_release_parent)) {
    CHECK(ku_task_driver_set_intent(&parent->ticket,KU_TASK_DRIVER_WAIT)==KU_TASK_DRIVER_OK);
    return 1;
  }
  return 0;
}
static void fixture_drive(void* raw,int cleanup) {
  KuTaskFrame_0* frame=(KuTaskFrame_0*)raw;
  CHECK(!cleanup && frame->header.state<2u && !fixture_drives[frame->header.state]++);
  if (frame->header.state==1u) CHECK(fixture_closed==1u);
}
static void fixture_returned(void* raw,uint32_t status) {
  KuTaskInstance_0* parent=(KuTaskInstance_0*)raw;
  if (status==KU_TASK_FRAME_SCOPE_REQUEST) {
    CHECK(!fixture_requests++ && parent->frame.header.state==0u && !fixture_closed);
    CHECK(!parent->frame.header.result_initialized && !fixture_frees[0] && !fixture_frees[1]);
    fixture_scope_ledger=fixture_ledger();
  }
}
static void fixture_no_scope_budget(KuTaskInstance_0* parent) {
  CHECK(!parent->scope_mode && !parent->scope_expected_mask && !parent->scope_issued_mask);
  CHECK(!parent->scope_token.driver && !parent->scope_token.epoch && !parent->receipt_mask && !parent->wait.driver);
  CHECK(!parent->drain_started && !parent->drain_deadline && !parent->drain_failure);
  CHECK(!parent->drain_deadline_published && !parent->drain_published_deadline);
  CHECK(!parent->has_cleanup_deadline && !parent->cleanup_deadline);
  CHECK(ku_task_driver_scope_receipt_empty(&parent->receipts[0]));
  KuTaskDriverV1* driver=parent->ticket.driver;
  CHECK(!ku_task_driver_lock(driver));
  KuTaskDriverSlotV1* slot=ku_task_driver_find(&parent->ticket);
  CHECK(slot && !slot->scope_session_active && !slot->scope_session_final && !slot->scope_session_epoch);
  CHECK(!slot->scope_receipts && !slot->scope_expected_mask && !slot->scope_receipt_capacity);
  CHECK(!slot->scope_wait_started && slot->scope_wait_deadline==UINT64_MAX && !slot->scope_wait_failure);
  CHECK(!slot->wait_epoch && !slot->scope_deadline_fired);
  CHECK(!ku_task_driver_unlock(driver));
}
static void fixture_continued(void* raw,uint64_t scope) {
  KuTaskInstance_0* parent=(KuTaskInstance_0*)raw;
  CHECK(!scope && !fixture_closed++ && fixture_requests==1u);
  CHECK(parent->frame.header.status==KU_TASK_FRAME_PENDING && parent->frame.header.state==1u);
  CHECK(fixture_drives[0]==1u && !fixture_drives[1] && !fixture_handoffs);
  CHECK(!fixture_frees[0] && !fixture_frees[1]);
  CHECK(ku_task_control_atomic_load(&parent->control.phase)==KU_TASK_CONTROL_LIVE);
  fixture_no_scope_budget(parent);
  FixtureLedger after=fixture_ledger();
  CHECK(after.calls==fixture_scope_ledger.calls && after.allocations==fixture_scope_ledger.allocations
      && after.bytes==fixture_scope_ledger.bytes && !after.overflow);
#if FIXTURE_ROOT_TASK
  CHECK((parent->frame.header.initialized&UINT64_C(8)) && parent->frame.s_3.tag==KU_TASK_VALUE_LIVE);
  CHECK(parent->frame.s_3.owner.lease.control && !fixture_child_observer.control);
  fixture_child=(KuTaskInstance_1*)parent->frame.s_3.owner.lease.control;
  CHECK(ku_task_control_atomic_load(&fixture_child->control.phase)==KU_TASK_CONTROL_LIVE);
  CHECK(ku_task_control_lease_retain(&parent->frame.s_3.owner.lease,&fixture_child_observer)==KU_TASK_CONTROL_OK);
#endif
  CHECK(ku_test_event_set(&fixture_closed_event));
}
static uint32_t fixture_child_hold(void* raw,int cleanup) {
  KuTaskInstance_1* child=(KuTaskInstance_1*)raw;
  CHECK(FIXTURE_ROOT_TASK && fixture_child==child && fixture_closed==1u);
  if (cleanup) {
    CHECK(fixture_handoffs==1u);
    CHECK(ku_task_control_atomic_load(&child->control.phase)==KU_TASK_CONTROL_REQUESTED_CANCEL);
    CHECK(ku_task_control_cleanup_deadline(&child->control)==fixture_child_deadline);
    if (ku_task_control_atomic_load(&fixture_release_child)) return UINT32_MAX;
    CHECK(ku_test_event_set(&fixture_child_cleanup_event));
  }
  CHECK(ku_task_driver_set_intent(&child->ticket,KU_TASK_DRIVER_WAIT)==KU_TASK_DRIVER_OK);
  return KU_TASK_CONTROL_PENDING;
}
static uint32_t ku_task_driver_owner_drop_receipt(const KuTaskDriverTicketV1* ticket,
    KuTaskControlOwnerV1* owner,uint64_t deadline,KuTaskDriverCleanupReceiptV1* receipt) {
  CHECK(FIXTURE_ROOT_TASK && fixture_parent && owner->lease.control==fixture_child_observer.control);
  CHECK(fixture_closed==1u && fixture_drives[1]==1u && !fixture_handoffs);
  CHECK(!fixture_parent->scope_mode && !fixture_parent->scope_token.driver);
  CHECK(fixture_parent->frame.header.status==KU_TASK_FRAME_EXIT_STAGED
      && fixture_parent->frame.header.result_initialized && !fixture_frees[0] && !fixture_frees[1]);
  CHECK(receipt==&fixture_parent->receipts[0] && deadline!=UINT64_MAX);
  uint32_t status=fixture_real_owner_drop_receipt(ticket,owner,deadline,receipt);
  CHECK(status==KU_TASK_DRIVER_OK && !owner->lease.control);
  fixture_child_receipt=*receipt; fixture_child_deadline=deadline; fixture_handoffs=1u;
  return status;
}
static KuTaskDriverSnapshotV1 fixture_idle(KuTaskDriverV1* driver) {
  CHECK(ku_task_driver_wait_idle(driver,ku_task_driver_now_ms()+2000u)==KU_TASK_DRIVER_OK);
  KuTaskDriverSnapshotV1 snapshot={0};
  CHECK(ku_task_driver_snapshot(driver,&snapshot)==KU_TASK_DRIVER_OK);
  CHECK(!snapshot.fault && !snapshot.running && !snapshot.queued && !snapshot.building);
  CHECK(snapshot.worker_waiting || snapshot.worker_exited);
  return snapshot;
}
static void fixture_pending(KuTaskValueV1* root) {
  KuTaskAdapterOutcomeV1 pending={0};
  CHECK(ku_task_value_take(root,NULL,&pending)==KU_TASK_CONTROL_PENDING);
  CHECK(ku_task_outcome_empty(&pending,4u));
  fixture_idle(root->ticket.driver);
}
int main(void) {
  CHECK(KU_TASK_FRAME_ABI_VERSION==4u && KU_TASK_CONTROL_ABI_VERSION==2u && KU_TASK_DRIVER_ABI_VERSION==6u);
  ku_task_control_atomic_init(&fixture_release_parent,0);
  ku_task_control_atomic_init(&fixture_release_child,0);
  CHECK(ku_test_event_init(&fixture_closed_event) && ku_test_event_init(&fixture_child_cleanup_event));
  KuTaskDriverV1* driver=(KuTaskDriverV1*)calloc(1,sizeof(*driver));
  KuTaskDriverSlotV1* slots=(KuTaskDriverSlotV1*)calloc(2,sizeof(*slots));
  size_t* ring=(size_t*)calloc(2,sizeof(*ring)); CHECK(driver && slots && ring);
  size_t fixed=sizeof(*driver)+2u*(sizeof(*slots)+sizeof(*ring));
  CHECK(ku_task_driver_init(driver,sizeof(*driver),KU_TASK_DRIVER_ABI_VERSION,slots,2,ring,2,
      fixed+sizeof(KuTaskInstance_0)+sizeof(KuTaskInstance_1)+256u)==KU_TASK_DRIVER_OK);
  KuResult_str input={0}; input.ok=true; input.value=fixture_owned(0u,"returned");
  KuString held=fixture_owned(1u,"remaining");
  KuTaskValueV1 root={0};
  CHECK(ku_task_0_start_value(driver,&input,&held,&root)==KU_TASK_DRIVER_OK && root.tag==KU_TASK_VALUE_LIVE);
  CHECK(fixture_empty_result(input) && fixture_empty_string(held));
  CHECK(ku_test_event_wait(&fixture_closed_event,2000)); fixture_idle(driver);
  KuTaskControlV1* control=root.owner.lease.control;
  CHECK(fixture_parent==(KuTaskInstance_0*)control && fixture_drives[0]==1u && !fixture_drives[1]);
  CHECK(!fixture_handoffs && !fixture_frees[0] && !fixture_frees[1]);
  fixture_no_scope_budget(fixture_parent);
#if FIXTURE_ROOT_TASK
  CHECK(fixture_child && fixture_child_observer.control==&fixture_child->control);
  CHECK(ku_task_control_atomic_load(&fixture_child->control.phase)==KU_TASK_CONTROL_LIVE);
  CHECK(!fixture_child->control.frame_destroyed && !fixture_child_receipt.driver);
#else
  CHECK(!fixture_child && !fixture_child_observer.control);
#endif
  for (unsigned repeat=0;repeat<2u;repeat++) {
    fixture_pending(&root);
    CHECK(ku_task_driver_wake(&root.ticket)==KU_TASK_DRIVER_OK); fixture_idle(driver);
    CHECK(fixture_drives[0]==1u && !fixture_drives[1] && fixture_closed==1u && !fixture_handoffs);
    fixture_no_scope_budget(fixture_parent);
  }
  ku_task_control_atomic_store(&fixture_release_parent,1);
  CHECK(ku_task_driver_wake(&root.ticket)==KU_TASK_DRIVER_OK);
#if FIXTURE_ROOT_TASK
  CHECK(ku_test_event_wait(&fixture_child_cleanup_event,2000)); fixture_idle(driver);
  CHECK(fixture_handoffs==1u && fixture_parent->payload_initialized && !fixture_frees[0] && fixture_frees[1]==1u);
  CHECK(ku_task_control_atomic_load(&control->phase)==KU_TASK_CONTROL_LIVE);
  CHECK(ku_task_driver_cleanup_receipt_read(&fixture_child_receipt)==KU_TASK_DRIVER_PENDING);
  fixture_pending(&root); /* Final public result still waits for the ROOT child's actual ACK. */
  ku_task_control_atomic_store(&fixture_release_child,1);
  CHECK(ku_task_driver_wake(&fixture_child->ticket)==KU_TASK_DRIVER_OK);
#endif
  fixture_idle(driver);
  CHECK(fixture_drives[1]==1u && fixture_closed==1u && fixture_requests==1u);
  CHECK(ku_task_control_atomic_load(&control->phase)==KU_TASK_CONTROL_COMPLETED);
#if FIXTURE_ROOT_TASK
  CHECK(ku_task_driver_cleanup_receipt_read(&fixture_child_receipt)==KU_TASK_DRIVER_CLEANUP_ACK);
  CHECK(ku_task_control_atomic_load(&fixture_child->control.phase)==KU_TASK_CONTROL_CANCELLED);
  CHECK(fixture_child->control.frame_destroyed && !fixture_child->frame_initialized);
#else
  CHECK(!fixture_parent->drain_started && !fixture_parent->drain_deadline && !fixture_handoffs);
#endif
  KuTaskAdapterOutcomeV1 outcome={0};
  CHECK(ku_task_value_take(&root,NULL,&outcome)==KU_TASK_CONTROL_OK); fixture_idle(driver);
  CHECK(outcome.result_kind==4u && outcome.exit_class==KU_TASK_EXIT_USER_RESULT
      && !outcome.has_cleanup_deadline && !outcome.cleanup_deadline);
  CHECK(outcome.value.string.ok && (uintptr_t)outcome.value.string.value.ptr==fixture_ids[0]);
  CHECK(control->frame_destroyed && !fixture_parent->frame_initialized && !fixture_parent->payload_initialized);
  CHECK(fixture_parent->frame.header.status==KU_TASK_FRAME_DESTROYED && !fixture_parent->frame.header.initialized);
  CHECK(!fixture_parent->frame.header.result_initialized && fixture_empty_result(fixture_parent->frame.result));
  CHECK(fixture_empty_result(fixture_parent->payload) && !fixture_frees[0] && fixture_frees[1]==1u);
  ku_task_outcome_drop(&outcome); CHECK(ku_task_outcome_empty(&outcome,4u));
  CHECK(fixture_frees[0]==1u && fixture_frees[1]==1u);
  if (fixture_child_observer.control) CHECK(ku_task_control_lease_release(&fixture_child_observer)==KU_TASK_CONTROL_OK);
  uint64_t deadline=ku_task_driver_now_ms()+2000u;
  CHECK(ku_task_value_drop(&root,deadline)==KU_TASK_DRIVER_OK);
  CHECK(ku_task_driver_shutdown(driver,deadline)==KU_TASK_DRIVER_OK);
  KuTaskDriverSnapshotV1 empty=fixture_idle(driver);
  CHECK(!empty.resident && !empty.reserved_bytes && !empty.running && !empty.queued && !empty.building);
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
  CHECK(ku_test_event_destroy(&fixture_closed_event) && ku_test_event_destroy(&fixture_child_cleanup_event));
  puts("empty-scope-adapter-ok"); return 0;
}
"#;
