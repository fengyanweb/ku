//! Driver-only bounded normal-scope sessions. No source if/loop lowering.
#[path = "support/native_allocation_harness.rs"]
mod native_allocation_harness;
#[allow(dead_code)]
#[path = "support/native_pg_harness.rs"]
mod native_harness;

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
use std::{fs, process::Command};

fn fixture_source() -> String {
    let ast = Parser::new(Lexer::new("fn main() {}").lex().unwrap())
        .parse_program()
        .unwrap();
    Checker::new().check(&ast).unwrap();
    let sync = ir::lower_program(&ast).unwrap();
    let result = IrType::Result(Box::new(IrType::Str));
    let tasks = TaskProgram {
        functions: vec![TaskFunction {
            id: TaskFunctionId(0),
            name: "ScopeSessionOwnedFrame".into(),
            slots: [IrType::Int, result.clone()]
                .into_iter()
                .map(|ty| TaskSlot {
                    ty: TaskSlotType::Value {
                        ty,
                        borrowed: false,
                    },
                })
                .collect(),
            parameters: vec![SlotId(0), SlotId(1)],
            entry: StateId(0),
            result,
            states: vec![
                TaskState {
                    operations: vec![],
                    terminator: TaskTerminator::Suspend {
                        resume: StateId(1),
                        cleanup: StateId(2),
                    },
                },
                TaskState {
                    operations: vec![],
                    terminator: TaskTerminator::Complete { value: SlotId(1) },
                },
                TaskState {
                    operations: vec![TaskOp::Drop { slot: SlotId(1) }],
                    terminator: TaskTerminator::Terminate,
                },
            ],
        }],
    };
    c::generate_task_frame_c_source(&sync, &tasks).unwrap()
}

fn replace_once(source: String, anchor: &str, replacement: &str) -> String {
    assert_eq!(
        source.matches(anchor).count(),
        1,
        "scope session hook: {anchor}"
    );
    source.replacen(anchor, replacement, 1)
}

#[test]
fn native_task_scope_sessions_complete_manifest_and_linearization_execute_in_c() {
    let generated = fixture_source();
    for forbidden in ["run_source", "const SOURCE", "task.spawn", "Task.new"] {
        assert!(!generated.contains(forbidden));
    }
    for required in [
        "#define KU_TASK_DRIVER_ABI_VERSION 5u",
        "ku_task_driver_scope_begin(",
        "ku_task_driver_scope_end(",
        "scope_expected_mask",
        "scope_session_epoch",
    ] {
        assert!(
            generated.contains(required),
            "missing session ABI: {required}"
        );
    }
    let instrumentation = format!("{NATIVE_THREAD_LIFECYCLE_HARNESS}\n{LEDGER_LOCK}\n{ALLOCATION_HOOK}\n{LOCKED_ALLOCATIONS}\n{HOOK_DECLARATIONS}\n");
    let source = replace_once(
        generated,
        "typedef struct KuString {",
        &format!("{instrumentation}typedef struct KuString {{"),
    );
    let source = replace_once(
        source,
        "static uint64_t ku_task_driver_now_ms(void) {",
        &format!("{CLOCK_HOOK}\nstatic uint64_t fixture_original_now(void) {{"),
    );
    let source = replace_once(source, "static uint32_t ku_task_0_resume(void* raw) {", "static uint32_t ku_task_0_resume(void* raw) {\n  uint32_t observed=fixture_resume(raw);\n  if (observed != UINT32_MAX) return observed;");
    let source = replace_once(source, "static uint32_t ku_task_0_cleanup(void* raw, uint32_t reason, KuTaskControlV1* budget) {", "static uint32_t ku_task_0_cleanup(void* raw, uint32_t reason, KuTaskControlV1* budget) {\n  uint32_t observed=fixture_cleanup(raw,reason);\n  if (observed != UINT32_MAX) return observed;");
    let source = replace_once(
        source,
        "static void ku_task_0_drop_frame(void* raw) {",
        "static void ku_task_0_drop_frame(void* raw) {\n  fixture_drop_frame(raw);",
    );
    let source = replace_once(source, "  slot->cleanup_acked_generation = slot->generation;\n  return KU_TASK_DRIVER_CLEANUP_ACK;", "  slot->cleanup_acked_generation = slot->generation;\n  fixture_ack(slot->driver_lease.control);\n  return KU_TASK_DRIVER_CLEANUP_ACK;");
    let source = replace_once(source, "static void ku_task_0_dispose(KuTaskControlV1* control, void* raw) {", "static void ku_task_0_dispose(KuTaskControlV1* control, void* raw) {\n  void* witness=fixture_before_dispose(raw);");
    let source = replace_once(source, "    ku_task_adapter_fault(ticket.driver, 0);\n}\nstatic const KuTaskControlOpsV1 ku_task_0_ops", "    ku_task_adapter_fault(ticket.driver, 0);\n  fixture_disposed(witness);\n}\nstatic const KuTaskControlOpsV1 ku_task_0_ops");
    let source = replace_once(source, "      ku_task_control_deadline_store(&control->cleanup_deadline_ms, absolute_deadline_ms);", "      fixture_publishing(control);\n      ku_task_control_deadline_store(&control->cleanup_deadline_ms, absolute_deadline_ms);");
    let source = replace_once(source, "    size_t close_phase = ku_task_control_atomic_load(&parent->driver_lease.control->phase);", "    fixture_close_before(parent->driver_lease.control);\n    size_t close_phase = ku_task_control_atomic_load(&parent->driver_lease.control->phase);\n    fixture_close_after(parent->driver_lease.control);");
    let mut source = replace_once(
        source,
        "int main(void) {",
        "static int ku_generated_main(void) {",
    );
    source.push_str(C_MAIN);
    let directory = TempDir::new("task-scope-session-v1");
    let c_file = directory.path().join("scope_session.c");
    fs::write(&c_file, source).unwrap();
    let Some(executable) = compile_harness(directory.path(), &c_file, "scope_session") else {
        assert!(
            std::env::var_os("GITHUB_ACTIONS").is_none(),
            "CI must execute native sessions"
        );
        return;
    };
    fs::remove_file(c_file).unwrap();
    let output = run_bounded(
        Command::new(&executable).current_dir(directory.path()),
        RUN_TIMEOUT,
        RUN_LIMITS,
    )
    .expect("session fixture must terminate within its process watchdog");
    assert!(
        output.status.success(),
        "scope session fixture:\n{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8(output.stdout).unwrap().replace('\r', ""),
        "task-scope-session-v1-ok\n"
    );
    assert!(output.stderr.is_empty());
}

const LEDGER_LOCK: &str = r#"
#define CHECK(c) do { if (!(c)) { fprintf(stderr, "scope session check line %d: %s\n", __LINE__, #c); abort(); } } while (0)
#if defined(_WIN32)
static SRWLOCK fixture_allocation_lock = SRWLOCK_INIT;
static void fixture_alloc_lock(void) { AcquireSRWLockExclusive(&fixture_allocation_lock); }
static void fixture_alloc_unlock(void) { ReleaseSRWLockExclusive(&fixture_allocation_lock); }
#else
static pthread_mutex_t fixture_allocation_lock = PTHREAD_MUTEX_INITIALIZER;
static void fixture_alloc_lock(void) { CHECK(!pthread_mutex_lock(&fixture_allocation_lock)); }
static void fixture_alloc_unlock(void) { CHECK(!pthread_mutex_unlock(&fixture_allocation_lock)); }
#endif
"#;

const LOCKED_ALLOCATIONS: &str = r#"
#undef malloc
#undef calloc
#undef realloc
#undef free
static void* fixture_malloc(size_t size) { fixture_alloc_lock(); void* p = ku_perf_malloc(size); fixture_alloc_unlock(); return p; }
static void* fixture_calloc(size_t count, size_t size) { fixture_alloc_lock(); void* p = ku_perf_calloc(count, size); fixture_alloc_unlock(); return p; }
static void* fixture_realloc(void* old, size_t size) { fixture_alloc_lock(); void* p = ku_perf_realloc(old, size); fixture_alloc_unlock(); return p; }
static void fixture_free(void* p) { fixture_alloc_lock(); ku_perf_free(p); fixture_alloc_unlock(); }
typedef struct FixtureLedger { size_t allocations, bytes, calls; int overflow; } FixtureLedger;
static FixtureLedger fixture_ledger(void) {
  fixture_alloc_lock(); FixtureLedger out = {ku_perf_live_allocations, ku_perf_live_bytes, ku_perf_calls, ku_perf_overflow};
  fixture_alloc_unlock(); return out;
}
#define malloc fixture_malloc
#define calloc fixture_calloc
#define realloc fixture_realloc
#define free fixture_free
"#;

