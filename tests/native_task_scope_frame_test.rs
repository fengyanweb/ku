//! Normal-scope Frame ABI only: real verified IR and emitted C, no adapter ACK.
//! Host Owned headers are legitimate dynamic inputs. The declared Task slot is
//! never started; its isolated raw-bit negative test is not a live Task owner.
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
use native_harness::{compile_harness, run_bounded, TempDir, RUN_LIMITS, RUN_TIMEOUT};
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

fn state(operations: Vec<TaskOp>, terminator: TaskTerminator) -> TaskState {
    TaskState {
        operations,
        terminator,
    }
}

fn continuation(marker: i64, cleanup: bool) -> TaskState {
    let mut operations = vec![
        TaskOp::Print {
            value: SlotId(3),
            newline: false,
        },
        TaskOp::Init {
            dst: SlotId(4),
            value: TaskConstant::Int(marker),
        },
        TaskOp::Print {
            value: SlotId(4),
            newline: true,
        },
        TaskOp::Drop { slot: SlotId(6) },
    ];
    let terminator = if cleanup {
        operations.push(TaskOp::Drop { slot: SlotId(2) });
        TaskTerminator::Terminate
    } else {
        TaskTerminator::Exit { value: SlotId(2) }
    };
    state(operations, terminator)
}

fn frames() -> TaskProgram {
    TaskProgram {
        functions: vec![
            TaskFunction {
                id: TaskFunctionId(0),
                name: "ScopeFrame".into(),
                slots: vec![
                    value(IrType::Bool),
                    value(IrType::Str),
                    value(result(IrType::Str)),
                    value(IrType::Int),
                    value(IrType::Int),
                    TaskSlot {
                        ty: TaskSlotType::Task {
                            result: result(IrType::Null),
                        },
                    },
                    value(IrType::Str),
                ],
                parameters: vec![SlotId(0), SlotId(1), SlotId(2)],
                entry: StateId(0),
                result: result(IrType::Str),
                states: vec![
                    state(
                        vec![
                            TaskOp::Init {
                                dst: SlotId(3),
                                value: TaskConstant::Int(37),
                            },
                            TaskOp::Move {
                                dst: SlotId(6),
                                src: SlotId(1),
                            },
                            TaskOp::ScopeEnter {
                                scope: TaskScopeId(0),
                                tasks: vec![SlotId(5)],
                            },
                        ],
                        TaskTerminator::Branch {
                            condition: SlotId(0),
                            then_state: StateId(1),
                            else_state: StateId(2),
                        },
                    ),
                    state(
                        vec![],
                        TaskTerminator::ScopeDrain {
                            scope: TaskScopeId(0),
                            ready: StateId(3),
                            cleanup: StateId(5),
                        },
                    ),
                    state(
                        vec![],
                        TaskTerminator::ScopeDrain {
                            scope: TaskScopeId(0),
                            ready: StateId(4),
                            cleanup: StateId(6),
                        },
                    ),
                    continuation(101, false),
                    continuation(202, false),
                    continuation(303, true),
                    continuation(404, true),
                ],
            },
            TaskFunction {
                id: TaskFunctionId(1),
                name: "WithoutScope".into(),
                slots: vec![value(result(IrType::Null))],
                parameters: vec![],
                entry: StateId(0),
                result: result(IrType::Null),
                states: vec![state(
                    vec![TaskOp::Init {
                        dst: SlotId(0),
                        value: TaskConstant::Ok(Box::new(TaskConstant::Null)),
                    }],
                    TaskTerminator::Exit { value: SlotId(0) },
                )],
            },
        ],
    }
}

fn replace_once(source: String, anchor: &str, replacement: &str) -> String {
    assert_eq!(
        source.matches(anchor).count(),
        1,
        "scope frame hook: {anchor}"
    );
    source.replacen(anchor, replacement, 1)
}

