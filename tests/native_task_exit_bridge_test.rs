//! R5h.3a real generated Exit bridge, not source scope/control-flow admission.
//! Dynamic Owned headers enter through lawful typed factories. Callback gates
//! only postpone progress; real frame/driver code does every move/drop/ACK.
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

fn frames() -> TaskProgram {
    let mut old_cleanup = vec![
        TaskOp::Init {
            dst: SlotId(9),
            value: TaskConstant::Str("BAD-old-cleanup".into()),
        },
        TaskOp::Print {
            value: SlotId(9),
            newline: true,
        },
        TaskOp::Drop { slot: SlotId(9) },
    ];
    for index in (0..4).rev() {
        old_cleanup.push(TaskOp::DropIfInit {
            slot: SlotId(index),
        });
    }
    TaskProgram {
        functions: vec![
            TaskFunction {
                id: TaskFunctionId(0),
                name: "ExitParent".into(),
                slots: vec![
                    value(IrType::Str),
                    value(IrType::Str),
                    value(IrType::Str),
                    value(result(IrType::Str)),
                    task(),
                    task(),
                    value(IrType::Str),
                    value(result(IrType::Str)),
                    value(result(IrType::Str)),
                    value(IrType::Str),
                ],
                parameters: vec![SlotId(0), SlotId(1), SlotId(2), SlotId(3)],
                entry: StateId(0),
                result: result(IrType::Str),
                states: vec![
                    state(
                        vec![],
                        TaskTerminator::Suspend {
                            resume: StateId(1),
                            cleanup: StateId(4),
                        },
                    ),
                    state(
                        vec![
                            TaskOp::Start {
                                dst: SlotId(4),
                                function: TaskFunctionId(1),
                                arguments: vec![SlotId(0)],
                            },
                            TaskOp::Start {
                                dst: SlotId(5),
                                function: TaskFunctionId(1),
                                arguments: vec![SlotId(1)],
                            },
                        ],
                        TaskTerminator::TryResult {
                            src: SlotId(3),
                            ok_value: SlotId(6),
                            err_result: SlotId(7),
                            ok: StateId(2),
                            err: StateId(3),
                        },
                    ),
                    state(
                        vec![TaskOp::WrapOk {
                            dst: SlotId(8),
                            src: SlotId(6),
                        }],
                        TaskTerminator::Exit { value: SlotId(8) },
                    ),
                    state(vec![], TaskTerminator::Exit { value: SlotId(7) }),
                    state(old_cleanup, TaskTerminator::Terminate),
                ],
            },
            TaskFunction {
                id: TaskFunctionId(1),
                name: "ExitChild".into(),
                slots: vec![value(IrType::Str), value(result(IrType::Null))],
                parameters: vec![SlotId(0)],
                entry: StateId(0),
                result: result(IrType::Null),
                states: vec![state(
                    vec![
                        TaskOp::Drop { slot: SlotId(0) },
                        TaskOp::Init {
                            dst: SlotId(1),
                            value: TaskConstant::Ok(Box::new(TaskConstant::Null)),
                        },
                    ],
                    TaskTerminator::Complete { value: SlotId(1) },
                )],
            },
            TaskFunction {
                id: TaskFunctionId(2),
                name: "ExitWithoutTasks".into(),
                slots: vec![
                    value(IrType::Str),
                    value(result(IrType::Str)),
                    value(IrType::Str),
                ],
                parameters: vec![SlotId(0), SlotId(1)],
                entry: StateId(0),
                result: result(IrType::Str),
                states: vec![state(
                    vec![TaskOp::Move {
                        dst: SlotId(2),
                        src: SlotId(0),
                    }],
                    TaskTerminator::Exit { value: SlotId(1) },
                )],
            },
        ],
    }
}

fn replace_once(source: String, anchor: &str, replacement: &str) -> String {
    assert_eq!(source.matches(anchor).count(), 1, "Exit hook: {anchor}");
    source.replacen(anchor, replacement, 1)
}