const HOOK_DECLARATIONS: &str = r#"
static uint32_t fixture_resume(void*);
static uint32_t fixture_cleanup(void*,uint32_t);
static void fixture_drop_frame(void*);
static void fixture_ack(void*);
static void* fixture_before_dispose(void*);
static void fixture_disposed(void*);
static void fixture_publishing(void*);
static void fixture_close_before(void*);
static void fixture_close_after(void*);
"#;

const CLOCK_HOOK: &str = r#"
static KuTaskControlDeadlineV1 fixture_clock;
/* Fail only the actual shutdown caller's clock sample. The worker and later
 * observers retain healthy clocks; the resulting driver clock_fault remains
 * sticky through destruction and is never cleared by this fixture. */
static KU_THREAD_LOCAL int fixture_fail_clock;
static KU_THREAD_LOCAL unsigned fixture_failed_clock_reads;
static uint64_t fixture_original_now(void);
static uint64_t ku_task_driver_now_ms(void) {
  if (fixture_fail_clock) {
    CHECK(++fixture_failed_clock_reads <= 2u);
    return UINT64_MAX;
  }
  uint64_t value=ku_task_control_deadline_load(&fixture_clock);
  return value ? value : fixture_original_now();
}
"#;

const C_MAIN: &str = r#"

#define ROLE_COUNT 6u
#define COMMAND_COUNT 32u
enum { OP_BEGIN, OP_END, OP_TRANSFER, OP_ARM, OP_DETACH, OP_HEADERS, OP_TOKEN_ERRORS,
       OP_ACTIVE_ERRORS, OP_BAD_MANIFEST, OP_LEGACY_ARM, OP_EPOCH, OP_REJECTED_BEGIN };