#[test]
fn native_task_scope_frame_request_continue_timeout_and_cleanup_execute_in_c() {
    let ast = Parser::new(Lexer::new("fn main() {}").lex().unwrap())
        .parse_program()
        .unwrap();
    Checker::new().check(&ast).unwrap();
    let sync = ir::lower_program(&ast).unwrap();
    let tasks = frames();
    let plan = verify_and_plan(&tasks, TaskLimits::default()).unwrap();
    let scoped = &plan.functions[0];
    assert!(scoped.hosted && scoped.exit_bridge);
    assert_eq!(
        scoped.scopes,
        vec![TaskScopeFrame {
            scope: TaskScopeId(0),
            task_mask: 1u64 << 5
        }]
    );
    assert!(
        scoped.slots.contains(&SlotId(3)),
        "Copy is live across both request edges"
    );
    assert!(
        scoped.slots.contains(&SlotId(6)),
        "moved Owned local persists"
    );
    assert!(
        !scoped.slots.contains(&SlotId(4)),
        "post-request dead Copy does not spill"
    );
    assert_eq!(scoped.suspensions.len(), 2);
    assert!(plan.functions[1].scopes.is_empty());

    let generated = c::generate_task_frame_c_source(&sync, &tasks).unwrap();
    assert!(generated.contains("#define KU_TASK_FRAME_ABI_VERSION 4u"));
    for state in [1, 2] {
        assert!(generated.contains(&format!(
            "static const KuTaskFrameScopeDescriptorV1 ku_task_frame_0_scope_{state} ="
        )));
    }
    for name in ["lookup", "request", "continue", "timeout"] {
        assert!(!generated.contains(&format!("ku_task_frame_1_scope_{name}(")));
    }
    for forbidden in ["run_source", "const SOURCE", "Task.new", "task.spawn"] {
        assert!(!generated.contains(forbidden));
    }
    let ledger = replace_once(
        LOCKED_ALLOCATIONS.to_owned(),
        "ku_perf_free(p);",
        "fixture_record_free(p); ku_perf_free(p);",
    );
    let headers =
        "#if defined(_WIN32)\n#include <windows.h>\n#else\n#include <pthread.h>\n#endif\n";
    let instrumentation = format!(
        "{headers}{LEDGER_LOCK}\n{ALLOCATION_HOOK}\nstatic void fixture_record_free(void*);\n{ledger}\n"
    );
    let generated = replace_once(
        generated,
        "typedef struct KuString {",
        &format!("{instrumentation}typedef struct KuString {{"),
    );
    let mut generated = replace_once(
        generated,
        "int main(void) {",
        "static int fixture_unmodified_main(void) {",
    );
    generated.push_str(C_MAIN);
    let directory = TempDir::new("native-task-scope-frame");
    let path = directory.path().join("program.c");
    fs::write(&path, generated).unwrap();
    let Some(executable) = compile_harness(directory.path(), &path, "program") else {
        assert!(
            std::env::var_os("GITHUB_ACTIONS").is_none(),
            "CI must execute scope frames"
        );
        return;
    };
    fs::remove_file(path).unwrap();
    let output = run_bounded(
        Command::new(executable).current_dir(directory.path()),
        RUN_TIMEOUT,
        RUN_LIMITS,
    )
    .expect("scope Frame fixture stays under the existing real process watchdog");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8(output.stdout).unwrap().replace('\r', ""),
        "37202\n37404\n37101\n37303\nscope-frame-ok\n"
    );
    assert!(output.stderr.is_empty());
}