#[test]
fn native_task_exit_bridge_transfers_before_values_and_cancels_staged_result_in_c() {
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
    assert!(plan.functions[0].exit_bridge && plan.functions[0].hosted);
    assert_eq!(plan.functions[0].scope_task_mask, (1 << 4) | (1 << 5));
    assert!(plan.functions[2].exit_bridge && plan.functions[2].hosted);
    assert_eq!(plan.functions[2].scope_task_mask, 0);
    assert_eq!(
        plan.functions[2].slots,
        vec![SlotId(0), SlotId(1), SlotId(2)]
    );
    let generated = c::generate_task_frame_c_source(&sync, &tasks).unwrap();
    assert!(generated.contains("#define KU_TASK_FRAME_ABI_VERSION 4u"));
    assert!(generated.contains("#define KU_TASK_DRIVER_ABI_VERSION 7u"));
    assert!(generated.contains("KU_TASK_FRAME_EXIT_STAGED = 11u"));
    for forbidden in ["run_source", "const SOURCE"] {
        assert!(!generated.contains(forbidden));
    }
    let ledger = replace_once(
        LOCKED_ALLOCATIONS.to_owned(),
        "ku_perf_free(p);",
        "fixture_record_free(p); ku_perf_free(p);",
    );
    let instrumentation = format!(
        "{NATIVE_THREAD_LIFECYCLE_HARNESS}\n{LEDGER_LOCK}\n{ALLOCATION_HOOK}\nstatic void fixture_record_free(void*);\n{ledger}\nstatic int fixture_stage(unsigned,void*,const void*,uint32_t);\nstatic void fixture_drive(unsigned,int);\nstatic uint32_t fixture_child_hold(void*,int);\nstatic void fixture_terminate(void*,uint32_t);\n"
    );
    let mut generated = replace_once(
        generated,
        "typedef struct KuString {",
        &format!("{instrumentation}typedef struct KuString {{"),
    );
    generated = replace_once(
        generated,
        "static uint64_t ku_task_driver_now_ms(void) {",
        &format!("{CLOCK_HOOK}\nstatic uint64_t fixture_original_now(void) {{"),
    );
    for id in [0, 2] {
        let resume = format!(
            "status = ku_task_frame_{id}_resume(&instance->frame, sizeof(instance->frame), KU_TASK_FRAME_ABI_VERSION, &clock);"
        );
        generated = replace_once(
            generated,
            &resume,
            &format!(
                "{resume}\n    if (fixture_stage({id}u,instance,&clock,status)) return KU_TASK_CONTROL_PENDING;"
            ),
        );
        let drive = format!(
            "static uint32_t ku_task_frame_{id}_drive(KuTaskFrame_{id}* frame, const KuTaskFrameClockV1* clock, int cleanup) {{"
        );
        generated = replace_once(
            generated,
            &drive,
            &format!("{drive}\n  fixture_drive({id}u,cleanup);"),
        );
    }
    let terminate = "static uint32_t ku_task_frame_0_terminate(void* storage, size_t bytes, uint32_t abi, uint32_t reason, uint64_t absolute_cleanup_deadline_ms, const KuTaskFrameClockV1* clock) {";
    generated = replace_once(
        generated,
        terminate,
        &format!("{terminate}\n  fixture_terminate(storage,reason);"),
    );
    for (entry, cleanup) in [
        ("static uint32_t ku_task_1_resume(void* raw) {", 0),
        (
            "static uint32_t ku_task_1_cleanup(void* raw, uint32_t reason, KuTaskControlV1* budget) {",
            1,
        ),
    ] {
        generated = replace_once(
            generated,
            entry,
            &format!(
                "{entry}\n  uint32_t held=fixture_child_hold(raw,{cleanup});\n  if (held!=UINT32_MAX) return held;"
            ),
        );
    }
    generated = replace_once(
        generated,
        "static uint32_t ku_task_driver_owner_drop_receipt(\n",
        "static uint32_t ku_task_driver_owner_drop_receipt(const KuTaskDriverTicketV1*,KuTaskControlOwnerV1*,uint64_t,KuTaskDriverCleanupReceiptV1*);\nstatic uint32_t fixture_real_owner_drop_receipt(\n",
    );
    generated = replace_once(
        generated,
        "int main(void) {",
        "static int fixture_original_main(void) {",
    );
    generated.push_str(C_MAIN);
    let directory = TempDir::new("native-task-exit-bridge");
    let path = directory.path().join("program.c");
    fs::write(&path, generated).unwrap();
    let Some(executable) = compile_harness(directory.path(), &path, "program") else {
        assert!(
            std::env::var_os("GITHUB_ACTIONS").is_none(),
            "CI must execute the generated Exit bridge"
        );
        return;
    };
    fs::remove_file(path).unwrap();
    let output = run_bounded(
        Command::new(executable).current_dir(directory.path()),
        RUN_TIMEOUT,
        RUN_LIMITS,
    )
    .expect("Exit bridge must stop under the existing real process watchdog");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8(output.stdout).unwrap().replace('\r', ""),
        "task-exit-bridge-ok\n"
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
static unsigned fixture_mode, fixture_stage_seen, fixture_transfers, fixture_terminate_staged;
static unsigned fixture_drive_calls[3], fixture_cleanup_drives;
static uintptr_t fixture_ids[6];
static unsigned fixture_frees[6];
static KuAtomicRefcount fixture_release_children;
static KuTestEvent fixture_staged;
static KuTaskDriverTicketV1 fixture_children[2];
static KuTaskControlLeaseV1 fixture_observers[2];
static KuTaskDriverCleanupReceiptV1 fixture_receipts[2];
static uint64_t fixture_child_deadline[2];

static int fixture_empty_string(KuString value) {
  return !value.ptr && !value.len && !value.capacity && !value.storage;
}
static int fixture_empty_result(KuResult_str value) {
  return !value.ok && fixture_empty_string(value.value)
      && fixture_empty_string(value.error.domain) && fixture_empty_string(value.error.code)
      && fixture_empty_string(value.error.message);
}
static void fixture_check_payload(KuResult_str value) {
  if (fixture_mode==1u || fixture_mode==2u) {
    CHECK(!value.ok && fixture_empty_string(value.value));
    CHECK((uintptr_t)value.error.domain.ptr==fixture_ids[3] && value.error.domain.len==6u);
    CHECK((uintptr_t)value.error.code.ptr==fixture_ids[4] && value.error.code.len==4u);
    CHECK((uintptr_t)value.error.message.ptr==fixture_ids[5] && value.error.message.len==7u);
    CHECK(!memcmp(value.error.domain.ptr,"domain",6u));
    CHECK(!memcmp(value.error.code.ptr,"code",4u));
    CHECK(!memcmp(value.error.message.ptr,"message",7u));
  } else {
    CHECK(value.ok && (uintptr_t)value.value.ptr==fixture_ids[3] && value.value.len==7u);
    CHECK(!memcmp(value.value.ptr,"payload",7u));
    CHECK(fixture_empty_string(value.error.domain) && fixture_empty_string(value.error.code)
        && fixture_empty_string(value.error.message));
  }
}
static void fixture_record_free(void* pointer) {
  /* Called with the shared allocation ledger lock held, before the real free.
   * Integer identities remain legal to compare after their allocations die. */
  if (!pointer) return;
  uintptr_t identity=(uintptr_t)pointer;
  for (size_t i=0;i<6u;i++) if (fixture_ids[i] && identity==fixture_ids[i]) {
    CHECK(!fixture_frees[i]++);
    if (i>=2u) CHECK(fixture_stage_seen==1u && fixture_transfers==(fixture_mode==3u ? 0u : 2u));
  }
}
static KuString fixture_owned(unsigned id,size_t capacity,const char* text) {
  size_t length=strlen(text); CHECK(id<6u && !fixture_ids[id] && length<=capacity);
  uint8_t* pointer=(uint8_t*)malloc(capacity); CHECK(pointer);
  memcpy(pointer,text,length); fixture_ids[id]=(uintptr_t)pointer;
  KuString value={pointer,length,capacity,KU_STRING_OWNED}; return value;
}
static void fixture_drive(unsigned id,int cleanup) {
  CHECK(id<3u);
  if (cleanup) { fixture_cleanup_drives++; CHECK(0 && "staged exit must not replay old cleanup CFG"); }
  fixture_drive_calls[id]++;
}
static void fixture_terminate(void* raw,uint32_t reason) {
  KuTaskFrame_0* frame=(KuTaskFrame_0*)raw;
  CHECK(fixture_mode==2u && reason==KU_TASK_FRAME_CANCELLED && !fixture_terminate_staged++);
  CHECK(frame->header.status==KU_TASK_FRAME_EXIT_STAGED && frame->header.result_initialized==1u);
  CHECK(frame->header.cleanup_state==UINT32_MAX && !(frame->header.initialized&UINT64_C(48)));
  CHECK(fixture_transfers==2u);
}
static int fixture_stage(unsigned id,void* raw,const void* raw_clock,uint32_t status) {
  if (status!=KU_TASK_FRAME_EXIT_STAGED || fixture_stage_seen) return 0;
  const KuTaskFrameClockV1* clock=(const KuTaskFrameClockV1*)raw_clock;
  CHECK(!fixture_transfers && !fixture_cleanup_drives);
  KuResult_str output={0};
  if (id==0u) {
    KuTaskInstance_0* instance=(KuTaskInstance_0*)raw;
    CHECK(fixture_mode<3u && fixture_drive_calls[0]==2u);
    CHECK(!instance->payload_initialized && !instance->drain_started);
    CHECK(instance->frame.header.status==KU_TASK_FRAME_EXIT_STAGED);
    CHECK(instance->frame.header.result_initialized==1u && !instance->frame.header.running);
    CHECK(instance->frame.header.cleanup_state==UINT32_MAX);
    CHECK(instance->frame.header.exit_class==KU_TASK_EXIT_USER_RESULT);
    CHECK(!instance->frame.header.has_exit_deadline && !instance->frame.header.exit_deadline);
    CHECK(instance->frame.header.initialized==UINT64_C(52));
    CHECK(fixture_empty_string(instance->frame.s_0) && fixture_empty_string(instance->frame.s_1));
    CHECK(fixture_empty_result(instance->frame.s_3) && fixture_empty_string(instance->frame.s_6));
    CHECK(fixture_empty_result(instance->frame.s_7) && fixture_empty_result(instance->frame.s_8));
    CHECK((uintptr_t)instance->frame.s_2.ptr==fixture_ids[2]);
    fixture_check_payload(instance->frame.result);
    unsigned char before[sizeof(instance->frame)]; memcpy(before,&instance->frame,sizeof(before));
    CHECK(ku_task_frame_0_resume(&instance->frame,sizeof(instance->frame),KU_TASK_FRAME_ABI_VERSION,clock)==KU_TASK_FRAME_EXIT_STAGED);
    CHECK(ku_task_frame_0_take_result(&instance->frame,sizeof(instance->frame),KU_TASK_FRAME_ABI_VERSION,&output)==KU_TASK_FRAME_INVALID_STATE);
    CHECK(ku_task_frame_0_destroy(&instance->frame,sizeof(instance->frame),KU_TASK_FRAME_ABI_VERSION)==KU_TASK_FRAME_INVALID_STATE);
    CHECK(ku_task_frame_0_finish_exit_values(&instance->frame,sizeof(instance->frame),KU_TASK_FRAME_ABI_VERSION)==KU_TASK_FRAME_INVALID_STATE);
    CHECK(!memcmp(before,&instance->frame,sizeof(before)) && fixture_empty_result(output));
    CHECK(fixture_drive_calls[0]==2u);
    KuTaskValueV1* children[2]={&instance->frame.s_4,&instance->frame.s_5};
    for (size_t i=0;i<2u;i++) {
      CHECK(children[i]->tag==KU_TASK_VALUE_LIVE);
      fixture_children[i]=children[i]->ticket;
      CHECK(ku_task_control_lease_retain(&children[i]->owner.lease,&fixture_observers[i])==KU_TASK_CONTROL_OK);
    }
    CHECK(ku_task_driver_set_intent(&instance->ticket,KU_TASK_DRIVER_WAIT)==KU_TASK_DRIVER_OK);
  } else {
    KuTaskInstance_2* instance=(KuTaskInstance_2*)raw;
    CHECK(id==2u && fixture_mode==3u && fixture_drive_calls[2]==1u);
    CHECK(!instance->payload_initialized && !instance->drain_started);
    CHECK(instance->frame.header.status==KU_TASK_FRAME_EXIT_STAGED);
    CHECK(instance->frame.header.result_initialized==1u && !instance->frame.header.running);
    CHECK(instance->frame.header.initialized==(UINT64_C(1)<<2));
    CHECK(fixture_empty_string(instance->frame.s_0) && fixture_empty_result(instance->frame.s_1));
    CHECK((uintptr_t)instance->frame.s_2.ptr==fixture_ids[2] && instance->frame.s_2.storage==KU_STRING_OWNED);
    fixture_check_payload(instance->frame.result);
    unsigned char before[sizeof(instance->frame)]; memcpy(before,&instance->frame,sizeof(before));
    CHECK(ku_task_frame_2_resume(&instance->frame,sizeof(instance->frame),KU_TASK_FRAME_ABI_VERSION,clock)==KU_TASK_FRAME_EXIT_STAGED);
    CHECK(ku_task_frame_2_take_result(&instance->frame,sizeof(instance->frame),KU_TASK_FRAME_ABI_VERSION,&output)==KU_TASK_FRAME_INVALID_STATE);
    CHECK(ku_task_frame_2_destroy(&instance->frame,sizeof(instance->frame),KU_TASK_FRAME_ABI_VERSION)==KU_TASK_FRAME_INVALID_STATE);
    CHECK(!memcmp(before,&instance->frame,sizeof(before)) && fixture_empty_result(output));
    CHECK(fixture_drive_calls[2]==1u);
    CHECK(ku_task_driver_set_intent(&instance->ticket,KU_TASK_DRIVER_WAIT)==KU_TASK_DRIVER_OK);
  }
  for (size_t i=0;i<6u;i++) CHECK(!fixture_frees[i]);
  fixture_stage_seen=1u;
  CHECK(ku_test_event_set(&fixture_staged));
  /* Stop one genuine callback turn after real frame staging. No frame status,
   * bitmap, output or ACK is invented. The next real wake/cancel resumes it. */
  return 1;
}
static unsigned fixture_child_index(const KuTaskDriverTicketV1* ticket) {
  for (unsigned i=0;i<2u;i++) if (ticket->driver==fixture_children[i].driver
      && ticket->slot==fixture_children[i].slot && ticket->generation==fixture_children[i].generation) return i;
  CHECK(0 && "unknown child identity"); return 0;
}
static uint32_t fixture_child_hold(void* raw,int cleanup) {
  KuTaskInstance_1* instance=(KuTaskInstance_1*)raw;
  unsigned index=fixture_child_index(&instance->ticket);
  if (cleanup && (ku_task_control_atomic_load(&fixture_release_children)&((size_t)1u<<index))) {
    CHECK(ku_task_control_cleanup_deadline(&instance->control)==fixture_child_deadline[index]);
    return UINT32_MAX; /* Fall through into the original generated cleanup. */
  }
  /* Before parent handoff, hold ordinary child callbacks too: the sole worker
   * remains free to run the staged parent. After handoff this gate holds only
   * cleanup; real owner cancellation supplies its wake and first winner. */
  CHECK(ku_task_driver_set_intent(&instance->ticket,KU_TASK_DRIVER_WAIT)==KU_TASK_DRIVER_OK);
  return KU_TASK_CONTROL_PENDING;
}
static uint32_t ku_task_driver_owner_drop_receipt(const KuTaskDriverTicketV1* ticket,
    KuTaskControlOwnerV1* owner,uint64_t deadline,KuTaskDriverCleanupReceiptV1* receipt) {
  unsigned index=fixture_child_index(ticket);
  CHECK(fixture_stage_seen==1u && !fixture_receipts[index].driver);
  uint32_t status=fixture_real_owner_drop_receipt(ticket,owner,deadline,receipt);
  CHECK(status==KU_TASK_DRIVER_OK && receipt->driver==ticket->driver
      && receipt->slot==ticket->slot && receipt->generation==ticket->generation);
  CHECK(!owner->lease.control);
  fixture_receipts[index]=*receipt; fixture_child_deadline[index]=deadline;
  fixture_alloc_lock(); fixture_transfers++; fixture_alloc_unlock();
  return status;
}
static KuTaskDriverSnapshotV1 fixture_idle(KuTaskDriverV1* driver) {
  CHECK(ku_task_driver_wait_idle(driver,ku_task_driver_now_ms()+2000u)==KU_TASK_DRIVER_OK);
  KuTaskDriverSnapshotV1 snapshot={0};
  CHECK(ku_task_driver_snapshot(driver,&snapshot)==KU_TASK_DRIVER_OK);
  CHECK(!snapshot.fault && !snapshot.running && !snapshot.queued && !snapshot.building);
  CHECK(snapshot.worker_target==1u && snapshot.workers_created==1u);
  CHECK(snapshot.workers_waiting+snapshot.workers_exited==snapshot.workers_created);
  CHECK(!ku_task_driver_lock(driver));
  CHECK(!driver->clock_fault);
  size_t bytes=0;
  for (size_t i=0;i<driver->capacity;i++) {
    CHECK(driver->slots[i].state!=KU_TASK_DRIVER_FAULTED);
    CHECK(driver->slots[i].charged_bytes<=SIZE_MAX-bytes); bytes+=driver->slots[i].charged_bytes;
  }
  CHECK(bytes==driver->reserved_bytes);
  CHECK(!ku_task_driver_unlock(driver));
  return snapshot;
}
static void fixture_check_frees(int payload_freed,unsigned child_mask) {
  fixture_alloc_lock();
  CHECK(fixture_frees[2]==1u);
  for (size_t i=3u;i<6u;i++) if (fixture_ids[i]) CHECK(fixture_frees[i]==(unsigned)payload_freed);
  if (fixture_mode<3u) for (unsigned i=0;i<2u;i++) CHECK(fixture_frees[i]==((child_mask>>i)&1u));
  fixture_alloc_unlock();
}
static void fixture_case(unsigned mode) {
  CHECK(!fixture_ledger().allocations && !fixture_ledger().bytes);
  fixture_mode=mode; fixture_stage_seen=fixture_transfers=fixture_terminate_staged=fixture_cleanup_drives=0;
  memset(fixture_ids,0,sizeof(fixture_ids)); memset(fixture_frees,0,sizeof(fixture_frees));
  memset(fixture_drive_calls,0,sizeof(fixture_drive_calls));
  memset(fixture_children,0,sizeof(fixture_children)); memset(fixture_observers,0,sizeof(fixture_observers));
  memset(fixture_receipts,0,sizeof(fixture_receipts)); memset(fixture_child_deadline,0,sizeof(fixture_child_deadline));
  ku_task_control_atomic_store(&fixture_release_children,0);
  CHECK(ku_test_event_init(&fixture_staged));
  ku_task_control_deadline_store(&fixture_clock,ku_test_real_now_ms()+100000u);
  uint64_t cleanup_now=ku_task_driver_now_ms();
  CHECK(cleanup_now!=UINT64_MAX && cleanup_now<UINT64_MAX-1000u);
  uint64_t deadline=cleanup_now+(mode==2u ? 500u : 1000u);
  KuTaskDriverV1* driver=(KuTaskDriverV1*)calloc(1,sizeof(*driver));
  KuTaskDriverSlotV1* slots=(KuTaskDriverSlotV1*)calloc(3,sizeof(*slots));
  size_t* ring=(size_t*)calloc(3,sizeof(*ring)); CHECK(driver && slots && ring);
  size_t fixed=sizeof(*driver)+3u*(sizeof(*slots)+sizeof(*ring));
  size_t instances=sizeof(KuTaskInstance_0)+2u*sizeof(KuTaskInstance_1)+sizeof(KuTaskInstance_2);
  uint64_t startup_now=ku_task_driver_now_ms();
  CHECK(startup_now!=UINT64_MAX && startup_now<UINT64_MAX-1000u);
  uint64_t startup_deadline=startup_now+1000u;
  CHECK(ku_task_driver_init(driver,sizeof(*driver),6u,
      slots,3,ring,3,fixed+instances+256u,1u,startup_deadline)==KU_TASK_DRIVER_ABI_MISMATCH);
  CHECK(ku_task_frame_zero_bytes(driver,sizeof(*driver))
      && ku_task_frame_zero_bytes(slots,(3)*sizeof(*slots))
      && ku_task_frame_zero_bytes(ring,(3)*sizeof(*ring)));
  CHECK(ku_task_driver_init(driver,sizeof(*driver),KU_TASK_DRIVER_ABI_VERSION,
      slots,3,ring,3,fixed+instances+256u,1u,startup_deadline)==KU_TASK_DRIVER_OK);
  KuString first={0},second={0};
  if (mode<3u) { first=fixture_owned(0u,11u,"first"); second=fixture_owned(1u,13u,"second"); }
  KuString remaining=fixture_owned(2u,17u,"remaining");
  KuResult_str returned={0};
  if (mode==1u || mode==2u) {
    returned.error.domain=fixture_owned(3u,19u,"domain");
    returned.error.code=fixture_owned(4u,23u,"code");
    returned.error.message=fixture_owned(5u,29u,"message");
  } else { returned.ok=true; returned.value=fixture_owned(3u,19u,"payload"); }
  KuTaskValueV1 root={0};
  uint32_t started=mode==3u ? ku_task_2_start_value(driver,&remaining,&returned,&root)
      : ku_task_0_start_value(driver,&first,&second,&remaining,&returned,&root);
  CHECK(started==KU_TASK_DRIVER_OK && root.tag==KU_TASK_VALUE_LIVE);
  CHECK(fixture_empty_string(first) && fixture_empty_string(second));
  CHECK(fixture_empty_string(remaining) && fixture_empty_result(returned));
  CHECK(ku_test_event_wait(&fixture_staged,2000));
  fixture_idle(driver); /* The actual callback has returned; no concurrent frame writer. */
  KuTaskControlV1* control=root.owner.lease.control;
  CHECK(ku_task_control_atomic_load(&control->phase)==KU_TASK_CONTROL_LIVE);
  CHECK(ku_task_control_atomic_load(&control->payload)==KU_TASK_CONTROL_PAYLOAD_EMPTY);
  CHECK(fixture_stage_seen==1u && !fixture_transfers);
  for (size_t i=0;i<6u;i++) CHECK(!fixture_frees[i]);
  if (mode==2u) {
    CHECK(ku_task_driver_request_cancel(&root.ticket,&root.owner.lease,KU_TASK_CONTROL_CANCELLED,deadline)==KU_TASK_CONTROL_OK);
    CHECK(ku_task_driver_request_cancel(&root.ticket,&root.owner.lease,KU_TASK_CONTROL_TIMED_OUT,deadline+100u)==KU_TASK_CONTROL_OK);
  } else CHECK(ku_task_driver_wake(&root.ticket)==KU_TASK_DRIVER_OK);
  fixture_idle(driver);
  CHECK(fixture_stage_seen==1u && !fixture_cleanup_drives);
  if (mode<3u) {
    KuTaskInstance_0* instance=(KuTaskInstance_0*)control;
    CHECK(fixture_drive_calls[0]==2u && fixture_transfers==2u);
    CHECK(instance->drain_started && instance->drain_deadline==deadline);
    CHECK(!(instance->frame.header.initialized&UINT64_C(48)) && fixture_empty_string(instance->frame.s_2));
    CHECK(fixture_terminate_staged==(mode==2u ? 1u : 0u));
    CHECK(ku_task_control_atomic_load(&control->payload)==KU_TASK_CONTROL_PAYLOAD_EMPTY);
    if (mode==2u) {
      CHECK(!instance->payload_initialized && !instance->frame.header.result_initialized);
      CHECK(instance->frame.header.status==KU_TASK_FRAME_CANCELLED);
      CHECK(ku_task_control_atomic_load(&control->phase)==KU_TASK_CONTROL_REQUESTED_CANCEL);
    } else {
      CHECK(instance->payload_initialized && instance->frame.header.status==KU_TASK_FRAME_READY);
      fixture_check_payload(instance->payload);
      KuTaskAdapterOutcomeV1 pending={0};
      CHECK(ku_task_value_take(&root,NULL,&pending)==KU_TASK_CONTROL_PENDING);
      CHECK(ku_task_outcome_empty(&pending,4u));
      /* Even Pending take sends a real wrapper notification. Drain it before
       * inspecting a still-private frame/payload or establishing a poll baseline. */
      fixture_idle(driver);
      CHECK(instance->payload_initialized && fixture_transfers==2u && fixture_drive_calls[0]==2u);
    }
    fixture_check_frees(mode==2u,0u);
    for (unsigned i=0;i<2u;i++) {
      CHECK(ku_task_driver_cleanup_receipt_read(&fixture_receipts[i])==KU_TASK_DRIVER_PENDING);
      CHECK(fixture_child_deadline[i]==deadline);
      CHECK(ku_task_control_cleanup_deadline(fixture_observers[i].control)==deadline);
    }
    ku_task_control_atomic_store(&fixture_release_children,1u);
    CHECK(ku_task_driver_wake(&fixture_children[0])==KU_TASK_DRIVER_OK);
    fixture_idle(driver);
    CHECK(ku_task_driver_cleanup_receipt_read(&fixture_receipts[0])==KU_TASK_DRIVER_CLEANUP_ACK);
    CHECK(ku_task_driver_cleanup_receipt_read(&fixture_receipts[1])==KU_TASK_DRIVER_PENDING);
    CHECK(ku_task_control_atomic_load(&control->payload)==KU_TASK_CONTROL_PAYLOAD_EMPTY);
    fixture_check_frees(mode==2u,1u);
    ku_task_control_atomic_store(&fixture_release_children,3u);
    CHECK(ku_task_driver_wake(&fixture_children[1])==KU_TASK_DRIVER_OK);
    fixture_idle(driver);
    CHECK(ku_task_driver_cleanup_receipt_read(&fixture_receipts[1])==KU_TASK_DRIVER_CLEANUP_ACK);
    fixture_check_frees(mode==2u,3u);
  } else {
    KuTaskInstance_2* instance=(KuTaskInstance_2*)control;
    CHECK(fixture_drive_calls[2]==1u && !fixture_transfers && !fixture_terminate_staged);
    CHECK(!instance->drain_started && fixture_empty_string(instance->frame.s_2));
    fixture_check_frees(0,0u);
  }
  uint32_t terminal=mode==2u ? KU_TASK_CONTROL_CANCELLED
      : mode==1u ? KU_TASK_CONTROL_FAILED : KU_TASK_CONTROL_COMPLETED;
  CHECK(ku_task_control_atomic_load(&control->phase)==terminal);
  CHECK(control->frame_destroyed);
  KuTaskDriverSnapshotV1 before=fixture_idle(driver);
  CHECK(ku_task_driver_wake(&root.ticket)==KU_TASK_DRIVER_OK);
  CHECK(fixture_idle(driver).polls==before.polls+1u);
  CHECK(fixture_stage_seen==1u && !fixture_cleanup_drives);
  CHECK(mode==3u ? fixture_drive_calls[2]==1u : fixture_drive_calls[0]==2u);
  CHECK(fixture_transfers==(mode==3u ? 0u : 2u));
  KuTaskAdapterOutcomeV1 outcome={0};
  uint32_t taken=ku_task_value_take(&root,NULL,&outcome);
  if (mode==2u) CHECK(taken==KU_TASK_CONTROL_CANCELLED && ku_task_outcome_empty(&outcome,4u));
  else {
    CHECK(taken==KU_TASK_CONTROL_OK && outcome.result_kind==4u);
    CHECK(outcome.exit_class==KU_TASK_EXIT_USER_RESULT && !outcome.has_cleanup_deadline && !outcome.cleanup_deadline);
    fixture_check_payload(outcome.value.string);
  }
  fixture_idle(driver);
  ku_task_outcome_drop(&outcome);
  fixture_check_frees(1,mode==3u ? 0u : 3u);
  CHECK(ku_task_value_drop(&root,deadline)==KU_TASK_DRIVER_OK);
  for (size_t i=0;i<2u;i++) if (fixture_observers[i].control)
    CHECK(ku_task_control_lease_release(&fixture_observers[i])==KU_TASK_CONTROL_OK);
  CHECK(ku_task_driver_shutdown(driver,deadline)==KU_TASK_DRIVER_OK);
  KuTaskDriverSnapshotV1 empty=fixture_idle(driver);
  CHECK(!empty.resident && !empty.reserved_bytes && !empty.queued && !empty.running && !empty.building);
  CHECK(ku_task_driver_join(driver,deadline)==KU_TASK_DRIVER_OK);
  CHECK(driver->worker_target==1u && driver->workers_created==1u && driver->workers_exited==1u
      && driver->workers_joined==1u && driver->workers[0].joined && driver->workers[0].closed);
  CHECK(ku_task_driver_destroy(driver)==KU_TASK_DRIVER_OK);
  free(ring); free(slots); free(driver);
  for (size_t i=0;i<6u;i++) if (fixture_ids[i]) CHECK(fixture_frees[i]==1u);
  CHECK(!fixture_ledger().allocations && !fixture_ledger().bytes && !fixture_ledger().overflow);
  CHECK(ku_test_event_destroy(&fixture_staged));
}
int main(void) {
  CHECK(KU_TASK_FRAME_ABI_VERSION==4u && KU_TASK_DRIVER_ABI_VERSION==7u);
  ku_task_control_deadline_init(&fixture_clock);
  ku_task_control_atomic_init(&fixture_release_children,0);
  for (unsigned mode=0;mode<4u;mode++) fixture_case(mode);
  puts("task-exit-bridge-ok"); return 0;
}
"#;