typedef struct FixtureDriver { KuTaskDriverV1* driver; KuTaskDriverSlotV1* slots; size_t* ring; size_t capacity; } FixtureDriver;
typedef struct FixtureCommand {
  unsigned op; size_t index, child, capacity; uint64_t mask, deadline, id; uint32_t expected;
} FixtureCommand;
typedef struct FixtureRole {
  KuTaskHandle_0 handle; KuTaskDriverTicketV1 ticket;
  KuTaskDriverCleanupReceiptV1 receipt, manifest[64];
  KuTaskDriverScopeTokenV1 scope, old_scope;
  KuTaskDriverWaitTokenV1 wait, old_wait;
  FixtureCommand commands[COMMAND_COUNT];
  KuTestEvent completed[COMMAND_COUNT], acked, disposed, publishing, publish_proceed, close_entered, close_proceed;
  KuAtomicRefcount resumes, cleanups, frame_drops, acks, disposes;
  size_t submitted, finished;
  uint32_t last_status, last_reason;
  int initialized, cleanup_hold, close_gate, publishing_gate;
} FixtureRole;
static FixtureRole roles[ROLE_COUNT];
static size_t fixture_count(KuAtomicRefcount* value) { return ku_task_control_atomic_load(value); }
static void fixture_inc(KuAtomicRefcount* value) { ku_task_control_atomic_store(value,fixture_count(value)+1u); }
static FixtureRole* fixture_role(void* raw) {
  KuTaskInstance_0* instance=(KuTaskInstance_0*)raw;
  CHECK(instance->frame.s_0>=0 && instance->frame.s_0<ROLE_COUNT);
  FixtureRole* role=&roles[(size_t)instance->frame.s_0]; CHECK(role->initialized); return role;
}
static uint64_t fixture_deadline(void) { return ku_task_driver_now_ms()+2000u; }
static void fixture_clock_begin(void) {
  /* Synthetic time selects interleavings, not OS timer performance. Each
   * event/join has a real 2s bound. Internal driver waits using frozen time
   * additionally rely on the unchanged real 20s process watchdog on failure. */
  ku_task_control_deadline_store(&fixture_clock,ku_test_real_now_ms()+100000u);
}
static void fixture_advance(FixtureDriver* runtime,uint64_t now) {
  CHECK(!ku_task_driver_lock(runtime->driver));
  CHECK(now>=ku_task_control_deadline_load(&fixture_clock));
  ku_task_control_deadline_store(&fixture_clock,now);
  (void)ku_task_driver_deadlines_locked(runtime->driver,now);
  ku_task_driver_signal(runtime->driver); CHECK(!ku_task_driver_unlock(runtime->driver));
}
static void fixture_idle(FixtureDriver* runtime) {
  CHECK(ku_task_driver_wait_idle(runtime->driver,fixture_deadline())==KU_TASK_DRIVER_OK);
  KuTaskDriverSnapshotV1 s={0}; CHECK(!ku_task_driver_snapshot(runtime->driver,&s));
  CHECK(!s.fault && !s.queued && !s.running && (s.worker_waiting || s.worker_exited));
}
static void fixture_stable(FixtureDriver* runtime) {
  fixture_idle(runtime); KuTaskDriverSnapshotV1 before={0},after={0};
  CHECK(!ku_task_driver_snapshot(runtime->driver,&before));
  for (size_t i=0;i<8u;i++) {
    fixture_idle(runtime); CHECK(!ku_task_driver_snapshot(runtime->driver,&after));
    CHECK(after.polls==before.polls && after.resident==before.resident);
  }
}
static void fixture_init(FixtureDriver* runtime,size_t capacity) {
  memset(runtime,0,sizeof(*runtime)); runtime->capacity=capacity;
  runtime->driver=(KuTaskDriverV1*)calloc(1,sizeof(*runtime->driver));
  runtime->slots=(KuTaskDriverSlotV1*)calloc(capacity,sizeof(*runtime->slots));
  runtime->ring=(size_t*)calloc(capacity,sizeof(*runtime->ring));
  CHECK(runtime->driver && runtime->slots && runtime->ring);
  size_t fixed=sizeof(*runtime->driver)+capacity*(sizeof(*runtime->slots)+sizeof(*runtime->ring));
  CHECK(KU_TASK_DRIVER_ABI_VERSION==5u && KU_TASK_FRAME_ABI_VERSION==2u);
  for (uint32_t old=1;old<5;old++) {
    CHECK(ku_task_driver_init(runtime->driver,sizeof(*runtime->driver),old,runtime->slots,capacity,runtime->ring,capacity,fixed+1048576u)==KU_TASK_DRIVER_ABI_MISMATCH);
    CHECK(ku_task_frame_zero_bytes(runtime->driver,sizeof(*runtime->driver)));
    CHECK(ku_task_frame_zero_bytes(runtime->slots,capacity*sizeof(*runtime->slots)));
    CHECK(ku_task_frame_zero_bytes(runtime->ring,capacity*sizeof(*runtime->ring)));
  }
  CHECK(!ku_task_driver_init(runtime->driver,sizeof(*runtime->driver),5u,runtime->slots,capacity,runtime->ring,capacity,fixed+1048576u));
  fixture_idle(runtime);
}
static void fixture_start(FixtureDriver* runtime,size_t i) {
  CHECK(i<ROLE_COUNT && !roles[i].initialized); FixtureRole* r=&roles[i]; memset(r,0,sizeof(*r));
  for (size_t j=0;j<COMMAND_COUNT;j++) CHECK(ku_test_event_init(&r->completed[j]));
  CHECK(ku_test_event_init(&r->acked)); CHECK(ku_test_event_init(&r->disposed));
  CHECK(ku_test_event_init(&r->publishing)); CHECK(ku_test_event_init(&r->publish_proceed));
  CHECK(ku_test_event_init(&r->close_entered)); CHECK(ku_test_event_init(&r->close_proceed));
  ku_task_control_atomic_init(&r->resumes,0); ku_task_control_atomic_init(&r->cleanups,0);
  ku_task_control_atomic_init(&r->frame_drops,0); ku_task_control_atomic_init(&r->acks,0); ku_task_control_atomic_init(&r->disposes,0);
  r->initialized=1; r->cleanup_hold=1;
  int64_t id=(int64_t)i; uint8_t* text=(uint8_t*)malloc(7u); CHECK(text); memcpy(text,"payload",7u);
  KuResult_str input={true,{text,7u,7u,KU_STRING_OWNED},{0}};
  CHECK(!ku_task_0_try_start(runtime->driver,&id,&input,&r->handle)); CHECK(!input.value.ptr);
  r->ticket=r->handle.ticket; fixture_idle(runtime);
}
static void fixture_transfer_into(size_t child,uint64_t deadline,KuTaskDriverCleanupReceiptV1* output) {
  FixtureRole* r=&roles[child]; size_t calls=fixture_ledger().calls;
  CHECK(!ku_task_driver_owner_drop_receipt(&r->ticket,&r->handle.owner,deadline,output));
  CHECK(!r->handle.owner.lease.control && output->generation==r->ticket.generation);
  r->receipt=*output; r->handle.ticket=(KuTaskDriverTicketV1){0};
  CHECK(fixture_ledger().calls==calls);
}
static void fixture_transfer(size_t child,uint64_t deadline) {
  fixture_transfer_into(child,deadline,&roles[child].receipt);
}
static size_t fixture_submit(FixtureDriver* runtime,size_t parent,FixtureCommand command) {
  FixtureRole* r=&roles[parent]; CHECK(!ku_task_driver_lock(runtime->driver));
  CHECK(runtime->slots[r->ticket.slot].state!=KU_TASK_DRIVER_RUNNING);
  size_t index=r->submitted; CHECK(index<COMMAND_COUNT && index==r->finished);
  r->commands[index]=command; r->submitted++;
  CHECK(!ku_task_driver_unlock(runtime->driver)); CHECK(!ku_task_driver_wake(&r->ticket)); return index;
}
static void fixture_done(FixtureDriver* runtime,size_t parent,size_t command) {
  CHECK(ku_test_event_wait(&roles[parent].completed[command],2000u)); fixture_idle(runtime);
  CHECK(roles[parent].finished==command+1u);
}
static void fixture_do(FixtureDriver* runtime,size_t parent,FixtureCommand command) {
  size_t i=fixture_submit(runtime,parent,command); fixture_done(runtime,parent,i);
}
static void fixture_release(FixtureDriver* runtime,size_t child) {
  CHECK(!ku_task_driver_lock(runtime->driver));
  CHECK(runtime->slots[roles[child].ticket.slot].state!=KU_TASK_DRIVER_RUNNING);
  roles[child].cleanup_hold=0; CHECK(!ku_task_driver_unlock(runtime->driver));
  CHECK(!ku_task_driver_wake(&roles[child].ticket));
  CHECK(ku_test_event_wait(&roles[child].acked,2000u));
  CHECK(ku_task_driver_cleanup_receipt_read(&roles[child].receipt)==KU_TASK_DRIVER_CLEANUP_ACK);
  fixture_idle(runtime);
}
static void fixture_retain(FixtureDriver* runtime,size_t i,KuTaskControlLeaseV1* observer) {
  CHECK(!ku_task_driver_lock(runtime->driver));
  CHECK(!ku_task_control_lease_retain(&runtime->slots[roles[i].ticket.slot].driver_lease,observer));
  CHECK(!ku_task_driver_unlock(runtime->driver));
}
static void fixture_live(size_t i) {
  CHECK(roles[i].handle.owner.lease.control);
  CHECK(ku_task_control_atomic_load(&roles[i].handle.owner.lease.control->phase)==KU_TASK_CONTROL_LIVE);
}
static void fixture_action(FixtureRole*,KuTaskInstance_0*,const FixtureCommand*);
static uint32_t fixture_resume(void* raw) {
  FixtureRole* r=fixture_role(raw); KuTaskInstance_0* instance=(KuTaskInstance_0*)raw;
  fixture_inc(&r->resumes);
  if (r->finished<r->submitted) {
    size_t command=r->finished; fixture_action(r,instance,&r->commands[command]);
    r->finished++; CHECK(ku_test_event_set(&r->completed[command]));
  }
  CHECK(!ku_task_driver_set_intent(&instance->ticket,KU_TASK_DRIVER_WAIT));
  return KU_TASK_CONTROL_PENDING;
}
static uint32_t fixture_cleanup(void* raw,uint32_t reason) {
  FixtureRole* r=fixture_role(raw); KuTaskInstance_0* instance=(KuTaskInstance_0*)raw;
  fixture_inc(&r->cleanups); r->last_reason=reason;
  if (r->cleanup_hold) {
    if (r->finished<r->submitted) {
      size_t command=r->finished; fixture_action(r,instance,&r->commands[command]);
      r->finished++; CHECK(ku_test_event_set(&r->completed[command]));
    }
    CHECK(!ku_task_driver_set_intent(&instance->ticket,KU_TASK_DRIVER_WAIT)); return KU_TASK_CONTROL_PENDING;
  }
  if (r->wait.driver) CHECK(!ku_task_driver_wait_detach(&r->wait));
  return UINT32_MAX;
}
static void fixture_drop_frame(void* raw) { fixture_inc(&fixture_role(raw)->frame_drops); }
static void fixture_ack(void* raw) {
  FixtureRole* r=fixture_role(((KuTaskControlV1*)raw)->context); fixture_inc(&r->acks);
  CHECK(ku_test_event_set(&r->acked)); /* Actual watermark event, queue lock held. */
}
static void* fixture_before_dispose(void* raw) { return fixture_role(raw); }
static void fixture_disposed(void* raw) {
  FixtureRole* r=(FixtureRole*)raw; fixture_inc(&r->disposes); CHECK(ku_test_event_set(&r->disposed));
}
static void fixture_publishing(void* raw) {
  FixtureRole* r=fixture_role(((KuTaskControlV1*)raw)->context);
  if (r->publishing_gate) { CHECK(ku_test_event_set(&r->publishing)); CHECK(ku_test_event_wait(&r->publish_proceed,2000u)); }
}
static void fixture_close_before(void* raw) {
  FixtureRole* r=fixture_role(((KuTaskControlV1*)raw)->context);
  if (r->close_gate==1) { CHECK(ku_test_event_set(&r->close_entered)); CHECK(ku_test_event_wait(&r->close_proceed,2000u)); }
}
static void fixture_close_after(void* raw) {
  FixtureRole* r=fixture_role(((KuTaskControlV1*)raw)->context);
  if (r->close_gate==2) { CHECK(ku_test_event_set(&r->close_entered)); CHECK(ku_test_event_wait(&r->close_proceed,2000u)); }
}
static void fixture_destroy_storage(FixtureDriver* runtime);
static void fixture_finish(FixtureDriver* runtime) {
  for (size_t i=0;i<ROLE_COUNT;i++) if (roles[i].initialized) {
    if (roles[i].handle.owner.lease.control) fixture_transfer(i,fixture_deadline());
    fixture_idle(runtime);
    if (!fixture_count(&roles[i].acks)) fixture_release(runtime,i);
    CHECK(ku_test_event_wait(&roles[i].disposed,2000u));
    CHECK(fixture_count(&roles[i].frame_drops)==1u && fixture_count(&roles[i].disposes)==1u);
  }
  CHECK(!ku_task_driver_shutdown(runtime->driver,fixture_deadline()));
  KuTaskDriverSnapshotV1 s={0}; CHECK(!ku_task_driver_snapshot(runtime->driver,&s));
  CHECK(!s.fault && !s.resident && !s.reserved_bytes && !s.queued && !s.running && !s.building);
  fixture_destroy_storage(runtime);
}
static void fixture_destroy_storage(FixtureDriver* runtime) {
  /* shutdown publishes worker_exited just before the OS entrypoint returns.
   * Wait on that actual thread, not a finite number of scheduler yields which
   * can expire before the worker is scheduled. Keep the existing 2s OS bound.
   * POSIX destroy joins after worker_exited, under the real process watchdog. */
#if defined(_WIN32)
  CHECK(WaitForSingleObject(runtime->driver->thread,2000u)==WAIT_OBJECT_0);
#endif
  CHECK(!ku_task_driver_destroy(runtime->driver));
  free(runtime->ring); free(runtime->slots); free(runtime->driver);
  for (size_t i=0;i<ROLE_COUNT;i++) if (roles[i].initialized) {
    FixtureRole* r=&roles[i];
    for (size_t j=0;j<COMMAND_COUNT;j++) CHECK(ku_test_event_destroy(&r->completed[j]));
    CHECK(ku_test_event_destroy(&r->acked)); CHECK(ku_test_event_destroy(&r->disposed));
    CHECK(ku_test_event_destroy(&r->publishing)); CHECK(ku_test_event_destroy(&r->publish_proceed));
    CHECK(ku_test_event_destroy(&r->close_entered)); CHECK(ku_test_event_destroy(&r->close_proceed));
    memset(r,0,sizeof(*r));
  }
  memset(runtime,0,sizeof(*runtime)); ku_task_control_deadline_store(&fixture_clock,0);
  FixtureLedger ledger=fixture_ledger(); CHECK(!ledger.allocations && !ledger.bytes && !ledger.overflow);
}
static int fixture_token_equal(const KuTaskDriverScopeTokenV1* a,const KuTaskDriverScopeTokenV1* b) {
  return a->driver==b->driver && a->parent_slot==b->parent_slot && a->parent_generation==b->parent_generation
      && a->scope_id==b->scope_id && a->epoch==b->epoch;
}
static void fixture_no_change(FixtureRole* r,const KuTaskDriverScopeTokenV1* token,
    KuTaskDriverSlotV1* p,const KuTaskDriverCleanupReceiptV1* span,uint64_t deadline,uint64_t epoch) {
  CHECK(fixture_token_equal(&r->scope,token));
  CHECK(p->scope_receipts==span && p->scope_wait_deadline==deadline && p->scope_session_epoch==epoch);
}
static void fixture_action(FixtureRole* r,KuTaskInstance_0* instance,const FixtureCommand* c) {
  KuTaskDriverV1* d=instance->ticket.driver; KuTaskDriverSlotV1* p=&d->slots[instance->ticket.slot];
  size_t calls=fixture_ledger().calls,refs=fixture_count(&instance->control.references); uint32_t status=UINT32_MAX;
  switch (c->op) {
    case OP_BEGIN: {
      CHECK(!r->scope.driver);
      memset(r->manifest,0,sizeof(r->manifest));
      status=ku_task_driver_scope_begin(&instance->ticket,c->id,r->manifest,c->capacity,c->mask,c->deadline,&r->scope);
      if (!status) {
        CHECK(p->scope_receipts==r->manifest && p->scope_receipt_capacity==c->capacity);
        CHECK(p->scope_expected_mask==c->mask && p->scope_wait_deadline==c->deadline);
        CHECK(p->scope_session_active && p->scope_wait_started);
      }
      break;
    }
    case OP_REJECTED_BEGIN: {
      /* Complete, independently stored headers and an empty manifest isolate
       * rejection by real fault/closing from argument or active-session errors. */
      CHECK(!r->scope.driver && !p->scope_session_active && !p->scope_receipts
          && !p->scope_receipt_capacity && !p->scope_expected_mask && !p->scope_wait_started);
      CHECK(ku_task_frame_zero_bytes(r->manifest,sizeof(r->manifest)));
      KuTaskDriverScopeTokenV1 previous=r->scope;
      uint64_t epoch=p->scope_session_epoch,deadline=p->scope_wait_deadline;
      uint64_t cancel_deadline=p->cancel_deadline,control_deadline=ku_task_control_cleanup_deadline(&instance->control);
      uint32_t fired=p->scope_deadline_fired,failure=p->scope_wait_failure;
      uint32_t closing=d->closing,fault=d->fault,clock_fault=d->clock_fault;
      size_t charged=p->charged_bytes;
      CHECK(closing && !p->cleanup_fault);
      status=ku_task_driver_scope_begin(&instance->ticket,c->id,r->manifest,1,0,c->deadline,&r->scope);
      CHECK(fixture_token_equal(&r->scope,&previous));
      CHECK(!p->scope_session_active && !p->scope_receipts && !p->scope_receipt_capacity
          && !p->scope_expected_mask && !p->scope_wait_started);
      CHECK(p->scope_session_epoch==epoch && p->scope_wait_deadline==deadline
          && p->scope_deadline_fired==fired && p->scope_wait_failure==failure);
      CHECK(p->cancel_deadline==cancel_deadline && p->charged_bytes==charged
          && ku_task_control_cleanup_deadline(&instance->control)==control_deadline);
      CHECK(d->closing==closing && d->fault==fault && d->clock_fault==clock_fault);
      CHECK(ku_task_frame_zero_bytes(r->manifest,sizeof(r->manifest)));
      break;
    }
    case OP_END: {
      KuTaskDriverScopeTokenV1 previous=r->scope;
      const KuTaskDriverCleanupReceiptV1* span=p->scope_receipts;
      uint64_t deadline=p->scope_wait_deadline,epoch=p->scope_session_epoch,wait_epoch=p->wait_epoch;
      uint64_t cancel_deadline=p->cancel_deadline; uint32_t cancel_pending=p->cancel_pending;
      status=ku_task_driver_scope_end(&instance->ticket,&r->scope);
      if (!status) {
        r->old_scope=previous;
        CHECK(ku_task_frame_zero_bytes(&r->scope,sizeof(r->scope)));
        CHECK(!p->scope_session_active && !p->scope_receipts && !p->scope_receipt_capacity && !p->scope_expected_mask);
        CHECK(!p->scope_wait_started && !p->scope_deadline_fired && !p->scope_wait_failure);
        CHECK(p->scope_wait_deadline==UINT64_MAX && p->scope_session_epoch==epoch && p->wait_epoch==wait_epoch);
        CHECK(p->cancel_deadline==cancel_deadline && p->cancel_pending==cancel_pending);
      } else fixture_no_change(r,&previous,p,span,deadline,epoch);
      break;
    }
    case OP_TRANSFER:
      CHECK(c->index<64u && c->child<ROLE_COUNT);
      fixture_transfer_into(c->child,c->deadline,&r->manifest[c->index]); status=KU_TASK_DRIVER_OK; break;
    case OP_ARM: {
      status=ku_task_driver_cleanup_wait_arm(&instance->ticket,&r->manifest[c->index],c->deadline,&r->wait);
      if (status==KU_TASK_DRIVER_PENDING && r->old_wait.driver) {
        KuTaskDriverWaitTokenV1 old=r->old_wait;
        KuTaskDriverWaitSnapshotV1 snapshot={99u,99u,99u,99u,99u};
        CHECK(old.epoch!=r->wait.epoch);
        CHECK(ku_task_driver_wait_read(&old,&snapshot)==KU_TASK_DRIVER_STALE);
        CHECK(snapshot.state==99u && snapshot.outcome==99u && snapshot.child_slot==99u
            && snapshot.child_generation==99u && snapshot.kind==99u);
        CHECK(ku_task_driver_wait_detach(&old)==KU_TASK_DRIVER_STALE);
        CHECK(old.driver==r->old_wait.driver && old.epoch==r->old_wait.epoch);
      }
      break;
    }
    case OP_LEGACY_ARM:
      status=ku_task_driver_cleanup_wait_arm(&instance->ticket,&roles[c->child].receipt,c->deadline,&r->wait); break;
    case OP_DETACH:
      r->old_wait=r->wait; status=ku_task_driver_wait_detach(&r->wait); break;
    case OP_HEADERS: {
      KuTaskDriverScopeTokenV1 out={0},bad={0}; bad.epoch=1;
      KuTaskDriverCleanupReceiptV1 issued=roles[c->child].receipt;
      CHECK(issued.driver);
      CHECK(ku_task_driver_scope_begin(&instance->ticket,0,r->manifest,0,0,c->deadline,&out)==KU_TASK_DRIVER_INVALID_ARGUMENT);
      CHECK(ku_task_driver_scope_begin(&instance->ticket,0,r->manifest,65,0,c->deadline,&out)==KU_TASK_DRIVER_INVALID_ARGUMENT);
      CHECK(ku_task_driver_scope_begin(&instance->ticket,0,r->manifest,1,2,c->deadline,&out)==KU_TASK_DRIVER_INVALID_ARGUMENT);
      CHECK(ku_task_driver_scope_begin(&instance->ticket,0,r->manifest,1,0,UINT64_MAX,&out)==KU_TASK_DRIVER_INVALID_ARGUMENT);
      CHECK(ku_task_driver_scope_begin(&instance->ticket,0,r->manifest,1,0,c->deadline,&bad)==KU_TASK_DRIVER_INVALID_STATE);
      CHECK(ku_task_driver_scope_begin(&instance->ticket,0,&issued,1,1,c->deadline,&out)==KU_TASK_DRIVER_INVALID_STATE);
      CHECK(ku_task_driver_scope_begin(&instance->ticket,0,r->manifest,1,0,c->deadline,(KuTaskDriverScopeTokenV1*)&instance->ticket)==KU_TASK_DRIVER_INVALID_ARGUMENT);
      CHECK(ku_task_driver_scope_begin(&instance->ticket,0,(KuTaskDriverCleanupReceiptV1*)&instance->control,1,1,c->deadline,&out)==KU_TASK_DRIVER_INVALID_ARGUMENT);
      CHECK(ku_task_driver_scope_begin(&instance->ticket,0,r->manifest,2,0,c->deadline,(KuTaskDriverScopeTokenV1*)&r->manifest[0])==KU_TASK_DRIVER_INVALID_ARGUMENT);
      CHECK(ku_task_driver_scope_begin(&instance->ticket,0,r->manifest,1,0,c->deadline,(KuTaskDriverScopeTokenV1*)d)==KU_TASK_DRIVER_INVALID_ARGUMENT);
      KuTaskDriverTicketV1 stale=instance->ticket; stale.generation++;
      CHECK(ku_task_driver_scope_begin(&stale,0,r->manifest,1,0,c->deadline,&out)==KU_TASK_DRIVER_STALE);
      CHECK(!p->scope_session_active && !p->scope_receipts && !p->scope_wait_started && !p->scope_session_epoch);
      CHECK(ku_task_frame_zero_bytes(&out,sizeof(out)) && bad.epoch==1u); status=KU_TASK_DRIVER_OK; break;
    }
    case OP_TOKEN_ERRORS: {
      KuTaskDriverScopeTokenV1 previous=r->scope,old=r->old_scope,bad=r->scope;
      const KuTaskDriverCleanupReceiptV1* span=p->scope_receipts;
      uint64_t deadline=p->scope_wait_deadline,epoch=p->scope_session_epoch;
      CHECK(old.epoch && (old.epoch!=previous.epoch || old.parent_generation!=previous.parent_generation));
      CHECK(ku_task_driver_scope_end(&instance->ticket,&old)==KU_TASK_DRIVER_STALE);
      CHECK(fixture_token_equal(&old,&r->old_scope));
      bad.scope_id++;
      CHECK(ku_task_driver_scope_end(&instance->ticket,&bad)==KU_TASK_DRIVER_STALE);
      bad=r->scope; bad.parent_generation++;
      CHECK(ku_task_driver_scope_end(&instance->ticket,&bad)==KU_TASK_DRIVER_STALE);
      CHECK(ku_task_driver_scope_end(&instance->ticket,(KuTaskDriverScopeTokenV1*)&instance->ticket)==KU_TASK_DRIVER_INVALID_ARGUMENT);
      CHECK(ku_task_driver_scope_end(&instance->ticket,(KuTaskDriverScopeTokenV1*)&instance->control)==KU_TASK_DRIVER_INVALID_ARGUMENT);
      CHECK(ku_task_driver_scope_end(&instance->ticket,(KuTaskDriverScopeTokenV1*)&r->manifest[0])==KU_TASK_DRIVER_INVALID_ARGUMENT);
      fixture_no_change(r,&previous,p,span,deadline,epoch); status=KU_TASK_DRIVER_OK; break;
    }
    case OP_ACTIVE_ERRORS: {
      KuTaskDriverScopeTokenV1 previous=r->scope,out={0};
      const KuTaskDriverCleanupReceiptV1* span=p->scope_receipts;
      uint64_t deadline=p->scope_wait_deadline,epoch=p->scope_session_epoch;
      CHECK(ku_task_driver_scope_begin(&instance->ticket,999,r->manifest,1,1,c->deadline,&out)==KU_TASK_DRIVER_INVALID_STATE);
      KuTaskDriverWaitTokenV1 wait={0}; KuTaskDriverCleanupReceiptV1 copy=r->manifest[c->index];
      CHECK(copy.driver);
      /* The active session already knows the canonical allowed addresses.
       * This smaller valid object must be rejected by address BEFORE a
       * receipt-sized typed read; legacy raw calls retain full-header rules. */
      int64_t short_object=0;
      CHECK(ku_task_driver_cleanup_wait_arm(&instance->ticket,(KuTaskDriverCleanupReceiptV1*)&short_object,c->deadline,&wait)==KU_TASK_DRIVER_INVALID_ARGUMENT);
      CHECK(ku_task_driver_cleanup_wait_arm(&instance->ticket,&r->manifest[c->index],c->deadline,(KuTaskDriverWaitTokenV1*)&r->manifest[0])==KU_TASK_DRIVER_INVALID_ARGUMENT);
      CHECK(ku_task_driver_cleanup_wait_arm(&instance->ticket,&copy,c->deadline,&wait)==KU_TASK_DRIVER_INVALID_ARGUMENT);
      CHECK(ku_task_driver_cleanup_wait_arm(&instance->ticket,&roles[c->child].receipt,c->deadline,&wait)==KU_TASK_DRIVER_INVALID_ARGUMENT);
      CHECK(ku_task_driver_wait_arm(&instance->ticket,&roles[c->child].ticket,&roles[c->child].handle.owner.lease,&wait)==KU_TASK_DRIVER_INVALID_STATE);
      CHECK(ku_task_frame_zero_bytes(&out,sizeof(out)) && ku_task_frame_zero_bytes(&wait,sizeof(wait)));
      fixture_no_change(r,&previous,p,span,deadline,epoch); status=KU_TASK_DRIVER_OK; break;
    }
    case OP_EPOCH: {
      /* Reach the finite counter boundary directly, without faking phase,
       * receipts, success, or reducing the counter after the test. */
      CHECK(!ku_task_driver_lock(d));
      CHECK(!p->scope_session_active && !p->scope_session_epoch);
      p->scope_session_epoch=UINT64_MAX-1u;
      CHECK(!ku_task_driver_unlock(d));
      CHECK(!ku_task_driver_scope_begin(&instance->ticket,0,r->manifest,1,0,c->deadline,&r->scope));
      CHECK(r->scope.epoch==UINT64_MAX);
      CHECK(!ku_task_driver_scope_end(&instance->ticket,&r->scope));
      status=ku_task_driver_scope_begin(&instance->ticket,0,r->manifest,1,0,c->deadline,&r->scope);
      CHECK(!r->scope.driver && !p->scope_session_active && !p->scope_receipts);
      CHECK(p->scope_session_epoch==UINT64_MAX && p->scope_wait_deadline==UINT64_MAX); break;
    }
    case OP_BAD_MANIFEST:
      /* Explicit raw-invalid, write-once test inputs, never repaired in-place.
       * A real cancellation/terminal later discards registration without reads. */
      if (c->index==0) {
        fixture_transfer_into(c->child,c->deadline,&r->manifest[0]);
        r->manifest[1]=r->manifest[0]; /* One real child is not two ACKs. */
      } else if (c->index==1) fixture_transfer_into((size_t)instance->frame.s_0,c->deadline,&r->manifest[0]);
      else {
        fixture_transfer_into(c->child,c->deadline,&r->manifest[0]);
        r->manifest[0].abi_version=4u; /* Malformed header, not a forged success. */
      }
      status=ku_task_driver_scope_end(&instance->ticket,&r->scope); break;
    default: CHECK(0);
  }
  r->last_status=status; CHECK(status==c->expected); CHECK(fixture_ledger().calls==calls);
  CHECK(fixture_count(&instance->control.references)==refs);
}

