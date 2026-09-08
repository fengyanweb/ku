//! Known-header alias/provenance rejection on real generated hosted Tasks.
//! Invalid raw requests never supply a replacement Start/Await implementation.
#[path = "support/native_allocation_harness.rs"]
mod native_allocation_harness;
#[allow(dead_code)]
#[path = "support/native_pg_harness.rs"]
mod native_harness;
#[path = "support/native_task_ledger.rs"]
mod native_task_ledger;

use ku::{
    backend::c, checker::Checker, ir::task::TaskTerminator, ir::task_lower, lexer::Lexer,
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
        "start alias hook: {anchor}"
    );
    source.replacen(anchor, replacement, 1)
}

#[test]
fn native_task_start_known_alias_provenance_and_take_retry_execute_in_c() {
    let source = r#"
async fn Parent(value: str): str! { child = Child(value) return await child }
async fn Child(value: str): str! { return ok(value) }
async fn Number(value: int): int! { return ok(value) }
async fn main(): null! { return ok(null) }
"#;
    let ast = Parser::new(Lexer::new(source).lex().unwrap())
        .parse_program()
        .unwrap();
    Checker::new().check(&ast).unwrap();
    let native = task_lower::lower_program(&ast).unwrap();
    for (index, name) in ["Parent", "Child", "Number", "main"].iter().enumerate() {
        assert_eq!(native.tasks.functions[index].name, *name);
    }
    let awaits: Vec<_> = native.tasks.functions[0]
        .states
        .iter()
        .filter_map(|state| {
            if let TaskTerminator::Await { task, .. } = &state.terminator {
                Some(task.0)
            } else {
                None
            }
        })
        .collect();
    assert_eq!(awaits.len(), 1);
    let generated = c::generate_native_task_c_source(&native, &Default::default()).unwrap();
    assert!(!generated.contains("run_source") && !generated.contains("const SOURCE"));
    let instrumentation = format!("{NATIVE_THREAD_LIFECYCLE_HARNESS}\n{LEDGER_LOCK}\n{ALLOCATION_HOOK}\n{LOCKED_ALLOCATIONS}\nstatic void fixture_probe(void*);\n");
    let generated = replace_once(
        generated,
        "typedef struct KuString {",
        &format!("{instrumentation}typedef struct KuString {{"),
    );
    let generated = replace_once(
        generated,
        "static uint32_t ku_task_0_resume(void* raw) {",
        "static uint32_t ku_task_0_resume(void* raw) {\n  fixture_probe(raw);",
    );
    let generated = replace_once(generated, "static uint32_t ku_task_driver_commit_child(", "static uint32_t ku_task_driver_commit_child(const KuTaskDriverTicketV1*,KuTaskControlOwnerV1*,const KuTaskDriverStartChargeV1*,size_t);\nstatic uint32_t fixture_real_commit_child(");
    let mut generated = replace_once(
        generated,
        "int main(void) {",
        "static int fixture_unmodified_source_main(void) {",
    );
    generated.push_str(&C_MAIN.replace("@AWAIT@", &format!("s_{}", awaits[0])));
    let directory = TempDir::new("native-task-start-alias");
    let path = directory.path().join("program.c");
    fs::write(&path, generated).unwrap();
    let Some(executable) = compile_harness(directory.path(), &path, "program") else {
        assert!(
            std::env::var_os("GITHUB_ACTIONS").is_none(),
            "CI must execute hosted Start alias and provenance rejection"
        );
        return;
    };
    fs::remove_file(path).unwrap();
    let output = run_bounded(
        Command::new(executable).current_dir(directory.path()),
        RUN_TIMEOUT,
        RUN_LIMITS,
    )
    .expect("Start alias fixture terminates under the real process watchdog");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8(output.stdout).unwrap().replace('\r', ""),
        "task-start-alias-ok\n"
    );
    assert!(output.stderr.is_empty());
}