const C_MAIN: &str = r#"
static uintptr_t fixture_buffers[4];
static unsigned fixture_frees[4],fixture_buffer_count;
static const KuTaskFrameScopeDescriptorV1* fixture_descriptors[2];
static void fixture_record_free(void* pointer) {
  if (!pointer) return;
  size_t matches=0;
  for (size_t i=0;i<fixture_buffer_count;i++) if ((uintptr_t)pointer==fixture_buffers[i]) {
    CHECK(!fixture_frees[i]++); matches++;
  }
  CHECK(matches==1u); /* Called under the shared ledger lock, before actual free. */
}
static KuString fixture_owned(const char* text,size_t capacity) {
  size_t length=strlen(text); CHECK(length<=capacity && fixture_buffer_count<4u);
  uint8_t* pointer=(uint8_t*)malloc(capacity); CHECK(pointer);
  memcpy(pointer,text,length);
  fixture_buffers[fixture_buffer_count++]=(uintptr_t)pointer;
  return (KuString){pointer,length,capacity,KU_STRING_OWNED};
}
static void fixture_all_freed(void) {
  for (size_t i=0;i<fixture_buffer_count;i++) CHECK(fixture_frees[i]==1u);
  FixtureLedger ledger=fixture_ledger();
  CHECK(!ledger.allocations && !ledger.bytes && !ledger.overflow);
}
static void fixture_text(KuString text,const char* expected) {
  CHECK(text.len==strlen(expected) && !memcmp(text.ptr,expected,text.len));
}
static void fixture_business_result(KuResult_str* value,int error) {
  CHECK(value->ok==!error);
  if (!error) fixture_text(value->value,"value");
  else {
    fixture_text(value->error.domain,"scope");
    fixture_text(value->error.code,"bad");
    fixture_text(value->error.message,"expected");
  }
}
typedef struct FixtureHost {
  KuTaskControlV1 control;
  uint64_t now;
  unsigned samples,finished,cleanups,drops,disposed;
} FixtureHost;
static uint64_t fixture_now(void* raw) {
  FixtureHost* host=(FixtureHost*)raw; host->samples++; return host->now;
}
/* A private, unexposed real R2 control supplies the raw Frame's LIVE phase.
 * It never drives a Frame or implements scope/drop/ACK. All tested Frames are
 * actually destroyed before this phase carrier itself is cancelled/disposed. */