static FixtureCommand fixture_command(unsigned op,uint32_t expected) {
  FixtureCommand c={0}; c.op=op; c.expected=expected; return c;
}
static void fixture_begin(FixtureDriver* rt,size_t parent,size_t capacity,uint64_t mask,uint64_t id,uint64_t deadline,uint32_t expected) {
  FixtureCommand c=fixture_command(OP_BEGIN,expected); c.capacity=capacity; c.mask=mask; c.id=id; c.deadline=deadline; fixture_do(rt,parent,c);
}
static void fixture_end(FixtureDriver* rt,size_t parent,uint32_t expected) {
  fixture_do(rt,parent,fixture_command(OP_END,expected));
}
static void fixture_issue(FixtureDriver* rt,size_t parent,size_t index,size_t child,uint64_t deadline) {
  FixtureCommand c=fixture_command(OP_TRANSFER,0); c.index=index; c.child=child; c.deadline=deadline; fixture_do(rt,parent,c);
}
static void fixture_arm(FixtureDriver* rt,size_t parent,size_t index,uint64_t deadline,uint32_t expected) {
  FixtureCommand c=fixture_command(OP_ARM,expected); c.index=index; c.deadline=deadline; fixture_do(rt,parent,c);
}
static void fixture_detach(FixtureDriver* rt,size_t parent) { fixture_do(rt,parent,fixture_command(OP_DETACH,0)); }
static KuTaskDriverWaitSnapshotV1 fixture_wait(size_t parent) {
  KuTaskDriverWaitSnapshotV1 s={0}; CHECK(!ku_task_driver_wait_read(&roles[parent].wait,&s));
  CHECK(s.kind==KU_TASK_DRIVER_WAIT_KIND_CLEANUP_ACK); return s;
}
static void fixture_normal_scopes(void) {
  fixture_clock_begin(); FixtureDriver rt; fixture_init(&rt,4);
  for (size_t i=0;i<4;i++) fixture_start(&rt,i);
  uint64_t first=ku_task_driver_now_ms()+60000u;
  fixture_begin(&rt,0,2,3,41,first,0); /* Both children declared BEFORE either transfer. */
  fixture_end(&rt,0,KU_TASK_DRIVER_PENDING); fixture_stable(&rt);
  fixture_issue(&rt,0,0,1,first); fixture_arm(&rt,0,0,first,KU_TASK_DRIVER_PENDING);
  fixture_end(&rt,0,KU_TASK_DRIVER_INVALID_STATE); fixture_stable(&rt);
  CHECK(fixture_wait(0).state==KU_TASK_DRIVER_WAIT_ARMED);
  fixture_release(&rt,1); fixture_detach(&rt,0);
  fixture_end(&rt,0,KU_TASK_DRIVER_PENDING); /* First ACK cannot stand in for missing second receipt. */
  FixtureCommand active=fixture_command(OP_ACTIVE_ERRORS,0); active.child=3; active.index=0; active.deadline=first;
  fixture_do(&rt,0,active);
  fixture_issue(&rt,0,1,2,first); fixture_arm(&rt,0,1,first,KU_TASK_DRIVER_PENDING);
  uint64_t old_wait_epoch=roles[0].wait.epoch;
  fixture_release(&rt,2);
  CHECK(fixture_wait(0).state==KU_TASK_DRIVER_WAIT_NOTIFIED && fixture_wait(0).outcome==KU_TASK_DRIVER_CLEANUP_ACK);
  fixture_advance(&rt,first+1u); fixture_idle(&rt);
  CHECK(!ku_task_driver_lock(rt.driver));
  CHECK(rt.slots[roles[0].ticket.slot].scope_deadline_fired && !rt.slots[roles[0].ticket.slot].scope_wait_failure);
  CHECK(!ku_task_driver_unlock(rt.driver));
  fixture_detach(&rt,0); fixture_end(&rt,0,0); fixture_live(0); fixture_live(3);
  CHECK(ku_test_event_wait(&roles[1].disposed,2000u) && ku_test_event_wait(&roles[2].disposed,2000u));
  fixture_start(&rt,4);
  CHECK((roles[4].ticket.slot==roles[1].ticket.slot && roles[4].ticket.generation>roles[1].ticket.generation)
      || (roles[4].ticket.slot==roles[2].ticket.slot && roles[4].ticket.generation>roles[2].ticket.generation));
  CHECK(ku_task_driver_cleanup_receipt_read(&roles[1].receipt)==KU_TASK_DRIVER_CLEANUP_ACK);
  CHECK(ku_task_driver_cleanup_receipt_read(&roles[2].receipt)==KU_TASK_DRIVER_CLEANUP_ACK);
  fixture_live(4); /* Old ACK watermarks do not cancel the reused slot. */
  uint64_t second=ku_task_driver_now_ms()+60000u;
  fixture_begin(&rt,0,1,1,42,second,0);
  fixture_do(&rt,0,fixture_command(OP_TOKEN_ERRORS,0));
  fixture_issue(&rt,0,0,4,second); fixture_arm(&rt,0,0,second,KU_TASK_DRIVER_PENDING);
  CHECK(roles[0].wait.epoch>old_wait_epoch); fixture_stable(&rt);
  CHECK(!ku_task_driver_lock(rt.driver));
  CHECK(rt.slots[roles[0].ticket.slot].scope_wait_deadline==second && !rt.slots[roles[0].ticket.slot].scope_deadline_fired);
  CHECK(!ku_task_driver_unlock(rt.driver));
  fixture_release(&rt,4); fixture_detach(&rt,0); fixture_end(&rt,0,0);
  fixture_live(0); fixture_live(3); fixture_finish(&rt);
}
static void fixture_headers_and_high_bit(void) {
  fixture_clock_begin(); FixtureDriver rt; fixture_init(&rt,3);
  fixture_start(&rt,0); fixture_start(&rt,1); fixture_start(&rt,2);
  uint64_t deadline=ku_task_driver_now_ms()+60000u;
  fixture_transfer(1,deadline); fixture_idle(&rt); fixture_release(&rt,1);
  FixtureCommand headers=fixture_command(OP_HEADERS,0); headers.child=1; headers.deadline=deadline; fixture_do(&rt,0,headers);
  fixture_begin(&rt,0,64,UINT64_C(1)<<63,0,deadline,0);
  fixture_issue(&rt,0,63,2,deadline); fixture_arm(&rt,0,63,deadline,KU_TASK_DRIVER_PENDING);
  fixture_release(&rt,2); fixture_detach(&rt,0); fixture_end(&rt,0,0);
  fixture_begin(&rt,0,1,0,0,deadline,0); /* Empty normal scope and scope ID zero are valid. */
  fixture_do(&rt,0,fixture_command(OP_TOKEN_ERRORS,0)); fixture_end(&rt,0,0);
  fixture_finish(&rt);
}
static void fixture_epoch_exhaustion(void) {
  fixture_clock_begin(); FixtureDriver rt; fixture_init(&rt,1); fixture_start(&rt,0);
  FixtureCommand c=fixture_command(OP_EPOCH,KU_TASK_DRIVER_LIMIT); c.deadline=ku_task_driver_now_ms()+60000u;
  fixture_do(&rt,0,c); fixture_live(0); fixture_finish(&rt);
}
static void fixture_invalid_manifest(unsigned kind) {
  fixture_clock_begin(); FixtureDriver rt; fixture_init(&rt,2); fixture_start(&rt,0); fixture_start(&rt,1);
  uint64_t deadline=ku_task_driver_now_ms()+60000u;
  fixture_begin(&rt,0,2,kind==0 ? 3u : 1u,7,deadline,0);
  FixtureCommand bad=fixture_command(OP_BAD_MANIFEST,KU_TASK_DRIVER_INVALID_ARGUMENT);
  bad.index=kind; bad.child=1; bad.deadline=deadline; fixture_do(&rt,0,bad);
  CHECK(roles[0].scope.driver); /* Rejection must not claim normal scope close. */
  fixture_finish(&rt); /* Genuine owner cleanup, not a repair of the invalid manifest. */
}
static void fixture_timeout(int unissued) {
  fixture_clock_begin(); FixtureDriver rt; fixture_init(&rt,2); fixture_start(&rt,0); fixture_start(&rt,1);
  uint64_t deadline=ku_task_driver_now_ms()+60000u;
  fixture_begin(&rt,0,1,1,17,deadline,0);
  fixture_end(&rt,0,KU_TASK_DRIVER_PENDING);
  if (!unissued) {
    fixture_issue(&rt,0,0,1,deadline); fixture_arm(&rt,0,0,deadline,KU_TASK_DRIVER_PENDING);
    fixture_stable(&rt);
  }
  fixture_advance(&rt,deadline); fixture_idle(&rt); fixture_live(0);
  if (!unissued) {
    CHECK(fixture_wait(0).outcome==KU_TASK_DRIVER_CLEANUP_TIMEOUT);
    fixture_detach(&rt,0);
  }
  fixture_end(&rt,0,KU_TASK_DRIVER_CLEANUP_TIMEOUT); fixture_live(0);
  CHECK(!ku_task_driver_lock(rt.driver));
  CHECK(rt.slots[roles[0].ticket.slot].scope_wait_failure==KU_TASK_DRIVER_CLEANUP_TIMEOUT);
  CHECK(rt.slots[roles[0].ticket.slot].scope_wait_deadline==deadline);
  CHECK(!ku_task_driver_unlock(rt.driver));
  if (!unissued) {
    fixture_release(&rt,1);
    fixture_end(&rt,0,KU_TASK_DRIVER_CLEANUP_TIMEOUT); /* A later ACK cannot erase a latched failure. */
  }
  fixture_live(0); fixture_finish(&rt);
}
typedef struct FixtureCancel {
  KuTaskControlLeaseV1* lease; uint32_t reason,status; uint64_t deadline;
} FixtureCancel;
static int fixture_cancel_thread(void* raw) {
  FixtureCancel* c=(FixtureCancel*)raw;
  c->status=ku_task_control_request_cancel(c->lease,c->reason,c->deadline); return 0;
}
static void fixture_close_race(int close_first,uint32_t reason) {
  fixture_clock_begin(); FixtureDriver rt; fixture_init(&rt,3);
  fixture_start(&rt,0); fixture_start(&rt,1); fixture_start(&rt,2);
  uint64_t deadline=ku_task_driver_now_ms()+60000u,shorter=deadline-10000u;
  fixture_begin(&rt,0,1,1,77,deadline,0); fixture_issue(&rt,0,0,1,deadline);
  fixture_arm(&rt,0,0,deadline,KU_TASK_DRIVER_PENDING); fixture_release(&rt,1); fixture_detach(&rt,0);
  KuTaskControlLeaseV1 observer={0}; fixture_retain(&rt,0,&observer);
  CHECK(!ku_task_driver_lock(rt.driver));
  roles[0].close_gate=close_first ? 2 : 1; roles[0].publishing_gate=1;
  CHECK(!ku_task_driver_unlock(rt.driver));
  size_t resumes=fixture_count(&roles[0].resumes);
  size_t command=fixture_submit(&rt,0,fixture_command(OP_END,close_first ? 0 : KU_TASK_DRIVER_WAIT_ABORTED));
  CHECK(ku_test_event_wait(&roles[0].close_entered,2000u));
  FixtureCancel cancel={&observer,reason,UINT32_MAX,shorter}; KuTestThread thread;
  CHECK(ku_test_thread_start(&thread,fixture_cancel_thread,&cancel));
  CHECK(ku_test_event_wait(&roles[0].publishing,2000u));
  CHECK(ku_task_control_is_publishing(ku_task_control_atomic_load(&observer.control->phase)));
  /* The real CAS has won but publication remains paused. The callback is
   * inside scope_end's driver mutex; direct R2 cancellation uses its genuine
   * retained lease and needs no mutex. Never overwrite the tested phase. */
  CHECK(ku_test_event_set(&roles[0].close_proceed));
  CHECK(ku_test_event_wait(&roles[0].completed[command],2000u));
  CHECK(ku_test_event_set(&roles[0].publish_proceed)); CHECK(ku_test_thread_join(&thread,2000u));
  CHECK(!thread.outcome && cancel.status==KU_TASK_CONTROL_OK);
  /* An actual driver wrapper supplies the notification after direct R2 CAS;
   * opposite reason and a larger D must preserve the original winner. */
  uint32_t opposite=reason==KU_TASK_CONTROL_CANCELLED ? KU_TASK_CONTROL_TIMED_OUT : KU_TASK_CONTROL_CANCELLED;
  CHECK(!ku_task_driver_request_cancel(&roles[0].ticket,&observer,opposite,deadline+10000u));
  fixture_done(&rt,0,command);
  CHECK(roles[0].last_reason==reason && ku_task_control_cleanup_deadline(observer.control)==shorter);
  CHECK(fixture_count(&roles[0].resumes)==resumes+1u); fixture_live(2);
  CHECK(!ku_task_driver_lock(rt.driver));
  KuTaskDriverSlotV1* parent=&rt.slots[roles[0].ticket.slot];
  CHECK(parent->cancel_deadline==deadline+10000u || parent->cancel_deadline==shorter);
  CHECK(parent->scope_session_active==(uint32_t)!close_first);
  CHECK(parent->scope_wait_deadline==(close_first ? UINT64_MAX : shorter));
  CHECK(!ku_task_driver_unlock(rt.driver));
  CHECK(!ku_task_control_lease_release(&observer)); fixture_finish(&rt);
}
static void fixture_cancel_tightens_only_explicit_children(void) {
  fixture_clock_begin(); FixtureDriver rt; fixture_init(&rt,3);
  fixture_start(&rt,0); fixture_start(&rt,1); fixture_start(&rt,2);
  uint64_t deadline=ku_task_driver_now_ms()+60000u,shorter=deadline-10000u;
  fixture_begin(&rt,0,1,1,21,deadline,0); fixture_issue(&rt,0,0,1,deadline);
  fixture_arm(&rt,0,0,deadline,KU_TASK_DRIVER_PENDING);
  CHECK(!ku_task_driver_request_cancel(&roles[0].ticket,&roles[0].handle.owner.lease,KU_TASK_CONTROL_TIMED_OUT,shorter));
  fixture_idle(&rt);
  CHECK(!ku_task_driver_lock(rt.driver));
  KuTaskControlV1* child=rt.slots[roles[1].ticket.slot].driver_lease.control;
  CHECK(child && ku_task_control_cleanup_deadline(child)==deadline);
  CHECK(rt.slots[roles[0].ticket.slot].scope_wait_deadline==shorter);
  CHECK(!ku_task_driver_unlock(rt.driver));
  /* This primitive does not traverse the borrowed manifest from timer paths.
   * A future adapter must propagate the budget with the real existing API. */
  CHECK(!ku_task_driver_cancel_receipt(&roles[0].manifest[0],shorter)); fixture_idle(&rt);
  CHECK(!ku_task_driver_lock(rt.driver));
  child=rt.slots[roles[1].ticket.slot].driver_lease.control;
  CHECK(child && ku_task_control_cleanup_deadline(child)==shorter);
  CHECK(!ku_task_driver_unlock(rt.driver));
  CHECK(roles[1].last_reason==KU_TASK_CONTROL_CANCELLED && roles[0].last_reason==KU_TASK_CONTROL_TIMED_OUT);
  CHECK(!ku_task_driver_request_cancel(&roles[0].ticket,&roles[0].handle.owner.lease,KU_TASK_CONTROL_CANCELLED,deadline+10000u));
  fixture_idle(&rt); fixture_detach(&rt,0); fixture_end(&rt,0,KU_TASK_DRIVER_WAIT_ABORTED);
  fixture_release(&rt,1); fixture_end(&rt,0,KU_TASK_DRIVER_WAIT_ABORTED);
  CHECK(ku_task_control_cleanup_deadline(roles[0].handle.owner.lease.control)==shorter);
  fixture_live(2); fixture_finish(&rt);
}
static void fixture_legacy_and_terminal_reuse(void) {
  fixture_clock_begin(); FixtureDriver rt; fixture_init(&rt,2); fixture_start(&rt,0); fixture_start(&rt,1);
  uint64_t deadline=ku_task_driver_now_ms()+60000u;
  fixture_transfer(1,deadline); fixture_idle(&rt);
  FixtureCommand legacy=fixture_command(OP_LEGACY_ARM,KU_TASK_DRIVER_PENDING); legacy.child=1; legacy.deadline=deadline;
  fixture_do(&rt,0,legacy);
  fixture_begin(&rt,0,1,0,9,deadline,KU_TASK_DRIVER_INVALID_STATE);
  fixture_release(&rt,1); fixture_detach(&rt,0);
  fixture_begin(&rt,0,1,0,9,deadline,KU_TASK_DRIVER_INVALID_STATE); /* Legacy drain D is never reset. */
  fixture_finish(&rt);

  fixture_clock_begin(); fixture_init(&rt,2); fixture_start(&rt,0); fixture_start(&rt,1);
  deadline=ku_task_driver_now_ms()+60000u;
  fixture_begin(&rt,0,1,0,0,deadline,0);
  KuTaskControlLeaseV1 observer={0}; fixture_retain(&rt,0,&observer);
  fixture_transfer(0,deadline); fixture_idle(&rt); fixture_release(&rt,0);
  CHECK(!ku_task_driver_lock(rt.driver));
  KuTaskDriverSlotV1* old=&rt.slots[roles[0].ticket.slot];
  CHECK(old->state==KU_TASK_DRIVER_RETIRING && !old->scope_session_active && !old->scope_receipts);
  CHECK(!old->scope_receipt_capacity && !old->scope_expected_mask && old->scope_session_epoch==1u);
  CHECK(!ku_task_driver_unlock(rt.driver));
  KuTaskDriverScopeTokenV1 token=roles[0].scope;
  CHECK(ku_task_driver_scope_end(&roles[0].ticket,&token)==KU_TASK_DRIVER_INVALID_STATE);
  CHECK(fixture_token_equal(&token,&roles[0].scope));
  CHECK(!ku_task_control_lease_release(&observer)); CHECK(ku_test_event_wait(&roles[0].disposed,2000u));
  fixture_start(&rt,2);
  CHECK(roles[2].ticket.slot==roles[0].ticket.slot && roles[2].ticket.generation>roles[0].ticket.generation);
  CHECK(!ku_task_driver_lock(rt.driver));
  CHECK(!rt.slots[roles[2].ticket.slot].scope_session_epoch && !rt.slots[roles[2].ticket.slot].scope_receipts);
  roles[2].old_scope=roles[0].scope; CHECK(!ku_task_driver_unlock(rt.driver));
  fixture_begin(&rt,2,1,0,0,deadline,0);
  fixture_do(&rt,2,fixture_command(OP_TOKEN_ERRORS,0)); fixture_end(&rt,2,0);
  fixture_live(1); fixture_finish(&rt);
}
/* Public wait_idle reports sticky INTERNAL after a real clock fault, so it
 * cannot certify quiescence. Wait on the real driver's OS condition predicate
 * with this healthy caller's clock, retaining the existing 2s test bound.
 * This helper only observes state; it never clears fault or manufactures ACK. */