const C_MAIN: &str = r#"
typedef struct FixtureRuntime {
  KuTaskDriverV1* driver;
  KuTaskDriverSlotV1* slots;
  size_t* ring;
  size_t capacity;
} FixtureRuntime;
typedef struct FixtureAccount {
  size_t resident,building,bytes,queued,running,head,calls;
  uint64_t generation[3],acked[3];
  size_t charge[3],references[3],wrapper[3];
  uint32_t state[3],owner[3];
  KuTaskControlV1* binding[3];
} FixtureAccount;
static FixtureRuntime fixture_other;
static KuTaskValueV1 fixture_other_task;
static int fixture_mode,fixture_probed,fixture_retried,fixture_committed;
static size_t fixture_rejections;

static FixtureAccount fixture_account(KuTaskDriverV1* driver) {
  FixtureAccount out={0}; out.calls=fixture_ledger().calls;
  CHECK(driver->capacity<=3u && !ku_task_driver_lock(driver));
  out.resident=driver->resident; out.building=driver->building;
  out.bytes=driver->reserved_bytes; out.queued=driver->queued;
  out.running=driver->running; out.head=driver->head;
  size_t sum=0;
  for (size_t i=0;i<driver->capacity;i++) {
    KuTaskDriverSlotV1* slot=&driver->slots[i];
    out.generation[i]=slot->generation; out.acked[i]=slot->cleanup_acked_generation;
    out.charge[i]=slot->charged_bytes; out.state[i]=slot->state;
    out.owner[i]=slot->owner_location; out.wrapper[i]=slot->wrapper_active;
    out.binding[i]=slot->binding;
    out.references[i]=slot->binding ? ku_task_control_atomic_load(&slot->binding->references) : 0;
    CHECK(slot->charged_bytes<=SIZE_MAX-sum); sum+=slot->charged_bytes;
  }
  CHECK(sum==driver->reserved_bytes); CHECK(!ku_task_driver_unlock(driver));
  return out;
}
static void fixture_same(KuTaskDriverV1* driver,const FixtureAccount* before,uint32_t fault) {
  FixtureAccount after=fixture_account(driver);
  CHECK(after.resident==before->resident && after.building==before->building && after.bytes==before->bytes);
  CHECK(after.queued==before->queued && after.running==before->running && after.head==before->head && after.calls==before->calls);
  for (size_t i=0;i<driver->capacity;i++) {
    CHECK(after.generation[i]==before->generation[i] && after.acked[i]==before->acked[i]);
    CHECK(after.charge[i]==before->charge[i] && after.state[i]==before->state[i]);
    CHECK(after.owner[i]==before->owner[i] && after.wrapper[i]==before->wrapper[i]);
    CHECK(after.binding[i]==before->binding[i] && after.references[i]==before->references[i]);
  }
  CHECK(!ku_task_driver_lock(driver)); CHECK(driver->fault==fault); CHECK(!ku_task_driver_unlock(driver));
  fixture_rejections++;
}
static void fixture_string_unchanged(KuString* input,uint8_t* pointer) {
  CHECK(input->ptr==pointer && input->len==1u && input->capacity==1u && input->storage==KU_STRING_OWNED);
  CHECK(!input->ptr[0]);
}
static void fixture_ticket_empty(KuTaskDriverTicketV1* output) {
  CHECK(!output->driver && !output->slot && !output->generation);
}
static void fixture_value_empty(KuTaskValueV1* output) {
  CHECK(!output->tag && !output->result_kind && !output->function_id && !output->rejection_code);
  CHECK(!output->owner.lease.control); fixture_ticket_empty(&output->ticket);
}
static void fixture_start_aliases(KuTaskInstance_0* instance) {
  KuTaskDriverV1* driver=instance->ticket.driver;
  KuTaskAdapterHostV1 host={&instance->ticket,&instance->control,&instance->wait,sizeof(*instance)};
  KuString* input=&instance->frame.s_0; uint8_t* pointer=input->ptr;
  KuTaskValueV1 output={0}; int64_t zero=0;
  FixtureAccount before=fixture_account(driver);
  /* These short allocations/zero headers must be rejected BEFORE any read of
   * the larger aliased host, ticket, source or output structure. The one-byte
   * case is sanitizer-sensitive; zero-int prevents pointer-content masking. */
  CHECK(ku_task_2_start_hosted((const KuTaskAdapterHostV1*)&zero,&zero,&output)==KU_TASK_DRIVER_INVALID_ARGUMENT);
  fixture_same(driver,&before,0); CHECK(!zero); fixture_value_empty(&output);
  CHECK(ku_task_2_start_hosted(&host,&zero,(KuTaskValueV1*)&zero)==KU_TASK_DRIVER_INVALID_ARGUMENT);
  fixture_same(driver,&before,0); CHECK(!zero); fixture_value_empty(&output);
  CHECK(ku_task_1_start_hosted((const KuTaskAdapterHostV1*)pointer,input,&output)==KU_TASK_DRIVER_INVALID_ARGUMENT);
  fixture_same(driver,&before,0); fixture_string_unchanged(input,pointer);
  CHECK(ku_task_1_start_hosted(&host,input,(KuTaskValueV1*)pointer)==KU_TASK_DRIVER_INVALID_ARGUMENT);
  fixture_same(driver,&before,0); fixture_string_unchanged(input,pointer);
  CHECK(ku_task_1_start_hosted(&host,input,(KuTaskValueV1*)input)==KU_TASK_DRIVER_INVALID_ARGUMENT);
  fixture_same(driver,&before,0); fixture_string_unchanged(input,pointer);
  CHECK(ku_task_1_start_hosted((const KuTaskAdapterHostV1*)&output,input,&output)==KU_TASK_DRIVER_INVALID_ARGUMENT);
  fixture_same(driver,&before,0); fixture_value_empty(&output);
  KuTaskAdapterHostV1 bad=host; bad.ticket=(const KuTaskDriverTicketV1*)&zero;
  CHECK(ku_task_2_start_hosted(&bad,&zero,&output)==KU_TASK_DRIVER_INVALID_ARGUMENT);
  fixture_same(driver,&before,0); CHECK(!zero); fixture_value_empty(&output);
  bad=host; bad.ticket=(const KuTaskDriverTicketV1*)pointer;
  CHECK(ku_task_1_start_hosted(&bad,input,&output)==KU_TASK_DRIVER_INVALID_ARGUMENT);
  fixture_same(driver,&before,0); fixture_string_unchanged(input,pointer);
  CHECK(ku_task_1_start_hosted(&host,input,(KuTaskValueV1*)&instance->ticket)==KU_TASK_DRIVER_INVALID_ARGUMENT);
  fixture_same(driver,&before,0);
  CHECK(ku_task_1_start_hosted(&host,input,(KuTaskValueV1*)&instance->control)==KU_TASK_DRIVER_INVALID_ARGUMENT);
  fixture_same(driver,&before,0);
  CHECK(ku_task_1_start_hosted(&host,input,(KuTaskValueV1*)driver)==KU_TASK_DRIVER_INVALID_ARGUMENT);
  fixture_same(driver,&before,0);
  CHECK(ku_task_1_start_value_impl(driver,(const KuTaskDriverStartChargeV1*)pointer,input,&output)==KU_TASK_DRIVER_INVALID_ARGUMENT);
  fixture_same(driver,&before,0); fixture_string_unchanged(input,pointer);
  CHECK(ku_task_2_start_value_impl(driver,(const KuTaskDriverStartChargeV1*)&zero,&zero,&output)==KU_TASK_DRIVER_INVALID_ARGUMENT);
  fixture_same(driver,&before,0); CHECK(!zero);
  CHECK(ku_task_1_start_value_impl(driver,(const KuTaskDriverStartChargeV1*)&output,input,&output)==KU_TASK_DRIVER_INVALID_ARGUMENT);
  fixture_same(driver,&before,0); fixture_value_empty(&output);
  CHECK(ku_task_1_start_value_impl(driver,(const KuTaskDriverStartChargeV1*)driver,input,&output)==KU_TASK_DRIVER_INVALID_ARGUMENT);
  fixture_same(driver,&before,0); fixture_string_unchanged(input,pointer);
  fixture_value_empty(&output);
}
static void fixture_reserve_rejections(KuTaskInstance_0* instance) {
  KuTaskDriverV1* driver=instance->ticket.driver;
  KuTaskDriverStartChargeV1 source={instance->ticket,&instance->control,sizeof(*instance),1u};
  KuTaskDriverTicketV1 output={0}; FixtureAccount before=fixture_account(driver);
  CHECK(ku_task_driver_reserve_child(driver,(const KuTaskDriverStartChargeV1*)&output,sizeof(KuTaskInstance_1),&output)==KU_TASK_DRIVER_INVALID_ARGUMENT);
  fixture_same(driver,&before,0); fixture_ticket_empty(&output);
  CHECK(ku_task_driver_reserve_child(driver,&source,sizeof(KuTaskInstance_1),(KuTaskDriverTicketV1*)&source)==KU_TASK_DRIVER_INVALID_ARGUMENT);
  fixture_same(driver,&before,0);
  CHECK(ku_task_driver_reserve_child(driver,(const KuTaskDriverStartChargeV1*)driver,sizeof(KuTaskInstance_1),&output)==KU_TASK_DRIVER_INVALID_ARGUMENT);
  fixture_same(driver,&before,0); fixture_ticket_empty(&output);
  CHECK(ku_task_driver_reserve_child(driver,&source,sizeof(KuTaskInstance_1),(KuTaskDriverTicketV1*)driver->ring)==KU_TASK_DRIVER_INVALID_ARGUMENT);
  fixture_same(driver,&before,0);
  CHECK(ku_task_driver_reserve_child(driver,&source,sizeof(KuTaskInstance_1),(KuTaskDriverTicketV1*)&instance->control)==KU_TASK_DRIVER_INVALID_ARGUMENT);
  fixture_same(driver,&before,0);
  KuTaskDriverStartChargeV1 bad=source; bad.parent_control=(KuTaskControlV1*)&bad;
  CHECK(ku_task_driver_reserve_child(driver,&bad,sizeof(KuTaskInstance_1),&output)==KU_TASK_DRIVER_INVALID_ARGUMENT);
  fixture_same(driver,&before,0); fixture_ticket_empty(&output);
  bad=source; bad.parent.generation++;
  CHECK(ku_task_driver_reserve_child(driver,&bad,sizeof(KuTaskInstance_1),&output)==KU_TASK_DRIVER_STALE);
  fixture_same(driver,&before,0); fixture_ticket_empty(&output);
  bad=source; bad.parent.slot=2u;
  CHECK(ku_task_driver_reserve_child(driver,&bad,sizeof(KuTaskInstance_1),&output)==KU_TASK_DRIVER_STALE);
  fixture_same(driver,&before,0); fixture_ticket_empty(&output);
  bad=source; bad.parent_control=fixture_other_task.owner.lease.control;
  CHECK(ku_task_driver_reserve_child(driver,&bad,sizeof(KuTaskInstance_1),&output)==KU_TASK_DRIVER_INVALID_STATE);
  fixture_same(driver,&before,0); fixture_ticket_empty(&output);
  bad=source; bad.parent=fixture_other_task.ticket; bad.parent_control=fixture_other_task.owner.lease.control;
  CHECK(ku_task_driver_reserve_child(driver,&bad,sizeof(KuTaskInstance_1),&output)==KU_TASK_DRIVER_INVALID_ARGUMENT);
  fixture_same(driver,&before,0); fixture_ticket_empty(&output);
  bad.owned_bytes=0;
  CHECK(ku_task_driver_reserve_child(driver,&bad,sizeof(KuTaskInstance_2),&output)==KU_TASK_DRIVER_INVALID_ARGUMENT);
  fixture_same(driver,&before,0); fixture_ticket_empty(&output);
  bad=source; bad.owned_bytes=0; bad.parent_control=fixture_other_task.owner.lease.control;
  CHECK(ku_task_driver_reserve_child(driver,&bad,sizeof(KuTaskInstance_2),&output)==KU_TASK_DRIVER_INVALID_STATE);
  fixture_same(driver,&before,0); fixture_ticket_empty(&output);
  bad=source; bad.minimum_parent_bytes=0;
  CHECK(ku_task_driver_reserve_child(driver,&bad,sizeof(KuTaskInstance_1),&output)==KU_TASK_DRIVER_INVALID_ARGUMENT);
  fixture_same(driver,&before,0); fixture_ticket_empty(&output);
  CHECK(ku_task_driver_reserve_child(driver,&source,0,&output)==KU_TASK_DRIVER_INVALID_ARGUMENT);
  fixture_same(driver,&before,0); fixture_ticket_empty(&output);
  CHECK(ku_task_driver_reserve_child(driver,&source,SIZE_MAX,&output)==KU_TASK_DRIVER_LIMIT);
  fixture_same(driver,&before,0); fixture_ticket_empty(&output);
  if (fixture_mode) {
    bad=source;
    if (fixture_mode==1) { bad.minimum_parent_bytes=sizeof(*instance)+2u; bad.owned_bytes=0; }
    else bad.owned_bytes=2u;
    CHECK(ku_task_driver_reserve_child(driver,&bad,sizeof(KuTaskInstance_1),&output)==KU_TASK_DRIVER_INTERNAL);
    fixture_same(driver,&before,KU_TASK_DRIVER_INTERNAL); fixture_ticket_empty(&output);
    /* The failure is sticky, not repaired. Request the real parent's cleanup
     * after proving no partial admission; the original adapter drops its input. */
    uint64_t cancel_now=ku_task_driver_now_ms();
    CHECK(cancel_now!=UINT64_MAX && cancel_now<UINT64_MAX-1000u);
    CHECK(ku_task_driver_cancel_bound(&instance->ticket,KU_TASK_CONTROL_CANCELLED,cancel_now+1000u)==KU_TASK_CONTROL_OK);
  }
}
static void fixture_probe(void* raw) {
  KuTaskInstance_0* instance=(KuTaskInstance_0*)raw;
  if (!fixture_probed) {
    fixture_start_aliases(instance); fixture_reserve_rejections(instance);
    fixture_probed=1; return;
  }
  if (fixture_mode || fixture_retried) return;
  /* Only the genuine second resume is eligible: the source Await owns this
   * hidden Task and its result-ready registration has already notified. */
  KuTaskValueV1* child=&instance->frame.@AWAIT@;
  CHECK(child->tag==KU_TASK_VALUE_LIVE && instance->wait.driver);
  KuTaskDriverWaitSnapshotV1 waiting={0};
  CHECK(ku_task_driver_wait_read(&instance->wait,&waiting)==KU_TASK_DRIVER_OK);
  CHECK(waiting.kind==KU_TASK_DRIVER_WAIT_KIND_RESULT && waiting.state==KU_TASK_DRIVER_WAIT_NOTIFIED && waiting.outcome==KU_TASK_DRIVER_WAIT_READY);
  KuTaskDriverTicketV1 bad_parent=instance->ticket; bad_parent.generation++;
  KuTaskAdapterOutcomeV1 output={0}; KuTaskAdapterTakeRequestV1 request={&output,&bad_parent};
  FixtureAccount before=fixture_account(instance->ticket.driver);
  CHECK(before.state[child->ticket.slot]==KU_TASK_DRIVER_TERMINAL_HELD);
  CHECK(ku_task_driver_take_result(&child->ticket,&child->owner.lease,&request)==KU_TASK_DRIVER_STALE);
  CHECK(ku_task_outcome_empty(&output,4u));
  CHECK(ku_task_control_atomic_load(&child->owner.lease.control->payload)==KU_TASK_CONTROL_PAYLOAD_AVAILABLE);
  /* wrapper_end legitimately queues a terminal child after the final AVAILABLE
   * publication. This single worker cannot run it until this callback returns;
   * allow that notification only, never a charge, owner or reference change. */
  before.state[child->ticket.slot]=KU_TASK_DRIVER_QUEUED; before.queued++;
  fixture_same(instance->ticket.driver,&before,0);
  fixture_retried=1; /* The original generated Await below performs the retry. */
}
static uint32_t ku_task_driver_commit_child(const KuTaskDriverTicketV1* ticket,
    KuTaskControlOwnerV1* owner,const KuTaskDriverStartChargeV1* source,size_t bytes) {
  CHECK(!fixture_mode && !fixture_committed);
  KuTaskDriverV1* driver=ticket->driver;
  KuTaskInstance_1* child=(KuTaskInstance_1*)owner->lease.control;
  uint8_t* input=child->frame.s_0.ptr; CHECK(input && !input[0]);
  uint64_t initialized=child->frame.header.initialized;
  size_t refs=ku_task_control_atomic_load(&owner->lease.control->references);
  FixtureAccount before=fixture_account(driver);
  CHECK(fixture_real_commit_child(ticket,(KuTaskControlOwnerV1*)source,source,bytes)==KU_TASK_DRIVER_INVALID_ARGUMENT);
  fixture_same(driver,&before,0);
  CHECK(fixture_real_commit_child((const KuTaskDriverTicketV1*)source,owner,source,bytes)==KU_TASK_DRIVER_INVALID_ARGUMENT);
  fixture_same(driver,&before,0);
  CHECK(fixture_real_commit_child(ticket,owner,(const KuTaskDriverStartChargeV1*)owner,bytes)==KU_TASK_DRIVER_INVALID_ARGUMENT);
  fixture_same(driver,&before,0);
  KuTaskDriverStartChargeV1 bad=*source; bad.parent.generation++;
  CHECK(fixture_real_commit_child(ticket,owner,&bad,bytes)==KU_TASK_DRIVER_STALE);
  fixture_same(driver,&before,0);
  bad=*source; bad.parent_control=fixture_other_task.owner.lease.control;
  CHECK(fixture_real_commit_child(ticket,owner,&bad,bytes)==KU_TASK_DRIVER_INVALID_STATE);
  fixture_same(driver,&before,0);
  bad=*source; bad.parent=fixture_other_task.ticket; bad.parent_control=fixture_other_task.owner.lease.control;
  CHECK(fixture_real_commit_child(ticket,owner,&bad,bytes)==KU_TASK_DRIVER_INVALID_ARGUMENT);
  fixture_same(driver,&before,0);
  bad=*source; bad.parent=*ticket; bad.parent_control=owner->lease.control;
  CHECK(fixture_real_commit_child(ticket,owner,&bad,bytes)==KU_TASK_DRIVER_INVALID_ARGUMENT);
  fixture_same(driver,&before,0);
  CHECK(fixture_real_commit_child(ticket,owner,source,bytes+1u)==KU_TASK_DRIVER_INVALID_STATE);
  fixture_same(driver,&before,0);
  CHECK(child->frame.s_0.ptr==input && child->frame.header.initialized==initialized);
  CHECK(ku_task_control_atomic_load(&owner->lease.control->references)==refs);
  uint32_t status=fixture_real_commit_child(ticket,owner,source,bytes);
  CHECK(status==KU_TASK_DRIVER_OK);
  before=fixture_account(driver); refs=ku_task_control_atomic_load(&owner->lease.control->references);
  CHECK(fixture_real_commit_child(ticket,owner,source,bytes)==KU_TASK_DRIVER_INVALID_STATE);
  fixture_same(driver,&before,0);
  CHECK(ku_task_control_atomic_load(&owner->lease.control->references)==refs);
  CHECK(child->frame.s_0.ptr==input && child->frame.header.initialized==initialized);
  fixture_committed=1; return status;
}
static void fixture_init(FixtureRuntime* runtime,size_t capacity) {
  memset(runtime,0,sizeof(*runtime)); runtime->capacity=capacity;
  runtime->driver=(KuTaskDriverV1*)calloc(1,sizeof(*runtime->driver));
  runtime->slots=(KuTaskDriverSlotV1*)calloc(capacity,sizeof(*runtime->slots));
  runtime->ring=(size_t*)calloc(capacity,sizeof(*runtime->ring)); CHECK(runtime->driver && runtime->slots && runtime->ring);
  size_t fixed=sizeof(*runtime->driver)+capacity*(sizeof(*runtime->slots)+sizeof(*runtime->ring));
  size_t tasks=sizeof(KuTaskInstance_0)+sizeof(KuTaskInstance_1)+sizeof(KuTaskInstance_2)+1u;
  uint64_t startup_now=ku_task_driver_now_ms();
  CHECK(startup_now!=UINT64_MAX && startup_now<UINT64_MAX-1000u);
  uint64_t startup_deadline=startup_now+1000u;
  CHECK(ku_task_driver_init(runtime->driver,sizeof(*runtime->driver),6u,
      runtime->slots,capacity,runtime->ring,capacity,fixed+tasks,1u,startup_deadline)==KU_TASK_DRIVER_ABI_MISMATCH);
  CHECK(ku_task_frame_zero_bytes(runtime->driver,sizeof(*runtime->driver))
      && ku_task_frame_zero_bytes(runtime->slots,(capacity)*sizeof(*runtime->slots))
      && ku_task_frame_zero_bytes(runtime->ring,(capacity)*sizeof(*runtime->ring)));
  CHECK(ku_task_driver_init(runtime->driver,sizeof(*runtime->driver),KU_TASK_DRIVER_ABI_VERSION,
      runtime->slots,capacity,runtime->ring,capacity,fixed+tasks,1u,startup_deadline)==KU_TASK_DRIVER_OK);
}
static void fixture_finish(FixtureRuntime* runtime,uint32_t fault,uint64_t deadline) {
  CHECK(ku_task_driver_shutdown(runtime->driver,deadline)==fault);
  KuTaskDriverSnapshotV1 snapshot={0};
  CHECK(ku_task_driver_snapshot(runtime->driver,&snapshot)==KU_TASK_DRIVER_OK);
  CHECK(snapshot.fault==fault && !snapshot.resident && !snapshot.reserved_bytes && !snapshot.building && !snapshot.running && !snapshot.queued);
  CHECK(ku_task_driver_join(runtime->driver,deadline)==KU_TASK_DRIVER_OK);
  CHECK(runtime->driver->worker_target==1u && runtime->driver->workers_created==1u && runtime->driver->workers_exited==1u
      && runtime->driver->workers_joined==1u && runtime->driver->workers[0].joined && runtime->driver->workers[0].closed);
  CHECK(ku_task_driver_destroy(runtime->driver)==KU_TASK_DRIVER_OK);
  free(runtime->ring); free(runtime->slots); free(runtime->driver);
}
static void fixture_case(int mode) {
  CHECK(!fixture_ledger().allocations && !fixture_ledger().bytes);
  fixture_mode=mode; fixture_probed=0; fixture_retried=0; fixture_committed=0; fixture_rejections=0;
  fixture_init(&fixture_other,1); fixture_other_task=(KuTaskValueV1){0};
  int64_t number=0;
  CHECK(ku_task_2_start_value(fixture_other.driver,&number,&fixture_other_task)==KU_TASK_DRIVER_OK);
  CHECK(fixture_other_task.tag==KU_TASK_VALUE_LIVE);
  CHECK(ku_task_driver_wait_idle(fixture_other.driver,ku_task_driver_now_ms()+2000u)==KU_TASK_DRIVER_OK);
  FixtureRuntime runtime; fixture_init(&runtime,3);
  uint8_t* pointer=(uint8_t*)malloc(1u); CHECK(pointer); pointer[0]=0;
  KuString input={pointer,1u,1u,KU_STRING_OWNED}; KuTaskValueV1 parent={0};
  CHECK(ku_task_0_start_value(runtime.driver,&input,&parent)==KU_TASK_DRIVER_OK && !input.ptr);
  uint64_t deadline=ku_task_driver_now_ms()+2000u;
  uint32_t fault=mode ? KU_TASK_DRIVER_INTERNAL : 0;
  CHECK(ku_task_driver_wait_idle(runtime.driver,deadline)==fault);
  CHECK(fixture_probed && fixture_rejections>=30u);
  KuTaskAdapterOutcomeV1 output={0};
  if (!mode) {
    CHECK(fixture_retried && fixture_committed);
    CHECK(ku_task_value_take(&parent,NULL,&output)==KU_TASK_CONTROL_OK);
    CHECK(output.result_kind==4u && output.exit_class==KU_TASK_EXIT_USER_RESULT && output.value.string.ok);
    CHECK(output.value.string.value.ptr==pointer && output.value.string.value.len==1u && output.value.string.value.capacity==1u
        && output.value.string.value.storage==KU_STRING_OWNED && !output.value.string.value.ptr[0]);
    CHECK(ku_task_driver_wait_idle(runtime.driver,deadline)==KU_TASK_DRIVER_OK);
    /* Even A=0 must reject a genuine but non-RUNNING parent. This probe is
     * outside callbacks, after the real parent has terminally completed. */
    KuTaskDriverStartChargeV1 nonrunning={parent.ticket,parent.owner.lease.control,sizeof(KuTaskInstance_0),0};
    KuTaskDriverTicketV1 reservation={0}; FixtureAccount before=fixture_account(runtime.driver);
    CHECK(ku_task_driver_reserve_child(runtime.driver,&nonrunning,sizeof(KuTaskInstance_2),&reservation)==KU_TASK_DRIVER_INVALID_STATE);
    fixture_same(runtime.driver,&before,0); fixture_ticket_empty(&reservation);
  } else {
    CHECK(!fixture_retried && !fixture_committed);
    CHECK(ku_task_value_take(&parent,NULL,&output)==KU_TASK_CONTROL_CANCELLED);
    CHECK(ku_task_outcome_empty(&output,4u));
  }
  ku_task_outcome_drop(&output);
  uint64_t cleanup_now=ku_task_driver_now_ms();
  CHECK(cleanup_now!=UINT64_MAX && cleanup_now<UINT64_MAX-1000u);
  uint64_t cleanup_deadline=cleanup_now+1000u;
  if (mode) cleanup_deadline=ku_task_driver_min(cleanup_deadline,
      ku_task_control_cleanup_deadline(parent.owner.lease.control));
  CHECK(ku_task_value_drop(&parent,cleanup_deadline)==KU_TASK_DRIVER_OK);
  CHECK(ku_task_value_drop(&fixture_other_task,cleanup_deadline)==KU_TASK_DRIVER_OK);
  fixture_finish(&runtime,fault,cleanup_deadline); fixture_finish(&fixture_other,0,cleanup_deadline);
  CHECK(!fixture_ledger().allocations && !fixture_ledger().bytes && !fixture_ledger().overflow);
}
int main(void) {
  fixture_case(0); fixture_case(1); fixture_case(2);
  puts("task-start-alias-ok"); return 0;
}
"#;