static uint32_t fixture_unused_resume(void* raw) {
  (void)raw; CHECK(0); return KU_TASK_CONTROL_INVALID_STATE;
}
static uint32_t fixture_host_cleanup(void* raw,uint32_t reason,KuTaskControlV1* budget) {
  FixtureHost* host=(FixtureHost*)raw;
  CHECK(host->finished && reason==KU_TASK_CONTROL_CANCELLED && budget==&host->control);
  CHECK(!host->cleanups++); return KU_TASK_CONTROL_OK;
}
static void fixture_host_drop(void* raw) {
  FixtureHost* host=(FixtureHost*)raw; CHECK(host->finished && !host->drops++);
}
static void fixture_unused_drop(void* raw) { (void)raw; CHECK(0); }
static uint32_t fixture_unused_take(void* raw,void* output) {
  (void)raw; (void)output; CHECK(0); return KU_TASK_CONTROL_INVALID_STATE;
}
static void fixture_host_dispose(KuTaskControlV1* control,void* raw) {
  FixtureHost* host=(FixtureHost*)raw;
  CHECK(control==&host->control && host->cleanups==1u && host->drops==1u && !host->disposed++);
  CHECK(!ku_task_control_atomic_load(&control->references)
      && !ku_task_control_atomic_load(&control->lifecycle_pin));
}
static const KuTaskControlOpsV1 fixture_host_ops={
  fixture_unused_resume,fixture_host_cleanup,fixture_host_drop,
  fixture_unused_drop,fixture_unused_take,fixture_host_dispose
};
static void fixture_stopped_negatives(KuTaskFrame_0* frame,const KuTaskFrameClockV1* clock,
    const KuTaskFrameScopeDescriptorV1* descriptor) {
  unsigned char before[sizeof(*frame)];
  memcpy(before,frame,sizeof(before));
  KuResult_str output={0};
  for (size_t i=0;i<4u;i++) {
    const KuTaskFrameScopeDescriptorV1* again=NULL;
    CHECK(ku_task_frame_0_resume(frame,sizeof(*frame),KU_TASK_FRAME_ABI_VERSION,clock)==KU_TASK_FRAME_SCOPE_REQUEST);
    CHECK(ku_task_frame_0_scope_request(frame,sizeof(*frame),KU_TASK_FRAME_ABI_VERSION,&again)==KU_TASK_FRAME_OK);
    CHECK(again==descriptor && !memcmp(before,frame,sizeof(before)));
  }
  CHECK(ku_task_frame_0_take_result(frame,sizeof(*frame),KU_TASK_FRAME_ABI_VERSION,&output)==KU_TASK_FRAME_INVALID_STATE);
  CHECK(ku_task_frame_0_destroy(frame,sizeof(*frame),KU_TASK_FRAME_ABI_VERSION)==KU_TASK_FRAME_INVALID_STATE);
  CHECK(ku_task_frame_0_finish_exit_values(frame,sizeof(*frame),KU_TASK_FRAME_ABI_VERSION)==KU_TASK_FRAME_INVALID_STATE);
  CHECK(!output.ok && ku_task_outcome_empty_string(output.value)
      && ku_task_outcome_empty_string(output.error.domain)
      && ku_task_outcome_empty_string(output.error.code)
      && ku_task_outcome_empty_string(output.error.message));
  const KuTaskFrameScopeDescriptorV1* untouched=descriptor;
  CHECK(ku_task_frame_0_scope_request(frame,sizeof(*frame),KU_TASK_FRAME_ABI_VERSION,&untouched)==KU_TASK_FRAME_INVALID_ARGUMENT);
  CHECK(untouched==descriptor);
  untouched=NULL;
  CHECK(ku_task_frame_0_scope_request(frame,sizeof(*frame)-1u,KU_TASK_FRAME_ABI_VERSION,&untouched)==KU_TASK_FRAME_INVALID_STORAGE);
  CHECK(ku_task_frame_0_scope_request(frame,sizeof(*frame),3u,&untouched)==KU_TASK_FRAME_ABI_MISMATCH);
  CHECK(ku_task_frame_0_scope_request(frame,sizeof(*frame),KU_TASK_FRAME_ABI_VERSION,
      (const KuTaskFrameScopeDescriptorV1**)frame)==KU_TASK_FRAME_INVALID_ARGUMENT);
  CHECK(!untouched);
  CHECK(ku_task_frame_0_scope_continue(frame,sizeof(*frame),KU_TASK_FRAME_ABI_VERSION,1u)==KU_TASK_FRAME_INVALID_STATE);
  CHECK(ku_task_frame_0_scope_timeout(frame,sizeof(*frame),KU_TASK_FRAME_ABI_VERSION,1u,1100u)==KU_TASK_FRAME_INVALID_STATE);
  CHECK(ku_task_frame_0_scope_timeout(frame,sizeof(*frame),KU_TASK_FRAME_ABI_VERSION,0u,UINT64_MAX)==KU_TASK_FRAME_INVALID_ARGUMENT);
  CHECK(!memcmp(before,frame,sizeof(before)));

  /* Isolated raw-negative metadata, not a fabricated child or recovery path.
   * Each rejection compares the SAME intentionally invalid byte state. Restore
   * only the field that this test changed before the next legitimate call. */
  uint64_t initialized=frame->header.initialized;
  frame->header.initialized|=UINT64_C(1)<<5;
  memcpy(before,frame,sizeof(before));
  CHECK(ku_task_frame_0_scope_continue(frame,sizeof(*frame),KU_TASK_FRAME_ABI_VERSION,0u)==KU_TASK_FRAME_INVALID_STATE);
  CHECK(!memcmp(before,frame,sizeof(before)));
  frame->header.initialized=initialized;
  uint32_t cleanup=frame->header.cleanup_state;
  frame->header.cleanup_state=descriptor->ready_state;
  memcpy(before,frame,sizeof(before));
  CHECK(ku_task_frame_0_scope_request(frame,sizeof(*frame),KU_TASK_FRAME_ABI_VERSION,&untouched)==KU_TASK_FRAME_INVALID_STATE);
  CHECK(!untouched && !memcmp(before,frame,sizeof(before)));
  frame->header.cleanup_state=cleanup;
  frame->header.result_initialized=1u;
  memcpy(before,frame,sizeof(before));
  CHECK(ku_task_frame_0_scope_timeout(frame,sizeof(*frame),KU_TASK_FRAME_ABI_VERSION,0u,1100u)==KU_TASK_FRAME_INVALID_STATE);
  CHECK(!memcmp(before,frame,sizeof(before)));
  frame->header.result_initialized=0u;
  CHECK(ku_task_frame_0_check(frame,sizeof(*frame),KU_TASK_FRAME_ABI_VERSION)==KU_TASK_FRAME_OK);
}
static void fixture_run(FixtureHost* carrier,int branch,int mode) {
  CHECK(!fixture_ledger().allocations);
  memset(fixture_buffers,0,sizeof(fixture_buffers)); memset(fixture_frees,0,sizeof(fixture_frees));
  fixture_buffer_count=0;
  KuTaskFrame_0 frame={0}; bool condition=branch!=0;
  KuString input=fixture_owned("owned",11u);
  KuResult_str business={0}; business.ok=!branch;
  if (!branch) business.value=fixture_owned("value",13u);
  else {
    business.error.domain=fixture_owned("scope",7u);
    business.error.code=fixture_owned("bad",9u);
    business.error.message=fixture_owned("expected",17u);
  }
  FixtureLedger original=fixture_ledger(); size_t calls=original.calls;
  KuTaskAdapterHostV1 host={NULL,&carrier->control,NULL,0};
  KuTaskFrameClockV1 clock={fixture_now,carrier,&host};
  carrier->now=100u; unsigned samples=carrier->samples;
  CHECK(ku_task_frame_0_init(&frame,sizeof(frame),KU_TASK_FRAME_ABI_VERSION,&condition,&input,&business)==KU_TASK_FRAME_OK);
  CHECK(!input.ptr && !business.ok && !business.value.ptr && !business.error.domain.ptr);
  CHECK(ku_task_frame_0_resume(&frame,sizeof(frame),KU_TASK_FRAME_ABI_VERSION,&clock)==KU_TASK_FRAME_SCOPE_REQUEST);
  CHECK(frame.header.status==KU_TASK_FRAME_SCOPE_REQUEST && !frame.header.running && !frame.header.result_initialized);
  CHECK(frame.s_3==37 && !frame.s_1.ptr && (uintptr_t)frame.s_6.ptr==fixture_buffers[0]);
  CHECK(!(frame.header.initialized & (UINT64_C(1)<<5)));
  fixture_business_result(&frame.s_2,branch);
  const KuTaskFrameScopeDescriptorV1* descriptor=NULL;
  CHECK(ku_task_frame_0_scope_request(&frame,sizeof(frame),KU_TASK_FRAME_ABI_VERSION,&descriptor)==KU_TASK_FRAME_OK);
  CHECK(descriptor && descriptor->scope_id==0u && descriptor->task_mask==(UINT64_C(1)<<5));
  CHECK(descriptor->request_state==(branch ? 1u : 2u));
  CHECK(descriptor->ready_state==(branch ? 3u : 4u) && descriptor->cleanup_state==(branch ? 5u : 6u));
  if (fixture_descriptors[branch]) CHECK(fixture_descriptors[branch]==descriptor);
  else fixture_descriptors[branch]=descriptor;
  fixture_stopped_negatives(&frame,&clock,descriptor);
  CHECK(carrier->samples==samples && fixture_ledger().calls==calls);
  CHECK(fixture_ledger().allocations==original.allocations && fixture_ledger().bytes==original.bytes);
  for (size_t i=0;i<fixture_buffer_count;i++) CHECK(!fixture_frees[i]);
  CHECK(ku_task_control_atomic_load(&carrier->control.phase)==KU_TASK_CONTROL_LIVE);

  if (mode==0) {
    CHECK(ku_task_frame_0_scope_continue(&frame,sizeof(frame),KU_TASK_FRAME_ABI_VERSION,0u)==KU_TASK_FRAME_OK);
    CHECK(frame.header.status==KU_TASK_FRAME_PENDING && frame.header.state==descriptor->ready_state);
    CHECK(frame.header.cleanup_state==UINT32_MAX && frame.s_3==37 && !fixture_frees[0]);
    unsigned char continued[sizeof(frame)]; memcpy(continued,&frame,sizeof(frame));
    const KuTaskFrameScopeDescriptorV1* no_request=NULL;
    CHECK(ku_task_frame_0_scope_continue(&frame,sizeof(frame),KU_TASK_FRAME_ABI_VERSION,0u)==KU_TASK_FRAME_INVALID_STATE);
    CHECK(ku_task_frame_0_scope_request(&frame,sizeof(frame),KU_TASK_FRAME_ABI_VERSION,&no_request)==KU_TASK_FRAME_INVALID_STATE);
    CHECK(!no_request && !memcmp(continued,&frame,sizeof(frame)));
    CHECK(ku_task_frame_0_resume(&frame,sizeof(frame),KU_TASK_FRAME_ABI_VERSION,&clock)==KU_TASK_FRAME_EXIT_STAGED);
    CHECK(frame.header.exit_class==KU_TASK_EXIT_USER_RESULT && !frame.header.has_exit_deadline);
    CHECK(fixture_frees[0]==1u && !frame.s_6.ptr && !frame.s_2.ok);
    for (size_t i=1;i<fixture_buffer_count;i++) CHECK(!fixture_frees[i]);
    fixture_business_result(&frame.result,branch);
    CHECK(ku_task_frame_0_finish_exit_values(&frame,sizeof(frame),KU_TASK_FRAME_ABI_VERSION)==KU_TASK_FRAME_OK);
    KuResult_str output={0};
    CHECK(ku_task_frame_0_take_result(&frame,sizeof(frame),KU_TASK_FRAME_ABI_VERSION,&output)==KU_TASK_FRAME_OK);
    fixture_business_result(&output,branch); ku_result_drop_str(&output);
  } else if (mode==1) {
    CHECK(ku_task_frame_0_terminate(&frame,sizeof(frame),KU_TASK_FRAME_ABI_VERSION,KU_TASK_FRAME_CANCELLED,1100u,&clock)==KU_TASK_FRAME_CANCELLED);
    CHECK(!frame.header.initialized && !frame.header.result_initialized && !frame.header.cleanup_timed_out);
    CHECK(frame.header.state==descriptor->cleanup_state);
    fixture_all_freed(); samples=carrier->samples;
    CHECK(ku_task_frame_0_terminate(&frame,sizeof(frame),KU_TASK_FRAME_ABI_VERSION,KU_TASK_FRAME_CANCELLED,1100u,&clock)==KU_TASK_FRAME_CANCELLED);
    CHECK(carrier->samples==samples);
  } else {
    uint64_t initialized=frame.header.initialized,deadline=1100u;
    CHECK(ku_task_frame_0_scope_timeout(&frame,sizeof(frame),KU_TASK_FRAME_ABI_VERSION,0u,deadline)==KU_TASK_FRAME_EXIT_STAGED);
    CHECK(frame.header.initialized==initialized && frame.header.result_initialized==1u);
    CHECK(frame.header.exit_class==KU_TASK_EXIT_RUNTIME_FAILURE && frame.header.has_exit_deadline==1u);
    CHECK(frame.header.exit_deadline==deadline && frame.header.cleanup_state==UINT32_MAX);
    CHECK(frame.s_3==37 && (uintptr_t)frame.s_6.ptr==fixture_buffers[0]);
    fixture_business_result(&frame.s_2,branch);
    CHECK(!frame.result.ok);
    fixture_text(frame.result.error.domain,"task");
    fixture_text(frame.result.error.code,"shutdown_timeout");
    fixture_text(frame.result.error.message,"owned child cleanup deadline expired");
    CHECK(frame.result.error.message.storage==KU_STRING_STATIC);
    carrier->now=deadline+50u;
    unsigned char staged[sizeof(frame)]; memcpy(staged,&frame,sizeof(frame));
    CHECK(ku_task_frame_0_resume(&frame,sizeof(frame),KU_TASK_FRAME_ABI_VERSION,&clock)==KU_TASK_FRAME_EXIT_STAGED);
    CHECK(ku_task_frame_0_scope_timeout(&frame,sizeof(frame),KU_TASK_FRAME_ABI_VERSION,0u,deadline+999u)==KU_TASK_FRAME_INVALID_STATE);
    CHECK(!memcmp(staged,&frame,sizeof(frame)) && frame.header.exit_deadline==deadline);
    CHECK(carrier->samples==samples && fixture_ledger().calls==calls);
    for (size_t i=0;i<fixture_buffer_count;i++) CHECK(!fixture_frees[i]);
    CHECK(ku_task_frame_0_finish_exit_values(&frame,sizeof(frame),KU_TASK_FRAME_ABI_VERSION)==KU_TASK_FRAME_OK);
    fixture_all_freed();
    KuResult_str output={0};
    CHECK(ku_task_frame_0_take_result(&frame,sizeof(frame),KU_TASK_FRAME_ABI_VERSION,&output)==KU_TASK_FRAME_OK);
    fixture_text(output.error.code,"shutdown_timeout"); ku_result_drop_str(&output);
  }
  CHECK(ku_task_control_atomic_load(&carrier->control.phase)==KU_TASK_CONTROL_LIVE);
  CHECK(ku_task_frame_0_destroy(&frame,sizeof(frame),KU_TASK_FRAME_ABI_VERSION)==KU_TASK_FRAME_OK);
  CHECK(ku_task_frame_0_destroy(&frame,sizeof(frame),KU_TASK_FRAME_ABI_VERSION)==KU_TASK_FRAME_INVALID_STATE);
  CHECK(fixture_ledger().calls==calls); fixture_all_freed();
}
int main(void) {
  FixtureHost carrier={0}; KuTaskControlOwnerV1 owner={0};
  CHECK(ku_task_control_init(&carrier.control,sizeof(carrier.control),KU_TASK_CONTROL_ABI_VERSION,
      &fixture_host_ops,&carrier,&owner)==KU_TASK_CONTROL_OK);
  for (int branch=0;branch<2;branch++) for (int mode=0;mode<3;mode++) fixture_run(&carrier,branch,mode);
  CHECK(fixture_descriptors[0] && fixture_descriptors[1] && fixture_descriptors[0]!=fixture_descriptors[1]);

  KuTaskFrame_1 plain={0}; KuTaskAdapterHostV1 host={NULL,&carrier.control,NULL,0};
  KuTaskFrameClockV1 clock={fixture_now,&carrier,&host}; KuResult_null output={0};
  CHECK(ku_task_frame_1_init(&plain,sizeof(plain),KU_TASK_FRAME_ABI_VERSION)==KU_TASK_FRAME_OK);
  /* No-scope raw status is rejected without any emitted lookup/helper. */
  plain.header.status=KU_TASK_FRAME_SCOPE_REQUEST;
  unsigned char invalid[sizeof(plain)]; memcpy(invalid,&plain,sizeof(plain));
  CHECK(ku_task_frame_1_check(&plain,sizeof(plain),KU_TASK_FRAME_ABI_VERSION)==KU_TASK_FRAME_INVALID_STATE);
  CHECK(ku_task_frame_1_resume(&plain,sizeof(plain),KU_TASK_FRAME_ABI_VERSION,&clock)==KU_TASK_FRAME_INVALID_STATE);
  CHECK(!memcmp(invalid,&plain,sizeof(plain))); plain.header.status=KU_TASK_FRAME_OK;
  CHECK(ku_task_frame_1_resume(&plain,sizeof(plain),KU_TASK_FRAME_ABI_VERSION,&clock)==KU_TASK_FRAME_EXIT_STAGED);
  CHECK(ku_task_frame_1_finish_exit_values(&plain,sizeof(plain),KU_TASK_FRAME_ABI_VERSION)==KU_TASK_FRAME_OK);
  CHECK(ku_task_frame_1_take_result(&plain,sizeof(plain),KU_TASK_FRAME_ABI_VERSION,&output)==KU_TASK_FRAME_OK && output.ok);
  ku_result_drop_null(&output);
  CHECK(ku_task_frame_1_destroy(&plain,sizeof(plain),KU_TASK_FRAME_ABI_VERSION)==KU_TASK_FRAME_OK);
  carrier.finished=1u;
  CHECK(ku_task_control_request_cancel(&owner.lease,KU_TASK_CONTROL_CANCELLED,2000u)==KU_TASK_CONTROL_OK);
  CHECK(ku_task_control_poll(&owner.lease)==KU_TASK_CONTROL_CANCELLED);
  CHECK(ku_task_control_owner_drop(&owner,2000u)==KU_TASK_CONTROL_OK && !owner.lease.control);
  CHECK(carrier.disposed==1u && carrier.cleanups==1u && carrier.drops==1u);
  fixture_all_freed(); puts("scope-frame-ok"); return 0;
}
"#;