static KuTaskDriverSnapshotV1 fixture_rejected_idle(FixtureDriver* runtime,int faulted,int exited) {
  CHECK(!ku_task_driver_lock(runtime->driver));
  uint64_t deadline=fixture_original_now()+2000u;
  while (runtime->driver->queued || runtime->driver->running
      || (exited ? !runtime->driver->worker_exited
                 : !runtime->driver->worker_waiting && !runtime->driver->worker_exited)) {
    CHECK(ku_task_driver_wait(runtime->driver,deadline)>=0);
    CHECK(fixture_original_now()<deadline);
  }
  CHECK(!ku_task_driver_unlock(runtime->driver));
  KuTaskDriverSnapshotV1 snapshot={0}; CHECK(!ku_task_driver_snapshot(runtime->driver,&snapshot));
  CHECK(snapshot.closing && snapshot.clock_fault==(uint32_t)faulted);
  CHECK(snapshot.fault==(faulted ? KU_TASK_DRIVER_INTERNAL : KU_TASK_DRIVER_OK));
  CHECK(!snapshot.queued && !snapshot.running);
  CHECK(exited ? snapshot.worker_exited : snapshot.worker_waiting || snapshot.worker_exited);
  return snapshot;
}
static void fixture_rejected_do(FixtureDriver* runtime,size_t parent,FixtureCommand command,int faulted) {
  size_t index=fixture_submit(runtime,parent,command);
  CHECK(ku_test_event_wait(&roles[parent].completed[index],2000u));
  fixture_rejected_idle(runtime,faulted,0);
  CHECK(roles[parent].finished==index+1u);
}
static void fixture_rejected_finish(FixtureDriver* runtime,int faulted) {
  for (size_t i=0;i<ROLE_COUNT;i++) if (roles[i].initialized) {
    FixtureRole* r=&roles[i];
    if (r->handle.owner.lease.control) fixture_transfer(i,fixture_original_now()+2000u);
    fixture_rejected_idle(runtime,faulted,0);
    if (!fixture_count(&r->acks)) {
      CHECK(!ku_task_driver_lock(runtime->driver));
      CHECK(runtime->slots[r->ticket.slot].state!=KU_TASK_DRIVER_RUNNING);
      CHECK(!runtime->slots[r->ticket.slot].cleanup_fault);
      r->cleanup_hold=0; CHECK(!ku_task_driver_unlock(runtime->driver));
      CHECK(!ku_task_driver_wake(&r->ticket));
      CHECK(ku_test_event_wait(&r->acked,2000u));
      CHECK(ku_task_driver_cleanup_receipt_read(&r->receipt)==KU_TASK_DRIVER_CLEANUP_ACK);
    }
    CHECK(ku_test_event_wait(&r->disposed,2000u));
    CHECK(fixture_count(&r->frame_drops)==1u && fixture_count(&r->acks)==1u
        && fixture_count(&r->disposes)==1u);
    fixture_rejected_idle(runtime,faulted,0);
  }
  KuTaskDriverSnapshotV1 snapshot=fixture_rejected_idle(runtime,faulted,1);
  CHECK(!snapshot.resident && !snapshot.building && !snapshot.reserved_bytes
      && !snapshot.parked && !snapshot.retiring && !snapshot.terminal_held);
  for (size_t i=0;i<runtime->capacity;i++) {
    CHECK(!runtime->slots[i].scope_session_active && !runtime->slots[i].scope_receipts
        && !runtime->slots[i].scope_receipt_capacity && !runtime->slots[i].scope_expected_mask);
  }
  uint64_t deadline=fixture_original_now()+2000u;
  CHECK(ku_task_driver_wait_idle(runtime->driver,deadline)==(faulted ? KU_TASK_DRIVER_INTERNAL : KU_TASK_DRIVER_OK));
  CHECK(ku_task_driver_shutdown(runtime->driver,deadline)==(faulted ? KU_TASK_DRIVER_INTERNAL : KU_TASK_DRIVER_OK));
  fixture_destroy_storage(runtime);
}
static void fixture_fault_or_closing_session(int faulted) {
  /* Real time throughout this scenario: no frozen-clock waits, deadline
   * renewal, sleeps, or synthetic assignments to driver fault/closing. */
  ku_task_control_deadline_store(&fixture_clock,0);
  FixtureDriver rt; fixture_init(&rt,3);
  fixture_start(&rt,0); fixture_start(&rt,1); fixture_start(&rt,2);
  uint64_t original=fixture_original_now()+60000u;
  fixture_begin(&rt,0,1,1,301u,original,KU_TASK_DRIVER_OK);
  fixture_issue(&rt,0,0,1,original);
  fixture_arm(&rt,0,0,original,KU_TASK_DRIVER_PENDING);
  fixture_release(&rt,1); fixture_detach(&rt,0);
  CHECK(ku_test_event_wait(&roles[1].disposed,2000u));
  /* Choose a different real original reason before shutdown's Cancelled.
   * The live owner's cleanup remains held by this bounded test callback. */
  CHECK(!ku_task_driver_request_cancel(&roles[0].ticket,&roles[0].handle.owner.lease,
                                      KU_TASK_CONTROL_TIMED_OUT,original));
  fixture_idle(&rt);
  CHECK(roles[0].last_reason==KU_TASK_CONTROL_TIMED_OUT);
  size_t parent_resumes=fixture_count(&roles[0].resumes),other_resumes=fixture_count(&roles[2].resumes);
  KuTaskDriverScopeTokenV1 token=roles[0].scope;
  uint64_t shutdown_deadline=fixture_original_now();
  if (faulted) {
    fixture_failed_clock_reads=0; fixture_fail_clock=1;
    uint32_t result=ku_task_driver_shutdown(rt.driver,shutdown_deadline);
    fixture_fail_clock=0;
    CHECK(result==KU_TASK_DRIVER_INTERNAL && fixture_failed_clock_reads==1u);
  } else {
    /* Deadline now is an intentional immediate shutdown refusal, not a
     * timing race: the public API itself closes admission and requests cancel. */
    CHECK(ku_task_driver_shutdown(rt.driver,shutdown_deadline)==KU_TASK_DRIVER_SHUTDOWN_TIMEOUT);
  }
  KuTaskDriverSnapshotV1 snapshot=fixture_rejected_idle(&rt,faulted,0);
  CHECK(snapshot.resident==2u && !snapshot.building && !snapshot.worker_exited);
  CHECK(ku_task_driver_destroy(rt.driver)==KU_TASK_DRIVER_PENDING);
  uint64_t expected=faulted ? 0u : shutdown_deadline;
  CHECK(!ku_task_driver_lock(rt.driver));
  KuTaskDriverSlotV1* parent=&rt.slots[roles[0].ticket.slot];
  CHECK(parent->scope_session_active && parent->scope_receipts==roles[0].manifest
      && parent->scope_receipt_capacity==1u && parent->scope_expected_mask==1u);
  CHECK(parent->scope_wait_deadline==expected && parent->scope_session_epoch==token.epoch);
  CHECK(parent->cancel_deadline==expected && !parent->cleanup_fault);
  CHECK(rt.driver->shutdown_deadline==expected);
  CHECK(!ku_task_driver_unlock(rt.driver));
  CHECK(ku_task_control_cleanup_deadline(roles[0].handle.owner.lease.control)==expected);
  CHECK(roles[0].last_reason==KU_TASK_CONTROL_TIMED_OUT
      && roles[2].last_reason==KU_TASK_CONTROL_CANCELLED);
  FixtureCommand begin=fixture_command(OP_REJECTED_BEGIN,
      faulted ? KU_TASK_DRIVER_INTERNAL : KU_TASK_DRIVER_WAIT_ABORTED);
  begin.id=302u; begin.deadline=original;
  fixture_rejected_do(&rt,2,begin,faulted);
  fixture_rejected_do(&rt,0,fixture_command(OP_END,
      faulted ? KU_TASK_DRIVER_INTERNAL : KU_TASK_DRIVER_WAIT_ABORTED),faulted);
  CHECK(fixture_token_equal(&roles[0].scope,&token));
  CHECK(fixture_count(&roles[0].resumes)==parent_resumes && fixture_count(&roles[2].resumes)==other_resumes);
  CHECK(roles[0].last_reason==KU_TASK_CONTROL_TIMED_OUT
      && ku_task_control_cleanup_deadline(roles[0].handle.owner.lease.control)==expected);
  CHECK(!ku_task_driver_lock(rt.driver));
  parent=&rt.slots[roles[0].ticket.slot];
  CHECK(parent->scope_session_active && parent->scope_receipts==roles[0].manifest
      && parent->scope_wait_deadline==expected && !parent->cleanup_fault);
  CHECK(!ku_task_driver_unlock(rt.driver));
  /* This is the recoverable clock-fault closing path, not a quarantined
   * cleanup invariant violation. Real owner retirement/ACK/free is required;
   * no control pin, logical ACK, or sticky error is patched to force success. */
  fixture_rejected_finish(&rt,faulted);
}
int main(void) {
  ku_task_control_deadline_init(&fixture_clock); ku_task_control_deadline_store(&fixture_clock,0);
  fixture_normal_scopes(); fixture_headers_and_high_bit(); fixture_epoch_exhaustion();
  for (unsigned kind=0;kind<3u;kind++) fixture_invalid_manifest(kind);
  fixture_timeout(0); fixture_timeout(1);
  fixture_close_race(0,KU_TASK_CONTROL_CANCELLED); fixture_close_race(1,KU_TASK_CONTROL_CANCELLED);
  fixture_close_race(0,KU_TASK_CONTROL_TIMED_OUT); fixture_close_race(1,KU_TASK_CONTROL_TIMED_OUT);
  fixture_cancel_tightens_only_explicit_children(); fixture_legacy_and_terminal_reuse();
  fixture_fault_or_closing_session(1); fixture_fault_or_closing_session(0);
  puts("task-scope-session-v1-ok"); return 0;
}
"#;
