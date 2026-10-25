//! Real source -> verified Task IR -> generated factories/continuations/root.
//! No fixture callback supplies Start, Await or scope cleanup on its behalf.
#[path = "support/native_allocation_harness.rs"]
mod native_allocation_harness;
#[allow(dead_code)]
#[path = "support/native_pg_harness.rs"]
mod native_harness;

use ku::{backend::c, checker::Checker, ir::task_lower, lexer::Lexer, parser::Parser};
use native_allocation_harness::ALLOCATION_HOOK;
use native_harness::{
    compile_harness, run_bounded, TempDir, BUILD_LIMITS, BUILD_TIMEOUT,
    NATIVE_THREAD_LIFECYCLE_HARNESS, RUN_LIMITS, RUN_TIMEOUT,
};
use std::{fs, process::Command};

fn checked(source: &str) -> ku::ast::Program {
    let ast = Parser::new(Lexer::new(source).lex().expect("source lexes"))
        .parse_program()
        .expect("source parses");
    Checker::new()
        .check(&ast)
        .unwrap_or_else(|error| panic!("{error}\n{source}"));
    ast
}

fn native(source: &str) -> String {
    let tasks =
        task_lower::lower_program(&checked(source)).expect("source lowers to verified Task IR");
    c::generate_native_task_c_source(&tasks, &Default::default()).expect("native Task C generation")
}

#[test]
fn native_task_external_owner_pump_preserves_affinity_wakes_and_retirement() {
    let generated=native("async fn Ready(value: int): int! { return ok(value) } async fn main(): null! { return ok(null) } async fn Work(): int! { index=0; while(index<5000) {index=index+1} return ok(index) }");
    let generated = generated.replacen(
        "int main(void) {",
        "static int ku_unused_source_main(void) {",
        1,
    );
    let source = format!(
        "{generated}\n{}",
        r#"
#define CHECK(c) do { if (!(c)) { fprintf(stderr,"external Task owner line %d: %s\n",__LINE__,#c); return 1; } } while (0)
static size_t notifications[2];
static void owner_wake(void* context,size_t owner) { (void)context; if(owner<2u) notifications[owner]++; }
int main(void) {
  KuTaskDriverV1 driver={0};
  KuTaskDriverSlotV1 slots[4]={0}; size_t ring[4]={0};
  size_t fixed=sizeof(driver)+sizeof(slots)+sizeof(ring);
  uint64_t deadline=ku_task_driver_now_ms()+1000u;
  CHECK(ku_task_driver_init_external(&driver,sizeof(driver),KU_TASK_DRIVER_ABI_VERSION,
      slots,4u,ring,4u,fixed+4u*sizeof(KuTaskInstance_0),2u,deadline,NULL,owner_wake)==KU_TASK_DRIVER_OK);
  KuTaskValueV1 tasks[4]={0};
  for(size_t i=0;i<4u;i++) {
    int64_t value=(int64_t)i;
    CHECK(ku_task_0_start_value(&driver,&value,&tasks[i])==KU_TASK_DRIVER_OK);
    CHECK(slots[tasks[i].ticket.slot].owner_worker==i%2u+1u);
    for(size_t n=0;n<1000u;n++) CHECK(ku_task_driver_wake(&tasks[i].ticket)==KU_TASK_DRIVER_OK);
  }
  CHECK(driver.queued==4u && !driver.global_queued);
  CHECK(driver.workers[0].local_queued==2u && driver.workers[1].local_queued==2u);
  CHECK(notifications[0] && notifications[1]);
  uint64_t next=UINT64_MAX;
  size_t before_first_pump[2]={notifications[0],notifications[1]};
  CHECK(ku_task_driver_pump(&driver,0u,1u,&next)==KU_TASK_DRIVER_OK);
  CHECK(driver.workers[0].polls==1u && !driver.workers[1].polls);
  CHECK(next==0u && driver.queued==3u);
  /* Remaining ready work is already represented by next==0. The owner must
   * turn again without a socket self-notification under the driver mutex. */
  CHECK(notifications[0]==before_first_pump[0] && notifications[1]==before_first_pump[1]);
  CHECK(ku_task_driver_pump(&driver,1u,2u,&next)==KU_TASK_DRIVER_OK);
  CHECK(driver.workers[1].polls==2u && driver.workers[0].polls==1u);
  CHECK(ku_task_driver_pump(&driver,0u,2u,&next)==KU_TASK_DRIVER_OK);
  CHECK(!driver.queued && driver.workers[0].polls==2u);
  KuTaskDriverTicketV1 old=tasks[0].ticket;
  for(size_t i=0;i<4u;i++) {
    KuTaskAdapterOutcomeV1 outcome={0};
    CHECK(ku_task_value_take(&tasks[i],NULL,&outcome)==KU_TASK_CONTROL_OK);
    CHECK(outcome.result_kind==1u && outcome.value.integer.ok && outcome.value.integer.value==(int64_t)i);
    ku_task_outcome_drop(&outcome);
    CHECK(ku_task_value_drop(&tasks[i],deadline)==KU_TASK_DRIVER_OK);
  }
  for(size_t n=0;n<16u && driver.resident;n++)
    for(size_t owner=0;owner<2u;owner++) {
      uint32_t result=ku_task_driver_pump(&driver,owner,4u,&next);
      CHECK(result==KU_TASK_DRIVER_OK || result==KU_TASK_DRIVER_PENDING);
    }
  CHECK(!driver.resident && !driver.reserved_bytes);
  KuTaskValueV1 work={0};
  CHECK(ku_task_2_start_value(&driver,&work)==KU_TASK_DRIVER_OK);
  size_t work_owner=slots[work.ticket.slot].owner_worker-1u;
  uint64_t before_polls=driver.workers[work_owner].polls;
  size_t before_notifications=notifications[work_owner];
  for(size_t turn=0;turn<16u && ku_task_control_status(&work.owner.lease)==KU_TASK_CONTROL_PENDING;turn++) {
    uint32_t result=ku_task_driver_pump(&driver,work_owner,64u,&next);
    CHECK(result==KU_TASK_DRIVER_OK || result==KU_TASK_DRIVER_PENDING);
  }
  CHECK(ku_task_control_status(&work.owner.lease)==KU_TASK_CONTROL_COMPLETED);
  uint64_t used_polls=driver.workers[work_owner].polls-before_polls;
  CHECK(used_polls>100u && notifications[work_owner]==before_notifications);
  KuTaskAdapterOutcomeV1 work_result={0};
  CHECK(ku_task_value_take(&work,NULL,&work_result)==KU_TASK_CONTROL_OK);
  CHECK(work_result.value.integer.ok && work_result.value.integer.value==5000);
  ku_task_outcome_drop(&work_result);
  CHECK(ku_task_value_drop(&work,deadline)==KU_TASK_DRIVER_OK);
  for(size_t turn=0;turn<16u && driver.resident;turn++)
    CHECK(ku_task_driver_pump(&driver,work_owner,4u,&next)==KU_TASK_DRIVER_OK);
  CHECK(!driver.resident && !driver.reserved_bytes);
  KuTaskValueV1 replacement={0}; int64_t value=9;
  size_t before_new_publication[2]={notifications[0],notifications[1]};
  CHECK(ku_task_0_start_value(&driver,&value,&replacement)==KU_TASK_DRIVER_OK);
  size_t replacement_owner=slots[replacement.ticket.slot].owner_worker-1u;
  /* Publication after the final pump lock observes pumping==0 and retains its
   * actual directed wake, even before the embedding loop reaches OS poll. */
  CHECK(notifications[replacement_owner]>before_new_publication[replacement_owner]);
  CHECK(notifications[1u-replacement_owner]==before_new_publication[1u-replacement_owner]);
  CHECK(ku_task_driver_wake(&old)==KU_TASK_DRIVER_STALE);
  CHECK(ku_task_value_drop(&replacement,deadline)==KU_TASK_DRIVER_OK);
  size_t before_close[2]={notifications[0],notifications[1]};
  CHECK(ku_task_driver_request_close(&driver,deadline)==KU_TASK_DRIVER_OK);
  CHECK(notifications[0]>before_close[0] && notifications[1]>before_close[1]);
  for(size_t n=0;n<16u && driver.workers_exited<2u;n++)
    for(size_t owner=0;owner<2u;owner++) if(driver.workers[owner].state!=KU_TASK_DRIVER_WORKER_EXITED) {
      uint32_t result=ku_task_driver_pump(&driver,owner,4u,&next);
      CHECK(result==KU_TASK_DRIVER_OK || result==KU_TASK_DRIVER_PENDING);
    }
  CHECK(!driver.resident && !driver.reserved_bytes && !driver.queued && driver.workers_exited==2u);
  CHECK(ku_task_driver_join(&driver,deadline)==KU_TASK_DRIVER_OK);
  CHECK(ku_task_driver_destroy(&driver)==KU_TASK_DRIVER_OK);
  /* A partial HTTP thread start leaves never-bound logical owners. With no
   * admitted work, the coordinator may bind/pump only these missing owners
   * after close, then reclaim through the ordinary EXITED/join contract. */
  memset(&driver,0,sizeof(driver));memset(slots,0,sizeof(slots));memset(ring,0,sizeof(ring));
  CHECK(ku_task_driver_init_external(&driver,sizeof(driver),KU_TASK_DRIVER_ABI_VERSION,
      slots,4u,ring,4u,fixed+4u*sizeof(KuTaskInstance_0),2u,deadline,NULL,owner_wake)==KU_TASK_DRIVER_OK);
  CHECK(ku_task_driver_request_close(&driver,deadline)==KU_TASK_DRIVER_OK);
  for(size_t owner=0;owner<2u;owner++) {
    CHECK(!driver.workers[owner].owner_bound);
    uint32_t result=ku_task_driver_pump(&driver,owner,1u,&next);
    CHECK(result==KU_TASK_DRIVER_OK || result==KU_TASK_DRIVER_PENDING);
  }
  CHECK(driver.workers_exited==2u && !driver.resident);
  CHECK(ku_task_driver_join(&driver,deadline)==KU_TASK_DRIVER_OK);
  CHECK(ku_task_driver_destroy(&driver)==KU_TASK_DRIVER_OK);
  puts("external-task-owners-ok"); return 0;
}
"#
    );
    let directory = TempDir::new("task-external-owner");
    let path = directory.path().join("external.c");
    fs::write(&path, source).unwrap();
    let Some(executable) = compile_harness(directory.path(), &path, "external") else {
        assert!(
            std::env::var_os("GITHUB_ACTIONS").is_none(),
            "CI requires a native C compiler"
        );
        return;
    };
    fs::remove_file(path).unwrap();
    let output = run_bounded(&mut Command::new(executable), RUN_TIMEOUT, RUN_LIMITS).unwrap();
    assert!(
        output.status.success(),
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).replace('\r', ""),
        "external-task-owners-ok\n"
    );
}

#[test]
fn native_task_external_owners_overlap_and_reclaim_after_cancel() {
    let mut generated = native(
        "async fn Work(value: int): int! { index=0; total=value; while(index<512) { total=total+1; index=index+1 } return ok(total) } async fn main(): null! { return ok(null) }",
    );
    // Wrap the generated callbacks, preserving the source-generated frame,
    // control CAS, dispatch budget, and actual external-owner pump path.
    for (signature, replacement) in [
        (
            "static uint32_t ku_task_0_resume(void* raw) {",
            "static uint32_t ku_task_0_resume(void* raw);\nstatic uint32_t ku_task_0_resume_inner(void* raw) {",
        ),
        (
            "static void ku_task_0_drop_frame(void* raw) {",
            "static void ku_task_0_drop_frame(void* raw);\nstatic void ku_task_0_drop_frame_inner(void* raw) {",
        ),
        (
            "static void ku_task_0_dispose(KuTaskControlV1* control, void* raw) {",
            "static void ku_task_0_dispose(KuTaskControlV1* control, void* raw);\nstatic void ku_task_0_dispose_inner(KuTaskControlV1* control, void* raw) {",
        ),
    ] {
        assert_eq!(generated.matches(signature).count(), 1);
        generated = generated.replacen(signature, replacement, 1);
    }
    assert_eq!(generated.matches("typedef struct KuString {").count(), 1);
    let source = generated
        .replacen(
            "typedef struct KuString {",
            &format!("{NATIVE_THREAD_LIFECYCLE_HARNESS}\ntypedef struct KuString {{"),
            1,
        )
        .replacen(
            "int main(void) {",
            "static int ku_unused_source_main(void) {",
            1,
        )
        + EXTERNAL_PARALLEL_MAIN;
    let directory = TempDir::new("task-external-parallel");
    let path = directory.path().join("external_parallel.c");
    fs::write(&path, source).unwrap();
    let Some(executable) = compile_harness(directory.path(), &path, "external_parallel") else {
        assert!(
            std::env::var_os("GITHUB_ACTIONS").is_none(),
            "CI requires a native C compiler"
        );
        return;
    };
    fs::remove_file(path).unwrap();
    let output = run_bounded(&mut Command::new(executable), RUN_TIMEOUT, RUN_LIMITS).unwrap();
    assert!(
        output.status.success(),
        "external owners must clean up before reporting absent overlap:\n{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).replace('\r', ""),
        "external-task-parallel-cleanup-ok\nexternal-task-parallel-ok\n"
    );
    assert!(output.stderr.is_empty());
}

const EXTERNAL_PARALLEL_MAIN: &str = r#"
#define CHECK(c) do { if (!(c)) { fprintf(stderr,"external parallel line %d: %s\n",__LINE__,#c); return 1; } } while (0)
#if defined(_WIN32)
typedef DWORD FixtureThreadId;
static SRWLOCK fixture_lock=SRWLOCK_INIT;
static void fixture_probe_lock(void) { AcquireSRWLockExclusive(&fixture_lock); }
static void fixture_probe_unlock(void) { ReleaseSRWLockExclusive(&fixture_lock); }
static FixtureThreadId fixture_thread_id(void) { return GetCurrentThreadId(); }
static int fixture_same_thread(FixtureThreadId a,FixtureThreadId b) { return a==b; }
#else
typedef pthread_t FixtureThreadId;
static pthread_mutex_t fixture_lock=PTHREAD_MUTEX_INITIALIZER;
static void fixture_probe_lock(void) { if(pthread_mutex_lock(&fixture_lock)) abort(); }
static void fixture_probe_unlock(void) { if(pthread_mutex_unlock(&fixture_lock)) abort(); }
static FixtureThreadId fixture_thread_id(void) { return pthread_self(); }
static int fixture_same_thread(FixtureThreadId a,FixtureThreadId b) { return pthread_equal(a,b); }
#endif
typedef struct FixtureProbe {
  void* context;
  size_t calls,active,maximum_active,frame_drops,disposals;
  FixtureThreadId thread;
  KuTestEvent entered,ran,retired;
} FixtureProbe;
typedef struct FixtureOwner {
  KuTaskDriverV1* driver;
  size_t index;
} FixtureOwner;
static FixtureProbe fixture_probes[2];
static KuTestEvent fixture_release,fixture_cleanup,fixture_finalize;
static int fixture_gate_failed;
static size_t fixture_probe_index(void* raw) {
  if(raw==fixture_probes[0].context) return 0;
  if(raw==fixture_probes[1].context) return 1;
  abort();
}
static uint32_t ku_task_0_resume(void* raw) {
  FixtureProbe* probe=&fixture_probes[fixture_probe_index(raw)];
  fixture_probe_lock();
  probe->active++;
  if(probe->active>probe->maximum_active) probe->maximum_active=probe->active;
  int first=++probe->calls==1u;
  if(first) probe->thread=fixture_thread_id();
  fixture_probe_unlock();
  if(first) {
    int entered=ku_test_event_set(&probe->entered);
    /* A broken serial implementation is released by the controller after its
     * shared observation window. This longer guard only bounds test cleanup. */
    int released=ku_test_event_wait(&fixture_release,5000u);
    if(!entered || !released) {
      fixture_probe_lock(); fixture_gate_failed=1; fixture_probe_unlock();
    }
  }
  uint32_t outcome=ku_task_0_resume_inner(raw);
  fixture_probe_lock(); probe->active--; fixture_probe_unlock();
  return outcome;
}
static void ku_task_0_drop_frame(void* raw) {
  fixture_probe_lock(); fixture_probes[fixture_probe_index(raw)].frame_drops++; fixture_probe_unlock();
  ku_task_0_drop_frame_inner(raw);
}
static void ku_task_0_dispose(KuTaskControlV1* control,void* raw) {
  fixture_probe_lock(); fixture_probes[fixture_probe_index(raw)].disposals++; fixture_probe_unlock();
  ku_task_0_dispose_inner(control,raw);
}
static void fixture_owner_wake(void* context,size_t owner) { (void)context; (void)owner; }
static unsigned long fixture_remaining(uint64_t deadline) {
  uint64_t now=ku_task_driver_now_ms();
  if(now==UINT64_MAX || now>=deadline) return 0;
  return (unsigned long)(deadline-now);
}
static int fixture_owner_run(void* raw) {
  FixtureOwner* owner=(FixtureOwner*)raw;
  KuTaskDriverV1* driver=owner->driver;
  int outcome=0;
  /* Pure ready source work: each real owner advances only its own bounded
   * queue. No sleeps or mock completion supply the source-loop result. */
  for(size_t turn=0;turn<256u;turn++) {
    uint64_t next=UINT64_MAX;
    uint32_t status=ku_task_driver_pump(driver,owner->index,1u,&next);
    if(status>KU_TASK_DRIVER_PENDING) { outcome=1; break; }
    if(ku_task_driver_lock(driver)) { outcome=2; break; }
    size_t ready=driver->workers[owner->index].local_queued;
    ku_task_driver_unlock(driver);
    if(!ready) break;
    if(turn==255u) outcome=3;
  }
  if(!ku_test_event_set(&fixture_probes[owner->index].ran)) outcome=4;
  if(!ku_test_event_wait(&fixture_cleanup,5000u)) return 5;
  /* The controller has dropped owners and requested close. The original
   * bound threads, rather than a controller impersonating them, retire slots. */
  for(size_t turn=0;turn<64u;turn++) {
    uint64_t next=UINT64_MAX;
    uint32_t status=ku_task_driver_pump(driver,owner->index,1u,&next);
    if(status>KU_TASK_DRIVER_PENDING) { outcome=6; break; }
    if(ku_task_driver_lock(driver)) { outcome=7; break; }
    size_t ready=driver->workers[owner->index].local_queued;
    ku_task_driver_unlock(driver);
    if(!ready) break;
    if(turn==63u) outcome=8;
  }
  /* A loop can be locally empty while the other owner still retires work.
   * Join that explicit phase before checking the driver's global exit rule. */
  if(!ku_test_event_set(&fixture_probes[owner->index].retired)) outcome=9;
  if(!ku_test_event_wait(&fixture_finalize,5000u)) return 10;
  if(ku_task_driver_lock(driver)) return 11;
  int exited=driver->workers[owner->index].state==KU_TASK_DRIVER_WORKER_EXITED;
  ku_task_driver_unlock(driver);
  if(!exited) {
    uint64_t next=UINT64_MAX;
    if(ku_task_driver_pump(driver,owner->index,1u,&next)>KU_TASK_DRIVER_PENDING) return 12;
  }
  return outcome;
}
int main(void) {
  KuTaskDriverV1 driver={0};
  KuTaskDriverSlotV1 slots[2]={0}; size_t ring[2]={0};
  KuTaskValueV1 tasks[2]={0};
  KuTaskDriverCleanupReceiptV1 cancelled={0};
  KuTaskDriverTicketV1 old={0};
  KuTestThread threads[2]={0};
  FixtureOwner owners[2]={{&driver,0u},{&driver,1u}};
  FixtureThreadId controller=fixture_thread_id();
  CHECK(ku_test_event_init(&fixture_release) && ku_test_event_init(&fixture_cleanup)
    && ku_test_event_init(&fixture_finalize));
  for(size_t i=0;i<2u;i++)
    CHECK(ku_test_event_init(&fixture_probes[i].entered) && ku_test_event_init(&fixture_probes[i].ran)
      && ku_test_event_init(&fixture_probes[i].retired));
  size_t fixed=sizeof(driver)+sizeof(slots)+sizeof(ring);
  uint64_t deadline=ku_task_driver_now_ms()+5000u;
  CHECK(ku_task_driver_init_external(&driver,sizeof(driver),KU_TASK_DRIVER_ABI_VERSION,
      slots,2u,ring,2u,fixed+2u*sizeof(KuTaskInstance_0),2u,deadline,NULL,fixture_owner_wake)==KU_TASK_DRIVER_OK);
  for(size_t i=0;i<2u;i++) {
    int64_t input=(int64_t)i+16;
    CHECK(ku_task_0_start_value(&driver,&input,&tasks[i])==KU_TASK_DRIVER_OK);
    CHECK(slots[tasks[i].ticket.slot].owner_worker==i+1u);
    fixture_probes[i].context=tasks[i].owner.lease.control->context;
  }
  old=tasks[0].ticket;
  size_t started=0;
  for(;started<2u;started++) if(!ku_test_thread_start(&threads[started],fixture_owner_run,&owners[started])) break;
  uint64_t observation_deadline=ku_task_driver_now_ms()+1000u;
  int entered[2]={0};
  for(size_t i=0;i<started;i++)
    entered[i]=ku_test_event_wait(&fixture_probes[i].entered,fixture_remaining(observation_deadline));
  int parallel=started==2u && entered[0] && entered[1];
  int cancellation_while_running=0;
  if(parallel) {
    fixture_probe_lock();
    parallel=fixture_probes[0].active==1u && fixture_probes[1].active==1u
      && !fixture_same_thread(fixture_probes[0].thread,fixture_probes[1].thread)
      && !fixture_same_thread(fixture_probes[0].thread,controller)
      && !fixture_same_thread(fixture_probes[1].thread,controller);
    fixture_probe_unlock();
    KuTaskDriverSnapshotV1 held={0};
    parallel=parallel && ku_task_driver_snapshot(&driver,&held)==KU_TASK_DRIVER_OK
      && held.running==2u && held.resident==2u && !held.queued && !held.fault;
    cancellation_while_running=ku_task_driver_owner_drop_receipt(
        &tasks[0].ticket,&tasks[0].owner,deadline,&cancelled)==KU_TASK_DRIVER_OK;
    if(cancellation_while_running) {
      cancellation_while_running=ku_task_control_atomic_load(
          &((KuTaskInstance_0*)fixture_probes[0].context)->control.phase)==KU_TASK_CONTROL_REQUESTED_CANCEL;
      tasks[0]=(KuTaskValueV1){0};
    }
  }
  /* Release every gate BEFORE any red assertion or join. In the serial case
   * the second callback can enter later and observes the already-open gate. */
  int cleanup_ok=ku_test_event_set(&fixture_release);
  for(size_t i=0;i<started;i++)
    if(!ku_test_event_wait(&fixture_probes[i].ran,5000u)) cleanup_ok=0;
  int correct=0;
  if(started==2u) {
    KuTaskAdapterOutcomeV1 value={0};
    uint32_t taken=ku_task_value_take(&tasks[1],NULL,&value);
    correct=taken==KU_TASK_CONTROL_OK && value.result_kind==1u
      && value.value.integer.ok && value.value.integer.value==529;
    ku_task_outcome_drop(&value);
  }
  for(size_t i=0;i<2u;i++) if(tasks[i].owner.lease.control) {
    if(ku_task_value_drop(&tasks[i],deadline)!=KU_TASK_DRIVER_OK) cleanup_ok=0;
  }
  if(ku_task_driver_request_close(&driver,deadline)!=KU_TASK_DRIVER_OK) cleanup_ok=0;
  if(!ku_test_event_set(&fixture_cleanup)) cleanup_ok=0;
  for(size_t i=0;i<started;i++)
    if(!ku_test_event_wait(&fixture_probes[i].retired,5000u)) cleanup_ok=0;
  /* A rare thread-start failure has no bound owner for that index. Close now
   * permits the controller to retire just those never-bound logical owners. */
  for(size_t i=started;i<2u;i++) {
    uint64_t next=UINT64_MAX;
    if(ku_task_driver_pump(&driver,i,64u,&next)>KU_TASK_DRIVER_PENDING) cleanup_ok=0;
  }
  if(!ku_test_event_set(&fixture_finalize)) cleanup_ok=0;
  for(size_t i=0;i<started;i++) {
    if(!ku_test_thread_join(&threads[i],5000u) || threads[i].outcome) cleanup_ok=0;
  }
  KuTaskDriverSnapshotV1 final={0};
  int empty=ku_task_driver_snapshot(&driver,&final)==KU_TASK_DRIVER_OK
    && !final.resident && !final.building && !final.queued && !final.running
    && !final.reserved_bytes && !final.timers && !final.fault && final.workers_exited==2u;
  int stale=ku_task_driver_wake(&old)==KU_TASK_DRIVER_STALE;
  int receipt=!cancelled.driver || ku_task_driver_cleanup_receipt_read(&cancelled)==KU_TASK_DRIVER_CLEANUP_ACK;
  if(ku_task_driver_join(&driver,ku_task_driver_now_ms()+1000u)!=KU_TASK_DRIVER_OK) cleanup_ok=0;
  if(ku_task_driver_destroy(&driver)!=KU_TASK_DRIVER_OK) cleanup_ok=0;
  int unique=1;
  for(size_t i=0;i<2u;i++) {
    FixtureProbe* probe=&fixture_probes[i];
    if(probe->active || probe->maximum_active>1u || probe->frame_drops!=1u || probe->disposals!=1u) unique=0;
    if(!ku_test_event_destroy(&probe->entered) || !ku_test_event_destroy(&probe->ran)
      || !ku_test_event_destroy(&probe->retired)) cleanup_ok=0;
  }
  if(!ku_test_event_destroy(&fixture_release) || !ku_test_event_destroy(&fixture_cleanup)
    || !ku_test_event_destroy(&fixture_finalize)) cleanup_ok=0;
#if !defined(_WIN32)
  if(pthread_mutex_destroy(&fixture_lock)) cleanup_ok=0;
#endif
  if(!cleanup_ok || !empty || !stale || !receipt || !unique) {
    fprintf(stderr,"external cleanup flags: cleanup=%d empty=%d stale=%d receipt=%d unique=%d resident=%zu queued=%zu running=%zu bytes=%zu exited=%zu fault=%u owner_outcomes=%d,%d drops=%zu,%zu disposals=%zu,%zu\n",
      cleanup_ok,empty,stale,receipt,unique,final.resident,final.queued,final.running,
      final.reserved_bytes,final.workers_exited,final.fault,threads[0].outcome,threads[1].outcome,
      fixture_probes[0].frame_drops,fixture_probes[1].frame_drops,
      fixture_probes[0].disposals,fixture_probes[1].disposals);
  }
  CHECK(cleanup_ok && empty && stale && receipt && unique);
  puts("external-task-parallel-cleanup-ok");
  CHECK(parallel && cancellation_while_running && correct
    && fixture_probes[1].calls>1u && !fixture_gate_failed);
  puts("external-task-parallel-ok"); return 0;
}
"#;

#[test]
fn native_task_quantum_default_preserves_wait_and_rejects_unregistered_pending() {
    let generated = native(
        "import time from \"std.time\"\nasync fn Work(): int! { index=0; while(index<512) { index=index+1 } time.sleep(1000)? return ok(index) } async fn InvalidPending(): int! { return ok(1) } async fn main(): null! { return ok(null) }",
    );
    let invalid = "static uint32_t ku_task_1_resume(void* raw) {";
    assert_eq!(generated.matches(invalid).count(), 1);
    let generated = generated
        .replacen(
            invalid,
            "static uint32_t ku_task_1_resume(void* raw) { (void)raw; return KU_TASK_CONTROL_PENDING; }\nstatic uint32_t ku_task_1_resume_original(void* raw) {",
            1,
        )
        .replacen(
            "int main(void) {",
            "static int ku_unused_source_main(void) {",
            1,
        );
    let source = generated
        + r#"
#define CHECK(c) do { if (!(c)) { fprintf(stderr,"quantum intent line %d: %s\n",__LINE__,#c); return 1; } } while (0)
static void fixture_wake(void* raw,size_t owner) { (void)raw; (void)owner; }
static int fixture_case(int invalid) {
  KuTaskDriverV1 driver={0};
  KuTaskDriverSlotV1 slots[1]={0}; size_t ring[1]={0};
  KuTaskValueV1 task={0};
  size_t fixed=sizeof(driver)+sizeof(slots)+sizeof(ring);
  uint64_t deadline=ku_task_driver_now_ms()+1000u;
  CHECK(ku_task_driver_init_external(&driver,sizeof(driver),KU_TASK_DRIVER_ABI_VERSION,
    slots,1u,ring,1u,fixed+sizeof(KuTaskInstance_0)+sizeof(KuTaskInstance_1),
    1u,deadline,NULL,fixture_wake)==KU_TASK_DRIVER_OK);
  CHECK((invalid ? ku_task_1_start_value(&driver,&task) : ku_task_0_start_value(&driver,&task))==KU_TASK_DRIVER_OK);
  KuTaskDriverTicketV1 old=task.ticket;
  /* An enclosing executor's TLS record must survive each nested pump. No
   * stack record may remain published after its run_one has returned. */
  KuTaskDriverExecutionV1 enclosing={0};
  ku_task_driver_execution=&enclosing;
  uint64_t next=UINT64_MAX;
  uint32_t pumped=ku_task_driver_pump(&driver,0u,1u,&next);
  CHECK(ku_task_driver_execution==&enclosing && !enclosing.default_yield);
  if(invalid) {
    CHECK(pumped==KU_TASK_DRIVER_INTERNAL && driver.fault==KU_TASK_DRIVER_INTERNAL);
    CHECK(!driver.queued && driver.resident==1u && !slots[0].intent);
    CHECK(ku_task_control_status(&task.owner.lease)==KU_TASK_CONTROL_PENDING);
  } else {
    CHECK(pumped==KU_TASK_DRIVER_OK && !driver.fault);
    CHECK(driver.queued==1u && slots[0].intent==KU_TASK_DRIVER_YIELD && next==0u);
    for(size_t turn=0;turn<256u && slots[0].timer_state!=KU_TASK_DRIVER_TIMER_ARMED;turn++) {
      CHECK(ku_task_driver_pump(&driver,0u,1u,&next)<=KU_TASK_DRIVER_PENDING);
      CHECK(ku_task_driver_execution==&enclosing);
    }
    CHECK(slots[0].timer_state==KU_TASK_DRIVER_TIMER_ARMED && driver.timers==1u);
    CHECK(slots[0].intent==KU_TASK_DRIVER_WAIT && slots[0].state==KU_TASK_DRIVER_PARKED);
    CHECK(!driver.queued && next!=UINT64_MAX && next>ku_task_driver_now_ms());
  }
  CHECK(ku_task_value_drop(&task,deadline)==KU_TASK_DRIVER_OK);
  uint32_t closing=ku_task_driver_request_close(&driver,deadline);
  CHECK(closing==(invalid ? KU_TASK_DRIVER_INTERNAL : KU_TASK_DRIVER_OK));
  for(size_t turn=0;turn<32u && driver.workers_exited!=1u;turn++) {
    pumped=ku_task_driver_pump(&driver,0u,1u,&next);
    CHECK(pumped==(invalid ? KU_TASK_DRIVER_INTERNAL : KU_TASK_DRIVER_OK)
      || (!invalid && pumped==KU_TASK_DRIVER_PENDING));
    CHECK(ku_task_driver_execution==&enclosing && !enclosing.default_yield);
  }
  CHECK(!driver.resident && !driver.running && !driver.queued && !driver.timers
    && !driver.reserved_bytes && driver.workers_exited==1u);
  CHECK(ku_task_driver_wake(&old)==KU_TASK_DRIVER_STALE);
  CHECK(ku_task_driver_join(&driver,deadline)==KU_TASK_DRIVER_OK);
  CHECK(ku_task_driver_destroy(&driver)==KU_TASK_DRIVER_OK);
  ku_task_driver_execution=NULL;
  return 0;
}
int main(void) {
  CHECK(!fixture_case(0));
  CHECK(!fixture_case(1));
  puts("quantum-default-wait-invalid-pending-ok"); return 0;
}
"#;
    let directory = TempDir::new("task-quantum-intent");
    let path = directory.path().join("quantum_intent.c");
    fs::write(&path, source).unwrap();
    let Some(executable) = compile_harness(directory.path(), &path, "quantum_intent") else {
        assert!(
            std::env::var_os("GITHUB_ACTIONS").is_none(),
            "CI requires a native C compiler"
        );
        return;
    };
    fs::remove_file(path).unwrap();
    let output = run_bounded(&mut Command::new(executable), RUN_TIMEOUT, RUN_LIMITS).unwrap();
    assert!(
        output.status.success(),
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).replace('\r', ""),
        "quantum-default-wait-invalid-pending-ok\n"
    );
    assert!(output.stderr.is_empty());
}

#[test]
fn native_task_scalar_edges_preserve_exact_quantum_cancel_and_error_boundaries() {
    let ast = checked(
        "async fn Work(seed: int, divisor: int): int! { index=0; total=seed; while(index<5000) { total=(total*33+index+seed)%100000000; index=index+1 } total=total/divisor; return ok(total) } async fn main(): null! { return ok(null) }",
    );
    let tasks = task_lower::lower_program(&ast).unwrap();
    let work = &tasks.tasks.functions[0];
    let plan = ku::ir::task::verify_and_plan(&tasks.tasks, Default::default()).unwrap();
    assert!(
        plan.functions[0].cooperative_slots.len() < work.slots.len(),
        "single-state arithmetic intermediates must not become persistent frame fields"
    );
    let mut generated = c::generate_native_task_c_source(&tasks, &Default::default()).unwrap();
    let begin = generated
        .find("static uint32_t ku_task_frame_0_drive(")
        .unwrap();
    let end = begin
        + generated[begin..]
            .find("static uint32_t ku_task_frame_0_resume(")
            .unwrap();
    let mut drive = generated[begin..end].to_owned();
    assert!(drive.contains("goto ku_task_scalar_check_"));
    assert!(drive.contains("goto ku_task_range_state_"));
    assert!(drive.contains("__ku_range_fast=!cleanup && ("));
    // Count executed *verified IR states*, including lexical empty drains and
    // latches; do not count C branches or substitute a different work quantum.
    for state in 0..work.states.len() {
        let label = format!("ku_task_state_{state}:;");
        assert_eq!(drive.matches(&label).count(), 1);
        drive = drive.replacen(&label, &format!("{label}\n  fixture_steps++;\n  if(fixture_cancel_at && fixture_steps==fixture_cancel_at) fixture_cancel_result=ku_task_control_request_cancel(fixture_cancel_lease,KU_TASK_CONTROL_CANCELLED,fixture_deadline);"), 1);
        let fast = format!("ku_task_range_state_{state}:;");
        if drive.contains(&fast) {
            assert_eq!(drive.matches(&fast).count(), 1);
            drive = drive.replacen(&fast, &format!("{fast}\n  fixture_fast_steps++; fixture_steps++;\n  if(fixture_cancel_at && fixture_steps==fixture_cancel_at) fixture_cancel_result=ku_task_control_request_cancel(fixture_cancel_lease,KU_TASK_CONTROL_CANCELLED,fixture_deadline);"), 1);
        }
    }
    drive = drive.replace(
        "__ku_range_fast=!cleanup && (",
        "__ku_range_fast=!fixture_force_slow && !cleanup && (",
    );
    let globals = "static size_t fixture_steps,fixture_fast_steps,fixture_cancel_at;\nstatic int fixture_force_slow;\nstatic uint32_t fixture_cancel_result;\nstatic uint64_t fixture_deadline;\nstatic KuTaskControlLeaseV1* fixture_cancel_lease;\n";
    generated.replace_range(begin..end, &(globals.to_owned() + &drive));
    let expected = (0..5000).fold(3i64, |total, index| (total * 33 + index + 3) % 100000000);
    let source = generated.replacen(
        "int main(void) {",
        "static int ku_unused_source_main(void) {",
        1,
    ) + &format!("\n#define FIXTURE_EXPECTED INT64_C({expected})\n")
        + r#"
#define CHECK(c) do { if (!(c)) { fprintf(stderr,"scalar edges line %d: %s\n",__LINE__,#c); return 1; } } while (0)
static void fixture_wake(void* raw,size_t owner) { (void)raw; (void)owner; }
static int fixture_case(size_t cancel_at,int overflow,int divide_zero,int force_slow) {
  KuTaskDriverV1 driver={0}; KuTaskDriverSlotV1 slots[1]={0}; size_t ring[1]={0};
  KuTaskValueV1 task={0}; uint64_t next=UINT64_MAX;
  size_t fixed=sizeof(driver)+sizeof(slots)+sizeof(ring);
  fixture_steps=fixture_fast_steps=0; fixture_force_slow=force_slow;
  fixture_cancel_at=cancel_at; fixture_cancel_result=UINT32_MAX;
  fixture_deadline=ku_task_driver_now_ms()+2000u;
  CHECK(ku_task_driver_init_external(&driver,sizeof(driver),KU_TASK_DRIVER_ABI_VERSION,
    slots,1u,ring,1u,fixed+sizeof(KuTaskInstance_0),1u,fixture_deadline,NULL,fixture_wake)==KU_TASK_DRIVER_OK);
  int64_t seed=overflow==2?INT64_MIN:overflow?INT64_MAX:3,divisor=divide_zero?0:1;
  CHECK(ku_task_0_start_value(&driver,&seed,&divisor,&task)==KU_TASK_DRIVER_OK);
  fixture_cancel_lease=&task.owner.lease;
  CHECK(ku_task_driver_pump(&driver,0u,1u,&next)==KU_TASK_DRIVER_OK);
  if(cancel_at) CHECK(fixture_steps==cancel_at && fixture_cancel_result==KU_TASK_CONTROL_OK);
  else if(!overflow) CHECK(fixture_steps==64u && driver.polls==1u && driver.queued==1u);
  if(force_slow) CHECK(!fixture_fast_steps);
  else if(!overflow) CHECK(fixture_fast_steps>0);
  if(overflow) CHECK(!fixture_fast_steps);
  for(size_t turn=0;turn<2000u && ku_task_control_status(&task.owner.lease)==KU_TASK_CONTROL_PENDING;turn++)
    CHECK(ku_task_driver_pump(&driver,0u,32u,&next)<=KU_TASK_DRIVER_PENDING);
  if(cancel_at) CHECK(ku_task_control_status(&task.owner.lease)==KU_TASK_CONTROL_CANCELLED);
  else {
    KuTaskAdapterOutcomeV1 outcome={0};
    CHECK(ku_task_value_take(&task,NULL,&outcome)==KU_TASK_CONTROL_OK);
    CHECK(outcome.result_kind==1u);
    if(overflow || divide_zero) {
      CHECK(!outcome.value.integer.ok);
      KuString message=outcome.value.integer.error.message;
      const char* expected=overflow?"integer overflow":"division by zero";
      CHECK(message.len==strlen(expected) && !memcmp(message.ptr,expected,message.len));
    } else CHECK(outcome.value.integer.ok && outcome.value.integer.value==FIXTURE_EXPECTED);
    ku_task_outcome_drop(&outcome);
  }
  CHECK(ku_task_value_drop(&task,fixture_deadline)==KU_TASK_DRIVER_OK);
  CHECK(ku_task_driver_request_close(&driver,fixture_deadline)==KU_TASK_DRIVER_OK);
  for(size_t turn=0;turn<16u && driver.workers_exited!=1u;turn++)
    CHECK(ku_task_driver_pump(&driver,0u,32u,&next)<=KU_TASK_DRIVER_PENDING);
  CHECK(!driver.resident && !driver.running && !driver.queued && !driver.reserved_bytes && !driver.fault);
  CHECK(ku_task_driver_join(&driver,fixture_deadline)==KU_TASK_DRIVER_OK);
  CHECK(ku_task_driver_destroy(&driver)==KU_TASK_DRIVER_OK);
  return 0;
}
int main(void) {
  for(int force_slow=0;force_slow<2;force_slow++) {
    CHECK(!fixture_case(0,0,0,force_slow));
    /* Publish at every original state position in both emitted paths. The
     * initial fast body has already been charged by general dispatch. */
    for(size_t cancel_at=1u;cancel_at<=64u;cancel_at++)
      CHECK(!fixture_case(cancel_at,0,0,force_slow));
    CHECK(!fixture_case(0,1,0,force_slow));
    CHECK(!fixture_case(0,2,0,force_slow));
    CHECK(!fixture_case(0,0,1,force_slow));
  }
  puts("task-scalar-exact-quantum-cancel-errors-ok"); return 0;
}
"#;
    let directory = TempDir::new("task-scalar-edges");
    let path = directory.path().join("scalar_edges.c");
    fs::write(&path, source).unwrap();
    let Some(executable) = compile_harness(directory.path(), &path, "scalar_edges") else {
        assert!(
            std::env::var_os("GITHUB_ACTIONS").is_none(),
            "CI requires a native C compiler"
        );
        return;
    };
    fs::remove_file(path).unwrap();
    let output = run_bounded(&mut Command::new(executable), RUN_TIMEOUT, RUN_LIMITS).unwrap();
    assert!(
        output.status.success(),
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).replace('\r', ""),
        "task-scalar-exact-quantum-cancel-errors-ok\n"
    );
    assert!(output.stderr.is_empty());
}

#[test]
fn native_task_guarded_ranges_keep_growth_failures_and_original_error_order() {
    let ast = checked(
        r#"
async fn Square(value:int):int! {
    total=value; i=0
    while(i<3) { total=total*total; i=i+1 }
    return ok(total)
}
async fn Remainder(divisor:int):int! {
    total=0; i=0
    while(i<3) {
        minimum= -9223372036854775807 - 1
        total=minimum%divisor
        total=total/0
        i=i+1
    }
    return ok(total)
}
async fn main():null! { return ok(null) }
"#,
    );
    let tasks = task_lower::lower_program(&ast).unwrap();
    let mut generated = c::generate_native_task_c_source(&tasks, &Default::default()).unwrap();
    let first = generated
        .find("static uint32_t ku_task_frame_0_drive(")
        .unwrap();
    generated.insert_str(
        first,
        "static size_t fixture_range_steps;\nstatic int fixture_range_slow;\n",
    );
    generated = generated.replace(
        "__ku_range_fast=!cleanup && (",
        "__ku_range_fast=!fixture_range_slow && !cleanup && (",
    );
    for function in &tasks.tasks.functions[..2] {
        let begin = generated
            .find(&format!(
                "static uint32_t ku_task_frame_{}_drive(",
                function.id.0
            ))
            .unwrap();
        let end = begin
            + generated[begin..]
                .find(&format!(
                    "static uint32_t ku_task_frame_{}_resume(",
                    function.id.0
                ))
                .unwrap();
        let mut drive = generated[begin..end].to_owned();
        assert!(drive.contains("ku_task_range_state_"));
        let fast = drive.find("ku_task_range_check_").unwrap();
        if function.id.0 == 0 {
            assert!(
                drive[fast..].contains("ku_int_mul("),
                "later squares must retain overflow checks"
            );
        } else {
            assert!(drive[fast..].contains("ku_int_rem("));
            assert!(drive[fast..].contains("ku_int_div("));
        }
        for state in 0..function.states.len() {
            let label = format!("ku_task_range_state_{state}:;");
            drive = drive.replace(&label, &format!("{label}\n  fixture_range_steps++;\n"));
        }
        generated.replace_range(begin..end, &drive);
    }
    let source = generated.replacen(
        "int main(void) {",
        "static int ku_unused_source_main(void) {",
        1,
    ) + r#"
#define CHECK(c) do { if (!(c)) { fprintf(stderr,"guarded range line %d: %s\n",__LINE__,#c); return 1; } } while (0)
static void fixture_wake(void* raw,size_t owner) { (void)raw; (void)owner; }
static int fixture_case(int variant,int slow) {
  KuTaskDriverV1 driver={0}; KuTaskDriverSlotV1 slots[1]={0}; size_t ring[1]={0};
  KuTaskValueV1 task={0}; uint64_t next=UINT64_MAX;
  uint64_t deadline=ku_task_driver_now_ms()+2000u;
  size_t fixed=sizeof(driver)+sizeof(slots)+sizeof(ring);
  fixture_range_steps=0; fixture_range_slow=slow;
  CHECK(ku_task_driver_init_external(&driver,sizeof(driver),KU_TASK_DRIVER_ABI_VERSION,
    slots,1u,ring,1u,fixed+sizeof(KuTaskInstance_0)+sizeof(KuTaskInstance_1),1u,
    deadline,NULL,fixture_wake)==KU_TASK_DRIVER_OK);
  int64_t value=variant==0?INT32_MAX:variant==1?-1:variant==2?0:2;
  CHECK((variant==0?ku_task_0_start_value(&driver,&value,&task)
    :ku_task_1_start_value(&driver,&value,&task))==KU_TASK_DRIVER_OK);
  for(size_t turn=0;turn<64u && ku_task_control_status(&task.owner.lease)==KU_TASK_CONTROL_PENDING;turn++)
    CHECK(ku_task_driver_pump(&driver,0u,32u,&next)<=KU_TASK_DRIVER_PENDING);
  CHECK(ku_task_control_status(&task.owner.lease)!=KU_TASK_CONTROL_PENDING);
  if(slow) CHECK(!fixture_range_steps); else CHECK(fixture_range_steps>0);
  KuTaskAdapterOutcomeV1 outcome={0};
  CHECK(ku_task_value_take(&task,NULL,&outcome)==KU_TASK_CONTROL_OK);
  CHECK(outcome.result_kind==1u && !outcome.value.integer.ok);
  KuString message=outcome.value.integer.error.message;
  const char* expected=variant<2?"integer overflow":"division by zero";
  CHECK(message.len==strlen(expected) && !memcmp(message.ptr,expected,message.len));
  ku_task_outcome_drop(&outcome);
  CHECK(ku_task_value_drop(&task,deadline)==KU_TASK_DRIVER_OK);
  CHECK(ku_task_driver_request_close(&driver,deadline)==KU_TASK_DRIVER_OK);
  for(size_t turn=0;turn<16u && driver.workers_exited!=1u;turn++)
    CHECK(ku_task_driver_pump(&driver,0u,32u,&next)<=KU_TASK_DRIVER_PENDING);
  CHECK(!driver.resident && !driver.running && !driver.queued && !driver.reserved_bytes && !driver.fault);
  CHECK(ku_task_driver_join(&driver,deadline)==KU_TASK_DRIVER_OK);
  CHECK(ku_task_driver_destroy(&driver)==KU_TASK_DRIVER_OK);
  return 0;
}
int main(void) {
  for(int slow=0;slow<2;slow++) for(int variant=0;variant<4;variant++) CHECK(!fixture_case(variant,slow));
  puts("task-guarded-range-growth-error-order-ok"); return 0;
}
"#;
    let directory = TempDir::new("task-guarded-range");
    let path = directory.path().join("guarded_range.c");
    fs::write(&path, source).unwrap();
    let Some(executable) = compile_harness(directory.path(), &path, "guarded_range") else {
        assert!(
            std::env::var_os("GITHUB_ACTIONS").is_none(),
            "CI must run guarded ranges"
        );
        return;
    };
    fs::remove_file(path).unwrap();
    let output = run_bounded(&mut Command::new(executable), RUN_TIMEOUT, RUN_LIMITS).unwrap();
    assert!(
        output.status.success(),
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8(output.stdout).unwrap().replace('\r', ""),
        "task-guarded-range-growth-error-order-ok\n"
    );
    assert!(output.stderr.is_empty());
}

#[test]
fn native_task_segments_keep_quantum_limits_and_explicit_intent_boundaries() {
    let mut generated = native(
        "import time from \"std.time\"\nasync fn Work(): int! { index=0; while(index<5000) { index=index+1 } return ok(index) } async fn Touch(): int! { time.sleep(0)? index=0; while(index<5000) { index=index+1 } return ok(index) } async fn main(): null! { return ok(null) }",
    );
    let clock = "static uint64_t ku_task_driver_now_ms(void) {";
    assert_eq!(generated.matches(clock).count(), 1);
    // A controlled monotonic clock makes segment-cap and boundary assertions
    // deterministic. Real wall-clock fairness remains a separate HTTP gate.
    generated = generated.replacen(
        clock,
        "static uint64_t fixture_clock=1000u;\nstatic size_t fixture_lock_entries,fixture_calls,fixture_last_lock,fixture_same_segment;\nstatic int fixture_mode,fixture_touch_seen,fixture_touch_boundary;\nstatic uint64_t ku_task_driver_now_ms(void) { return fixture_clock; }\nstatic uint64_t fixture_original_now_ms(void) {",
        1,
    );
    let lock = "static int ku_task_driver_lock(KuTaskDriverV1* driver) {";
    assert_eq!(generated.matches(lock).count(), 1);
    generated = generated.replacen(lock, &format!("{lock}\n  fixture_lock_entries++;"), 1);
    for id in 0..2 {
        let signature = format!("static uint32_t ku_task_{id}_resume(void* raw) {{");
        assert_eq!(generated.matches(&signature).count(), 1);
        generated = generated.replacen(
            &signature,
            &format!(
                r#"static uint32_t ku_task_{id}_resume_inner(void* raw);
static uint32_t ku_task_{id}_resume(void* raw) {{
  if(fixture_calls && fixture_last_lock==fixture_lock_entries) fixture_same_segment++;
  if({id}u==1u && fixture_calls==1u)
    fixture_touch_boundary=fixture_lock_entries>fixture_last_lock;
  fixture_calls++;
  uint32_t result=ku_task_{id}_resume_inner(raw);
  if({id}u==1u && fixture_calls==1u)
    fixture_touch_seen=ku_task_driver_execution && ku_task_driver_execution->intent_touched
      && ku_task_driver_execution->cooperative_yield;
  fixture_last_lock=fixture_lock_entries;
  if(fixture_mode==2) fixture_clock++;
  return result;
}}
static uint32_t ku_task_{id}_resume_inner(void* raw) {{"#
            ),
            1,
        );
    }
    let source = generated.replacen(
        "int main(void) {",
        "static int ku_unused_source_main(void) {",
        1,
    ) + r#"
#define CHECK(c) do { if (!(c)) { fprintf(stderr,"segment boundary line %d: %s\n",__LINE__,#c); return 1; } } while (0)
static void fixture_wake(void* raw,size_t owner) { (void)raw; (void)owner; }
static int fixture_case(int mode) {
  fixture_mode=mode;
  fixture_calls=fixture_last_lock=fixture_same_segment=0;
  fixture_touch_seen=fixture_touch_boundary=0;
  KuTaskDriverV1 driver={0};
  KuTaskDriverSlotV1 slots[1]={0}; size_t ring[1]={0};
  KuTaskValueV1 task={0};
  size_t fixed=sizeof(driver)+sizeof(slots)+sizeof(ring);
  uint64_t deadline=fixture_clock+1000u;
  CHECK(ku_task_driver_init_external(&driver,sizeof(driver),KU_TASK_DRIVER_ABI_VERSION,
    slots,1u,ring,1u,fixed+sizeof(KuTaskInstance_0)+sizeof(KuTaskInstance_1),
    1u,deadline,NULL,fixture_wake)==KU_TASK_DRIVER_OK);
  CHECK((mode==1 ? ku_task_1_start_value(&driver,&task) : ku_task_0_start_value(&driver,&task))==KU_TASK_DRIVER_OK);
  KuTaskDriverTicketV1 old=task.ticket;
  uint64_t next=UINT64_MAX;
  if(mode==0) {
    CHECK(ku_task_driver_pump(&driver,0u,1u,&next)==KU_TASK_DRIVER_OK);
    CHECK(driver.polls==1u && driver.workers[0].polls==1u && fixture_calls==1u);
    CHECK(driver.queued==1u && next==0u);
  }
  uint64_t before_polls=driver.polls;
  size_t before_calls=fixture_calls, before_locks=fixture_lock_entries;
  CHECK(ku_task_driver_pump(&driver,0u,32u,&next)==KU_TASK_DRIVER_OK);
  CHECK(driver.polls-before_polls==32u && fixture_calls-before_calls==32u);
  CHECK(driver.workers[0].polls==driver.polls && driver.queued==1u && next==0u);
  if(mode==0) {
    CHECK(fixture_same_segment==31u);
    CHECK(fixture_lock_entries-before_locks==2u);
  } else if(mode==1) {
    CHECK(fixture_touch_seen && fixture_touch_boundary);
    CHECK(fixture_same_segment==30u);
  } else {
    CHECK(!fixture_same_segment && fixture_lock_entries-before_locks==33u);
  }
  CHECK(ku_task_value_drop(&task,deadline)==KU_TASK_DRIVER_OK);
  CHECK(ku_task_driver_request_close(&driver,deadline)==KU_TASK_DRIVER_OK);
  for(size_t turn=0;turn<16u && driver.workers_exited!=1u;turn++)
    CHECK(ku_task_driver_pump(&driver,0u,32u,&next)<=KU_TASK_DRIVER_PENDING);
  CHECK(!driver.resident && !driver.running && !driver.queued && !driver.timers
    && !driver.reserved_bytes && driver.workers_exited==1u && !driver.fault);
  CHECK(!ku_task_driver_execution && ku_task_driver_wake(&old)==KU_TASK_DRIVER_STALE);
  CHECK(ku_task_driver_join(&driver,deadline)==KU_TASK_DRIVER_OK);
  CHECK(ku_task_driver_destroy(&driver)==KU_TASK_DRIVER_OK);
  return 0;
}
int main(void) {
  for(int mode=0;mode<3;mode++) CHECK(!fixture_case(mode));
  puts("task-segment-quantum-intent-clock-ok"); return 0;
}
"#;
    let directory = TempDir::new("task-segment-boundaries");
    let path = directory.path().join("segments.c");
    fs::write(&path, source).unwrap();
    let Some(executable) = compile_harness(directory.path(), &path, "segments") else {
        assert!(
            std::env::var_os("GITHUB_ACTIONS").is_none(),
            "CI requires a native C compiler"
        );
        return;
    };
    fs::remove_file(path).unwrap();
    let output = run_bounded(&mut Command::new(executable), RUN_TIMEOUT, RUN_LIMITS).unwrap();
    assert!(
        output.status.success(),
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).replace('\r', ""),
        "task-segment-quantum-intent-clock-ok\n"
    );
    assert!(output.stderr.is_empty());
}

#[test]
fn native_task_scalar_empty_scope_publishes_pending_before_host_allocation() {
    let tasks = task_lower::lower_program(&checked(
        "async fn Work(): str! { i=0; total=0; while(i<2) { total=total+i+1; i=i+1 } text=str(total); return ok(text) } async fn main(): null! { return ok(null) }",
    ))
    .unwrap();
    let work = &tasks.tasks.functions[0];
    let mut generated = c::generate_native_task_c_source(&tasks, &Default::default()).unwrap();
    generated = generated.replacen(
        "typedef struct KuString {",
        &format!("{ALLOCATION_HOOK}\nstatic size_t fixture_steps,fixture_entry_seen,fixture_host_calls,fixture_bad_pending,fixture_bad_first_quantum,fixture_terminal_calls,fixture_bad_terminal;\nstatic uint32_t fixture_entry_status=UINT32_MAX,fixture_host_status=UINT32_MAX;\ntypedef struct KuString {{"),
        1,
    );
    let begin = generated
        .find("static uint32_t ku_task_frame_0_drive(")
        .unwrap();
    let end = begin
        + generated[begin..]
            .find("static uint32_t ku_task_frame_0_resume(")
            .unwrap();
    let mut drive = generated[begin..end].to_owned();
    for state in 0..work.states.len() {
        for label in [
            format!("ku_task_state_{state}:;"),
            format!("ku_task_range_state_{state}:;"),
        ] {
            if drive.contains(&label) {
                // Observe actual state entry; never canonicalize the header.
                drive = drive.replacen(&label, &format!("{label}\n  if(!fixture_entry_seen++) fixture_entry_status=frame->header.status;\n  fixture_steps++;\n"), 1);
            }
        }
    }
    generated.replace_range(begin..end, &drive);
    let reconcile = "static uint32_t ku_task_0_reconcile_charge(void* raw) {";
    assert_eq!(generated.matches(reconcile).count(), 1);
    generated = generated.replacen(
        reconcile,
        &format!(r#"{reconcile}
  KuTaskInstance_0* observed=(KuTaskInstance_0*)raw;
  if(observed->frame.header.running) {{
    if(!fixture_host_calls++) fixture_host_status=observed->frame.header.status;
    fixture_bad_pending+=observed->frame.header.status!=KU_TASK_FRAME_PENDING;
    fixture_bad_first_quantum+=fixture_steps>=64u || observed->ticket.driver->polls!=0u;
  }} else if(observed->frame.header.result_initialized) {{
    fixture_terminal_calls++;
    fixture_bad_terminal+=observed->frame.header.status!=KU_TASK_FRAME_READY && observed->frame.header.status!=KU_TASK_FRAME_EXIT_STAGED;
  }}"#),
        1,
    );
    let source = generated.replacen(
        "int main(void) {",
        "static int ku_unused_source_main(void) {",
        1,
    ) + r#"
#define CHECK(c) do { if (!(c)) { fprintf(stderr,"short pending line %d: %s; entry=%u host=%u steps=%zu calls=%zu bad=%zu terminal_bad=%zu\n",__LINE__,#c,fixture_entry_status,fixture_host_status,fixture_steps,fixture_host_calls,fixture_bad_pending,fixture_bad_terminal); return 1; } } while(0)
static void fixture_wake(void* raw,size_t owner) { (void)raw; (void)owner; }
int main(void) {
  KuTaskDriverV1 driver={0}; KuTaskDriverSlotV1 slots[1]={0}; size_t ring[1]={0};
  KuTaskValueV1 task={0}; uint64_t next=UINT64_MAX,deadline=ku_task_driver_now_ms()+2000u;
  size_t fixed=sizeof(driver)+sizeof(slots)+sizeof(ring);
  CHECK(ku_task_driver_init_external(&driver,sizeof(driver),KU_TASK_DRIVER_ABI_VERSION,
    slots,1u,ring,1u,fixed+sizeof(KuTaskInstance_0)+4096u,1u,deadline,NULL,fixture_wake)==KU_TASK_DRIVER_OK);
  CHECK(ku_task_0_start_value(&driver,&task)==KU_TASK_DRIVER_OK);
  KuTaskDriverTicketV1 old=task.ticket;
  KuTaskInstance_0* instance=(KuTaskInstance_0*)task.owner.lease.control->context;
  CHECK(instance->frame.header.status==KU_TASK_FRAME_OK);
  CHECK(ku_task_driver_pump(&driver,0u,1u,&next)<=KU_TASK_DRIVER_PENDING);
  CHECK(fixture_entry_seen && fixture_entry_status==KU_TASK_FRAME_OK);
  CHECK(fixture_host_calls && !fixture_bad_pending && !fixture_bad_first_quantum);
  for(size_t turn=0;turn<16u && ku_task_control_status(&task.owner.lease)==KU_TASK_CONTROL_PENDING;turn++)
    CHECK(ku_task_driver_pump(&driver,0u,1u,&next)<=KU_TASK_DRIVER_PENDING);
  CHECK(ku_task_control_status(&task.owner.lease)==KU_TASK_CONTROL_COMPLETED);
  CHECK(fixture_terminal_calls && !fixture_bad_terminal);
  KuTaskAdapterOutcomeV1 outcome={0};
  CHECK(ku_task_value_take(&task,NULL,&outcome)==KU_TASK_CONTROL_OK);
  CHECK(outcome.result_kind==4u && outcome.value.string.ok);
  KuString text=outcome.value.string.value;
  CHECK(text.storage==KU_STRING_OWNED && text.len==1u && text.ptr && text.ptr[0]=='3');
  ku_task_outcome_drop(&outcome);
  CHECK(ku_task_value_drop(&task,deadline)==KU_TASK_DRIVER_OK && !task.owner.lease.control);
  CHECK(ku_task_driver_request_close(&driver,deadline)==KU_TASK_DRIVER_OK);
  for(size_t turn=0;turn<16u && driver.workers_exited!=1u;turn++)
    CHECK(ku_task_driver_pump(&driver,0u,1u,&next)<=KU_TASK_DRIVER_PENDING);
  CHECK(!driver.resident && !driver.running && !driver.queued && !driver.timers && !driver.reserved_bytes && !driver.fault);
  CHECK(!slots[0].binding && ku_task_driver_wake(&old)==KU_TASK_DRIVER_STALE);
  CHECK(ku_task_driver_join(&driver,deadline)==KU_TASK_DRIVER_OK && ku_task_driver_destroy(&driver)==KU_TASK_DRIVER_OK);
  CHECK(ku_perf_calls>=2u && !ku_perf_live_allocations && !ku_perf_live_bytes && !ku_perf_overflow);
  printf("short-scalar-pending-host-ok entry=%u host=%u steps=%zu callbacks=%zu terminal=%zu\n",fixture_entry_status,fixture_host_status,fixture_steps,fixture_host_calls,fixture_terminal_calls);
  return 0;
}
"#;
    let directory = TempDir::new("task-short-pending");
    let path = directory.path().join("pending_publication.c");
    fs::write(&path, source).unwrap();
    let Some(executable) = compile_harness(directory.path(), &path, "pending_publication") else {
        assert!(
            std::env::var_os("GITHUB_ACTIONS").is_none(),
            "CI requires a native C compiler"
        );
        return;
    };
    fs::remove_file(path).unwrap();
    let output = run_bounded(&mut Command::new(executable), RUN_TIMEOUT, RUN_LIMITS).unwrap();
    assert!(
        output.status.success(),
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout).replace('\r', "");
    assert!(
        stdout.starts_with("short-scalar-pending-host-ok entry=0 host=1 "),
        "{stdout}"
    );
    assert!(output.stderr.is_empty());
    eprint!("{stdout}");
}

#[test]
fn native_task_scalar_quantum_reconciles_owned_changes_children_and_cleanup() {
    let source = r#"
import fs from "std.fs"
async fn Work(value: str): str! {
    text=value+"a"
    i=0; total=3
    while(i<1000) { total=(total*33+i+3)%100000000; i=i+1 }
    text=text+"b"
    i=0
    while(i<1000) { total=(total*33+i+3)%100000000; i=i+1 }
    return ok(text)
}
async fn Child(value: str): str! { return ok(value) }
async fn Parent(value: str): str! {
    i=0; total=3
    while(i<1000) { total=(total*33+i+3)%100000000; i=i+1 }
    child=Child(value)
    i=0
    while(i<1000) { total=(total*33+i+3)%100000000; i=i+1 }
    text=(await child)?
    text=text+"c"
    i=0
    while(i<1000) { total=(total*33+i+3)%100000000; i=i+1 }
    return ok(text)
}
async fn Blocking(value: str): str! {
    text=value+"d"
    i=0; while(i<50000) { i=i+1 }
    found=fs.exists("not-reached-by-fixture")
    return ok(text)
}
async fn Finally(value: str): str! {
    text=value+"e"
    i=0
    try { while(i<50000) { i=i+1 } } finally { text=text+"z" }
    return ok(text)
}
async fn Failure(value: str): str! {
    text=value+"f"
    i=0; while(i<1000) { i=i+1 }
    large=9223372036854775807
    overflow=large+1
    return ok(text)
}
async fn main(): null! { return ok(null) }
"#;
    let mut ast = Parser::new(Lexer::new(source).lex().unwrap())
        .parse_program()
        .unwrap();
    ast.items
        .retain(|item| !matches!(item, ku::ast::Item::Import(_)));
    ast.items.insert(
        0,
        ku::ast::Item::Module(ku::ast::ModuleDecl {
            name: "std:fs".into(),
            span: Default::default(),
        }),
    );
    Checker::new().check(&ast).unwrap();
    let tasks = task_lower::lower_program(&ast).unwrap();
    let child_mask = tasks.tasks.functions[2]
        .slots
        .iter()
        .enumerate()
        .filter(|(_, slot)| matches!(slot.ty, ku::ir::task::TaskSlotType::Task { .. }))
        .fold(0u64, |mask, (index, _)| mask | (1u64 << index));
    assert_ne!(child_mask, 0);
    let mut generated = c::generate_native_task_c_source(&tasks, &Default::default()).unwrap();
    let frame_plan = ku::ir::task::verify_and_plan(&tasks.tasks, Default::default()).unwrap();
    generated = generated.replacen(
        "typedef struct KuString {",
        "static size_t fixture_scans[6],fixture_skipped[6],fixture_pure_scans[6],fixture_mixed_scans[6],fixture_terminal_scans[6],fixture_child_scans,fixture_copy_flushes[6],fixture_copy_returns[6];\ntypedef struct KuString {",
        1,
    );
    for id in [0, 2, 3, 5] {
        let function = &tasks.tasks.functions[id];
        let frame = &frame_plan.functions[id];
        let mask = frame
            .cooperative_slots
            .iter()
            .filter(|slot| {
                matches!(
                    function.slots[slot.0].ty,
                    ku::ir::task::TaskSlotType::Value {
                        ty: ku::ir::IrType::Int | ku::ir::IrType::Bool | ku::ir::IrType::Null,
                        borrowed: false
                    }
                )
            })
            .fold(0u64, |mask, slot| mask | (1u64 << slot.0));
        assert_ne!(mask, 0);
        if id == 2 {
            assert_eq!(mask & child_mask, 0);
        }
        let begin = generated
            .find(&format!("static uint32_t ku_task_frame_{id}_drive("))
            .unwrap();
        let end = begin
            + generated[begin..]
                .find(&format!("static uint32_t ku_task_frame_{id}_resume("))
                .unwrap();
        let mut drive = generated[begin..end].to_owned();
        let active = "  int __ku_copy_init_active=0;\n";
        assert_eq!(drive.matches(active).count(), 1);
        drive = drive.replacen(active, &format!("{active}  int fixture_copy_seen=0;\n"), 1);
        let entered=format!("  if (!__ku_copy_init_active) {{ __ku_copy_initialized=frame->header.initialized & UINT64_C({mask}); __ku_copy_init_active=1; }}\n");
        assert!(drive.contains(&entered));
        drive = drive.replace(&entered, &format!("{entered}  fixture_copy_seen=1;\n"));
        let flushed=format!("  if (__ku_copy_init_active) {{\n    frame->header.initialized=(frame->header.initialized & ~UINT64_C({mask})) | (__ku_copy_initialized & UINT64_C({mask}));\n    __ku_copy_init_active=0;\n  }}\n");
        assert!(drive.contains(&flushed));
        // This runs immediately before generic dispatch clock/_check helpers,
        // non-pure host states and terminated cleanup inspect the real header.
        let observed=format!("{flushed}  if (__ku_scalar_phase) abort();\n  if (fixture_copy_seen) {{ if ((frame->header.initialized & UINT64_C({mask})) != (__ku_copy_initialized & UINT64_C({mask}))) abort(); fixture_copy_seen=0; fixture_copy_flushes[{id}]++; }}\n");
        drive = drive.replace(&flushed, &observed);
        generated.replace_range(begin..end, &drive);
        let begin = generated
            .find(&format!(
                "static uint32_t ku_task_frame_{id}_spill_scalars("
            ))
            .unwrap();
        let body = begin + generated[begin..].find(") {\n").unwrap() + 4;
        let helper_end = body + generated[body..].find("\n}\n").unwrap();
        let body = body
            + generated[body..helper_end]
                .find("  if (copy_active)")
                .expect("scalar spill must still merge Copy initialization bits");
        let finish = body + generated[body..].find('\n').unwrap() + 1;
        let merge = generated[body..finish].to_owned();
        assert!(merge.contains("if (copy_active)"));
        generated.replace_range(body..finish,&format!("  uint64_t fixture_owned_before=frame->header.initialized & ~UINT64_C({mask});\n{merge}  if ((frame->header.initialized & ~UINT64_C({mask}))!=fixture_owned_before) abort();\n  if (copy_active) {{ if ((frame->header.initialized & UINT64_C({mask}))!=(copy_initialized & UINT64_C({mask}))) abort(); fixture_copy_returns[{id}]++; }}\n"));
    }
    for id in [0, 2, 3, 4, 5] {
        let reconcile = format!("static uint32_t ku_task_{id}_reconcile_charge(void* raw) {{");
        assert_eq!(generated.matches(&reconcile).count(), 1);
        generated = generated.replacen(
            &reconcile,
            &format!("{reconcile}\n  fixture_scans[{id}]++;"),
            1,
        );
        let resume = format!("static uint32_t ku_task_{id}_resume(void* raw) {{");
        assert_eq!(generated.matches(&resume).count(), 1);
        generated = generated.replacen(
            &resume,
            &format!(
                r#"static uint32_t ku_task_{id}_resume_inner(void* raw);
static uint32_t ku_task_{id}_resume(void* raw) {{
  size_t before=fixture_scans[{id}];
  uint32_t outcome=ku_task_{id}_resume_inner(raw);
  KuTaskInstance_{id}* instance=(KuTaskInstance_{id}*)raw;
  KuTaskDriverExecutionV1* execution=ku_task_driver_execution;
  int scalar=execution && execution->cooperative_yield && execution->scalar_quantum;
  if(outcome==KU_TASK_CONTROL_PENDING && scalar) {{
    if(fixture_scans[{id}]==before) fixture_skipped[{id}]++;
    else fixture_pure_scans[{id}]++;
    if({id}u==2u && (instance->frame.header.initialized & UINT64_C({child_mask}))
      && fixture_scans[{id}]>before) fixture_child_scans++;
  }} else if(fixture_scans[{id}]>before) {{
    if(outcome==KU_TASK_CONTROL_PENDING) fixture_mixed_scans[{id}]++;
    else fixture_terminal_scans[{id}]++;
  }}
  return outcome;
}}
static uint32_t ku_task_{id}_resume_inner(void* raw) {{"#
            ),
            1,
        );
    }
    let generated = generated.replacen(
        "int main(void) {",
        "static int ku_unused_source_main(void) {",
        1,
    );
    let source = generated
        + r#"
#define CHECK(c) do { if (!(c)) { fprintf(stderr,"scalar reconcile line %d: %s\n",__LINE__,#c); return 1; } } while (0)
static void fixture_wake(void* raw,size_t owner) { (void)raw; (void)owner; }
static int fixture_case(size_t id,int cancel) {
  KuTaskDriverV1 driver={0}; KuTaskDriverSlotV1 slots[4]={0}; size_t ring[4]={0};
  KuTaskValueV1 task={0};
  size_t fixed=sizeof(driver)+sizeof(slots)+sizeof(ring);
  uint64_t deadline=ku_task_driver_now_ms()+2000u, next=UINT64_MAX;
  CHECK(ku_task_driver_init_external(&driver,sizeof(driver),KU_TASK_DRIVER_ABI_VERSION,
    slots,4u,ring,4u,fixed+131072u,1u,deadline,NULL,fixture_wake)==KU_TASK_DRIVER_OK);
  uint8_t* buffer=malloc(16u); CHECK(buffer); memcpy(buffer,"payload",7u);
  KuString input={buffer,7u,16u,KU_STRING_OWNED};
  uint32_t started=id==0u ? ku_task_0_start_value(&driver,&input,&task)
    : id==2u ? ku_task_2_start_value(&driver,&input,&task)
    : id==3u ? ku_task_3_start_value(&driver,&input,&task)
    : id==4u ? ku_task_4_start_value(&driver,&input,&task)
    : ku_task_5_start_value(&driver,&input,&task);
  CHECK(started==KU_TASK_DRIVER_OK && !input.ptr);
  KuTaskDriverTicketV1 old=task.ticket;
  size_t before_skip=fixture_skipped[id], before_pure=fixture_pure_scans[id];
  for(size_t turn=0;turn<1024u;turn++) {
    CHECK(ku_task_driver_pump(&driver,0u,1u,&next)<=KU_TASK_DRIVER_PENDING);
    if(cancel && (fixture_skipped[id]>before_skip+4u || fixture_pure_scans[id]>before_pure+4u)) break;
    if(ku_task_control_status(&task.owner.lease)!=KU_TASK_CONTROL_PENDING) break;
  }
  if(cancel) {
    size_t before=fixture_scans[id];
    CHECK(ku_task_value_drop(&task,deadline)==KU_TASK_DRIVER_OK);
    for(size_t turn=0;turn<256u && driver.resident;turn++)
      CHECK(ku_task_driver_pump(&driver,0u,4u,&next)<=KU_TASK_DRIVER_PENDING);
    CHECK(fixture_scans[id]>before);
  } else if(id==5u) {
    CHECK(ku_task_control_status(&task.owner.lease)==KU_TASK_CONTROL_FAILED);
    KuTaskAdapterOutcomeV1 outcome={0}; size_t before=fixture_scans[id];
    CHECK(ku_task_value_take(&task,NULL,&outcome)==KU_TASK_CONTROL_OK);
    CHECK(fixture_scans[id]>before && outcome.result_kind==4u && !outcome.value.string.ok);
    ku_task_outcome_drop(&outcome);
    CHECK(ku_task_value_drop(&task,deadline)==KU_TASK_DRIVER_OK);
    CHECK(fixture_skipped[id]>before_skip && fixture_terminal_scans[id]);
  } else {
    CHECK(ku_task_control_status(&task.owner.lease)==KU_TASK_CONTROL_COMPLETED);
    KuTaskAdapterOutcomeV1 outcome={0}; size_t before=fixture_scans[id];
    CHECK(ku_task_value_take(&task,NULL,&outcome)==KU_TASK_CONTROL_OK);
    CHECK(fixture_scans[id]>before && outcome.result_kind==4u && outcome.value.string.ok);
    KuString text=outcome.value.string.value;
    CHECK(text.len==(id==0u ? 9u : 8u) && !memcmp(text.ptr,id==0u ? "payloadab" : "payloadc",text.len));
    ku_task_outcome_drop(&outcome);
    CHECK(ku_task_value_drop(&task,deadline)==KU_TASK_DRIVER_OK);
    CHECK(fixture_skipped[id]>before_skip && fixture_mixed_scans[id] && fixture_terminal_scans[id]);
  }
  CHECK(ku_task_driver_request_close(&driver,deadline)==KU_TASK_DRIVER_OK);
  for(size_t turn=0;turn<256u && driver.workers_exited!=1u;turn++)
    CHECK(ku_task_driver_pump(&driver,0u,4u,&next)<=KU_TASK_DRIVER_PENDING);
  CHECK(!driver.resident && !driver.running && !driver.queued && !driver.timers
    && !driver.reserved_bytes && driver.workers_exited==1u && !driver.fault);
  CHECK(!ku_task_driver_execution && ku_task_driver_wake(&old)==KU_TASK_DRIVER_STALE);
  CHECK(ku_task_driver_join(&driver,deadline)==KU_TASK_DRIVER_OK);
  CHECK(ku_task_driver_destroy(&driver)==KU_TASK_DRIVER_OK);
  return 0;
}
int main(void) {
  CHECK(!fixture_case(0u,0)); CHECK(!fixture_case(0u,1));
  CHECK(!fixture_case(2u,0)); CHECK(fixture_child_scans);
  CHECK(!fixture_case(3u,1)); CHECK(!fixture_case(4u,1));
  CHECK(!fixture_case(5u,0));
  CHECK(fixture_pure_scans[3] && !fixture_skipped[3]);
  CHECK(fixture_pure_scans[4] && !fixture_skipped[4]);
  CHECK(fixture_copy_flushes[0] && fixture_copy_flushes[2] && fixture_copy_flushes[5]);
  /* Blocking is cancelled inside its pure loop: only the return boundary is reached. */
  CHECK(!fixture_copy_flushes[3] && fixture_copy_returns[3]);
  CHECK(fixture_copy_returns[0] && fixture_copy_returns[2] && fixture_copy_returns[3] && fixture_copy_returns[5]);
  puts("scalar-quantum-owned-child-cleanup-ok"); return 0;
}
"#;
    let directory = TempDir::new("task-scalar-reconcile");
    let path = directory.path().join("scalar_reconcile.c");
    fs::write(&path, source).unwrap();
    let Some(executable) = compile_harness(directory.path(), &path, "scalar_reconcile") else {
        assert!(
            std::env::var_os("GITHUB_ACTIONS").is_none(),
            "CI requires a native C compiler"
        );
        return;
    };
    fs::remove_file(path).unwrap();
    let output = run_bounded(&mut Command::new(executable), RUN_TIMEOUT, RUN_LIMITS).unwrap();
    assert!(
        output.status.success(),
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).replace('\r', ""),
        "scalar-quantum-owned-child-cleanup-ok\n"
    );
    assert!(output.stderr.is_empty());
}

const MOVES_AND_AWAITS: &str = r#"
async fn main(): null! {
    first = Child(3)
    second = Child(4)
    moved = first
    a = (await moved)?
    b = (await second)?
    text = (await Text("Ku 世界"))?
    println(a)
    println(b)
    println(text)
    return ok(null)
}
async fn Child(value: int): int! { return ok(value) }
async fn Text(value: str): str! { return ok(value) }
"#;

#[test]
fn native_task_source_generated_start_await_and_scope_execute_without_source() {
    let cases = [
        (MOVES_AND_AWAITS, "3\n4\nKu 世界\n", "", true),
        (
            "async fn main(): null! { first = Child(1) second = Child(2) third = Child(3) return ok(null) } async fn Child(value: int): int! { return ok(value) }",
            "", "", true,
        ),
        (
            "async fn Flag(value: bool): bool! { return ok(value) } async fn Empty(): null! { return ok(null) } async fn main(): null! { flag = (await Flag(true))? empty = (await Empty())? print(flag) println(empty) return ok(null) }",
            "truenull\n", "", true,
        ),
        (
            "async fn Broken(): int! { fail \"expected failure\" } async fn main(): null! { child = Broken() result = await child println(7) return ok(null) }",
            "7\n", "", true,
        ),
        (
            "async fn Broken(): int! { fail \"expected failure\" } async fn main(): null! { child = Broken() value = (await child)? println(value) return ok(null) }",
            "", "expected failure\n", false,
        ),
        (
            "async fn Number(): int! { return ok(8) } async fn Carry(text: str, value: int): str! { println(value) return ok(text) } async fn main(): null! { text = (await Carry(\"argument before await\", (await Number())?))? println(text) return ok(null) }",
            "8\nargument before await\n", "", true,
        ),
        (
            "async fn Unwrap(value: str!): str! { return value } async fn main(): null! { wrapped = ok(\"a\0b 世界\") text = (await Unwrap(wrapped))? println(text) return ok(null) }",
            "a\0b 世界\n", "", true,
        ),
        (
            "async fn println(value: int): null! { return ok(null) } async fn main(): null! { ignored = println(7) return ok(null) }",
            "", "", true,
        ),
        (
            "async fn main(): null! { text = \"kept\" text println(text) return ok(null) }",
            "kept\n", "", true,
        ),
        (
            "async fn ok(value: int): null! { fail \"user ok failed\" } async fn main(): null! { child = ok(7) result = await child fail \"main done\" }",
            "", "main done\n", false,
        ),
    ];
    for (index, (source, expected_out, expected_err, success)) in cases.into_iter().enumerate() {
        let generated = native(source);
        for forbidden in ["run_source", "const SOURCE", "Task.new", "task.spawn"] {
            assert!(!generated.contains(forbidden));
        }
        assert!(generated.contains("ku_task_driver_wait_result("));
        assert!(generated.contains("ku_task_root_driver"));
        let directory = TempDir::new(&format!("native-task-source-{index}"));
        let path = directory.path().join("program.c");
        fs::write(&path, generated).unwrap();
        let Some(executable) = compile_harness(directory.path(), &path, "program") else {
            assert!(
                std::env::var_os("GITHUB_ACTIONS").is_none(),
                "CI must execute native source Tasks"
            );
            continue;
        };
        fs::remove_file(path).unwrap();
        let output = run_bounded(
            Command::new(executable).current_dir(directory.path()),
            RUN_TIMEOUT,
            RUN_LIMITS,
        )
        .expect("native source Tasks terminate within the bounded process watchdog");
        assert_eq!(
            output.status.success(),
            success,
            "case {index}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            String::from_utf8(output.stdout).unwrap().replace('\r', ""),
            expected_out,
            "case {index}"
        );
        assert_eq!(
            String::from_utf8(output.stderr).unwrap().replace('\r', ""),
            expected_err,
            "case {index}"
        );
    }
}

#[test]
fn native_task_source_lowering_keeps_pending_away_from_argument_and_move_operations() {
    use ku::ir::task::{TaskOp, TaskTerminator};
    let native = task_lower::lower_program(&checked(MOVES_AND_AWAITS)).unwrap();
    let main = native
        .tasks
        .functions
        .iter()
        .find(|function| function.id == native.entry)
        .unwrap();
    let awaits = main
        .states
        .iter()
        .filter(|state| matches!(state.terminator, TaskTerminator::Await { .. }))
        .collect::<Vec<_>>();
    assert_eq!(awaits.len(), 3);
    assert!(awaits.iter().all(|state| state.operations.is_empty()));
    assert_eq!(
        main.states
            .iter()
            .flat_map(|state| &state.operations)
            .filter(|operation| matches!(operation, TaskOp::Start { .. }))
            .count(),
        3
    );
}

#[test]
fn native_task_source_exit_owns_normal_results_without_erasing_await_cleanup() {
    use ku::ir::{
        task::{self, SlotId, TaskConstant, TaskLimits, TaskOp, TaskSlotType, TaskTerminator},
        IrType,
    };

    let source = r#"
async fn ReturnText(value: str): str! { local = value return ok(local) }
async fn FailText(): str! { retained = "held" fail "expected failure" }
async fn Question(value: str!): str! { local = value? return ok(local) }
async fn ShortAnd(gate: bool): bool! { return ok(gate && (await Flag())?) }
async fn ShortOr(gate: bool): bool! { return ok(gate || (await Flag())?) }
async fn Flag(): bool! { return ok(true) }
async fn AwaitText(value: str): str! {
    retained = "held across await"
    child = ReturnText(value)
    output = (await child)?
    println(retained)
    return ok(output)
}
async fn main(): null! { return ok(null) }
"#;
    let native = task_lower::lower_program(&checked(source)).unwrap();
    let plan = task::verify_and_plan(&native.tasks, TaskLimits::default()).unwrap();
    let mut await_count = 0;
    for function in &native.tasks.functions {
        let frame = plan
            .functions
            .iter()
            .find(|frame| frame.function == function.id)
            .unwrap();
        assert!(frame.exit_bridge && frame.hosted, "{}", function.name);
        let owned: Vec<_> = function
            .slots
            .iter()
            .enumerate()
            .filter(|(_, slot)| {
                matches!(&slot.ty, TaskSlotType::Value { ty, borrowed: false }
                    if matches!(ty, IrType::Str | IrType::Result(_)))
            })
            .map(|(index, _)| SlotId(index))
            .collect();
        assert!(owned.iter().all(|slot| frame.slots.contains(slot)));
        let cleanup: Vec<_> = owned
            .iter()
            .rev()
            .map(|slot| TaskOp::DropIfInit { slot: *slot })
            .collect();
        let mut exit_count = 0;
        for state in &function.states {
            match state.terminator {
                TaskTerminator::Exit { value } => {
                    exit_count += 1;
                    assert_eq!(
                        function.slots[value.0].ty,
                        TaskSlotType::Value {
                            ty: function.result.clone(),
                            borrowed: false,
                        }
                    );
                    assert!(
                        state
                            .operations
                            .iter()
                            .all(|operation| { !matches!(operation, TaskOp::DropIfInit { .. }) }),
                        "{} normal Exit must leave remaining Values for handoff-first glue",
                        function.name
                    );
                }
                TaskTerminator::Complete { .. } => {
                    panic!("{} still uses legacy normal completion", function.name)
                }
                TaskTerminator::Await {
                    cleanup: cleanup_state,
                    ..
                } => {
                    await_count += 1;
                    assert!(state.operations.is_empty(), "Pending must not replay setup");
                    let block = &function.states[cleanup_state.0];
                    assert_eq!(block.terminator, TaskTerminator::Terminate);
                    // Keep every conditional Value drop in reverse declaration
                    // order, including slots only initialized on another edge.
                    // Task owners are deliberately absent: the host hands them
                    // off before entering this cancellation-only successor.
                    assert_eq!(block.operations, cleanup, "{}", function.name);
                }
                _ => {}
            }
        }
        assert!(exit_count > 0, "{} has no normal Exit", function.name);
    }
    assert_eq!(await_count, 3);

    let failed = native
        .tasks
        .functions
        .iter()
        .find(|function| function.name == "FailText")
        .unwrap();
    let state = &failed.states[failed.entry.0];
    let TaskTerminator::Exit { value } = state.terminator else {
        panic!("fail must stage a normal user Result");
    };
    assert!(state.operations.iter().any(|operation| {
        matches!(operation, TaskOp::Init {
            dst,
            value: TaskConstant::Err { domain, code, message, .. },
        } if *dst == value && domain == "ku" && code == "fail" && message == "expected failure")
    }));

    let question = native
        .tasks
        .functions
        .iter()
        .find(|function| function.name == "Question")
        .unwrap();
    let TaskTerminator::TryResult {
        ok_value,
        err_result,
        ok,
        err,
        ..
    } = question.states[question.entry.0].terminator
    else {
        panic!("? must retain both typed Result successors");
    };
    assert_eq!(
        question.states[err.0].terminator,
        TaskTerminator::Exit { value: err_result }
    );
    let success = &question.states[ok.0];
    assert!(matches!(success.terminator, TaskTerminator::Exit { .. }));
    assert!(success
        .operations
        .iter()
        .any(|operation| matches!(operation, TaskOp::Move { src, .. } if *src == ok_value)));

    for (name, right_on_true) in [("ShortAnd", true), ("ShortOr", false)] {
        let function = native
            .tasks
            .functions
            .iter()
            .find(|function| function.name == name)
            .unwrap();
        let TaskTerminator::Branch {
            then_state,
            else_state,
            ..
        } = function.states[function.entry.0].terminator
        else {
            panic!("{name} lost short-circuit control flow");
        };
        let (right, join) = if right_on_true {
            (then_state, else_state)
        } else {
            (else_state, then_state)
        };
        let skipped = &function.states[join.0];
        assert!(matches!(skipped.terminator, TaskTerminator::Exit { .. }));
        assert!(skipped
            .operations
            .iter()
            .all(|operation| !matches!(operation, TaskOp::Start { .. })));
        assert_eq!(
            function.states[right.0]
                .operations
                .iter()
                .filter(|operation| matches!(operation, TaskOp::Start { .. }))
                .count(),
            1
        );
        let TaskTerminator::Jump { target: poll } = function.states[right.0].terminator else {
            panic!("{name} RHS must set up its child before a separate Await");
        };
        let TaskTerminator::Await { ready, .. } = function.states[poll.0].terminator else {
            panic!("{name} RHS lost Await");
        };
        let TaskTerminator::TryResult {
            ok,
            err,
            err_result,
            ..
        } = function.states[ready.0].terminator
        else {
            panic!("{name} RHS must propagate the awaited Result");
        };
        assert_eq!(
            function.states[err.0].terminator,
            TaskTerminator::Exit { value: err_result }
        );
        assert_eq!(
            function.states[ok.0].terminator,
            TaskTerminator::Jump { target: join }
        );
        assert!(function.states[ok.0]
            .operations
            .iter()
            .any(|operation| matches!(operation, TaskOp::Copy { .. })));
    }
}

#[test]
fn native_task_source_zero_task_exit_persists_owned_locals_not_dead_copy_temporaries() {
    use ku::ir::{
        task::{self, SlotId, TaskLimits, TaskOp, TaskSlotType, TaskTerminator},
        IrType,
    };

    // A legitimate typed host may supply Owned storage for value. This source
    // only moves it to a local; it does not introduce source heap-allocation syntax.
    let source = r#"
async fn Local(value: str): str! {
    local = value
    number = 7
    println(number)
    return ok(local)
}
async fn main(): null! { return ok(null) }
"#;
    let native = task_lower::lower_program(&checked(source)).unwrap();
    let plan = task::verify_and_plan(&native.tasks, TaskLimits::default()).unwrap();
    let function = &native.tasks.functions[0];
    assert_eq!(function.name, "Local");
    let frame = plan
        .functions
        .iter()
        .find(|frame| frame.function == function.id)
        .unwrap();
    assert!(frame.exit_bridge && frame.hosted);
    assert_eq!(frame.scope_task_mask, 0);
    assert!(frame.suspensions.is_empty());
    let state = &function.states[function.entry.0];
    assert!(matches!(state.terminator, TaskTerminator::Exit { .. }));
    let moved_local = state
        .operations
        .iter()
        .find_map(|operation| match operation {
            TaskOp::Move { dst, src } if function.parameters.contains(src) => Some(*dst),
            _ => None,
        })
        .expect("source binding must move the owned parameter to a distinct local");
    assert!(!function.parameters.contains(&moved_local));
    assert!(frame.slots.contains(&moved_local));
    let mut dead_copy = 0;
    let mut dead_null = 0;
    for (index, slot) in function.slots.iter().enumerate() {
        let id = SlotId(index);
        match &slot.ty {
            TaskSlotType::Value {
                ty: IrType::Int,
                borrowed: false,
            } if !function.parameters.contains(&id) => {
                dead_copy += 1;
                assert!(
                    !frame.slots.contains(&id),
                    "dead Copy slot {index} was spilled"
                );
            }
            TaskSlotType::Value {
                ty: IrType::Null,
                borrowed: false,
            } => {
                dead_null += 1;
                assert!(
                    !frame.slots.contains(&id),
                    "unused print result {index} was spilled"
                );
                assert!(state.operations.iter().any(|operation| {
                    matches!(operation, TaskOp::Init { dst, value: task::TaskConstant::Null } if *dst == id)
                }));
            }
            TaskSlotType::Value {
                ty: IrType::Str | IrType::Result(_),
                borrowed: false,
            } => assert!(
                frame.slots.contains(&id),
                "Owned slot {index} was not persisted"
            ),
            other => panic!("unexpected zero-Task fixture slot: {other:?}"),
        }
    }
    assert_eq!(
        dead_copy, 2,
        "literal and local Copy snapshots must both exist"
    );
    assert_eq!(dead_null, 1, "println has one unused null result");
}

#[test]
fn native_task_source_subset_rejects_unimplemented_constructs() {
    let sources = [
        "async fn main(): null! { for item in [[1], [2]] {} return ok(null) }",
        "async fn main(): null! { try { println(1) } finally { for item in 0 { println(item) } } return ok(null) }",
        "fn Sync(): int { return 1 } async fn main(): null! { return ok(null) }",
        "async fn Child(): int! { return ok(1) } async fn main(): null! { chosen = Child chosen = Child return ok(null) }",
        "async fn Child(): int! { return ok(1) } async fn main(): null! { Child() return ok(null) }",
        "async fn main(): int! { return ok(1) }",
        "async fn Child(): int! { value = (await Child())? return ok(value) } async fn main(): null! { return ok(null) }",
    ];
    for source in sources {
        let error = task_lower::lower_program(&checked(source))
            .expect_err("unimplemented source must be rejected");
        assert!(
            error.message.contains("not support") || error.message.contains("recursive"),
            "{error}"
        );
    }
}

#[test]
fn native_task_source_ordinary_synchronous_artifacts_remain_runtime_free() {
    let ast = checked("fn main() { println(7) }");
    assert!(!task_lower::has_async_entry(&ast));
    let generated = c::generate_c_source(&ku::ir::lower_program(&ast).unwrap()).unwrap();
    for forbidden in [
        "KuTaskValue",
        "ku_task_driver_",
        "ku_task_root_",
        "_beginthreadex",
        "pthread_create",
    ] {
        assert!(
            !generated.contains(forbidden),
            "synchronous source gained Task runtime: {forbidden}"
        );
    }
}

#[test]
fn native_task_source_cli_imports_build_and_run_after_source_graph_moves() {
    for emit_only in [true, false] {
        let directory = TempDir::new("native-task-cli-import");
        let sources = directory.path().join("sources");
        fs::create_dir(&sources).unwrap();
        fs::write(
            sources.join("worker.ku"),
            "import time from \"std.time\"\nasync fn Text(value: str): str! { time.sleep(1)? return ok(value) }",
        )
        .unwrap();
        let main = sources.join("main.ku");
        fs::write(&main, "import { Text } from \"./worker.ku\"\nasync fn main(): null! { for item in 3 { if (item == 0) { continue } result = (await Text(\"import 世界\"))? println(result) break } return ok(null) }").unwrap();
        let binary = directory
            .path()
            .join(if cfg!(windows) { "app.exe" } else { "app" });
        let mut command = Command::new(env!("CARGO_BIN_EXE_ku"));
        command.current_dir(directory.path()).arg("build");
        if emit_only {
            command.arg("--native").arg(&main);
        } else {
            command
                .args(["--backend", "c"])
                .arg(&main)
                .arg("-o")
                .arg(&binary);
        }
        let built = run_bounded(&mut command, BUILD_TIMEOUT, BUILD_LIMITS).unwrap();
        let diagnostics = format!(
            "{}{}",
            String::from_utf8_lossy(&built.stdout),
            String::from_utf8_lossy(&built.stderr)
        );
        assert!(
            built.status.success() || (!emit_only && diagnostics.contains("C compiler not found")),
            "{diagnostics}"
        );
        let artifact = if emit_only {
            main.with_extension("c")
        } else {
            let root = sources.join(".ku/build/debug/c");
            let artifacts: Vec<_> = fs::read_dir(&root)
                .unwrap()
                .take(257)
                .map(|entry| entry.unwrap().path().join("app.c"))
                .filter(|path| path.is_file())
                .collect();
            assert_eq!(
                artifacts.len(),
                1,
                "expected one output-digest C artifact under {}",
                root.display()
            );
            artifacts[0].clone()
        };
        let generated = fs::read_to_string(&artifact)
            .unwrap_or_else(|error| panic!("{}: {error}\n{diagnostics}", artifact.display()));
        assert!(
            generated.contains("ku_task_root_driver") && generated.contains("ku_task_host_await")
        );
        assert!(!generated.contains("run_source") && !generated.contains("const SOURCE"));
        let detached = directory.path().join("detached.c");
        fs::write(&detached, generated).unwrap();
        fs::rename(&sources, directory.path().join("moved-sources")).unwrap();
        assert!(!sources.exists());
        let executable = if emit_only {
            compile_harness(directory.path(), &detached, "app")
        } else if built.status.success() {
            Some(binary)
        } else {
            None
        };
        let Some(executable) = executable else {
            assert!(
                std::env::var_os("GITHUB_ACTIONS").is_none(),
                "CI must link and run native Task CLI artifacts"
            );
            continue;
        };
        fs::remove_file(detached).unwrap();
        let output = run_bounded(
            Command::new(executable).current_dir(directory.path()),
            RUN_TIMEOUT,
            RUN_LIMITS,
        )
        .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            String::from_utf8(output.stdout).unwrap().replace('\r', ""),
            "import 世界\n"
        );
        assert!(output.stderr.is_empty());
    }
}

#[test]
fn native_task_source_cli_rejects_unsupported_and_moved_values_before_artifacts() {
    let sources = [
        "async fn main(): null! { try { println(1) } finally { while (false) { println(0) } } return ok(null) }",
        "async fn Child(): str! { return ok(\"child\") } async fn main(): null! { child = Child() moved = child value = await child return ok(null) }",
        "async fn Child(value: str): str! { return ok(value) } async fn main(): null! { value = \"owned\" child = Child(value) println(value) return ok(null) }",
    ];
    for source in sources {
        let directory = TempDir::new("native-task-cli-reject");
        let main = directory.path().join("main.ku");
        fs::write(&main, source).unwrap();
        let error = ku::cli::run_cli(vec![
            "ku".into(),
            "build".into(),
            "--native".into(),
            main.to_string_lossy().into_owned(),
        ])
        .expect_err("unsupported or invalid ownership must not produce native code");
        assert!(
            error.message.contains("not support")
                || error.message.contains("moved")
                || error.message.contains("has already been awaited"),
            "{error}"
        );
        assert!(!main.with_extension("c").exists());
    }
}

#[test]
fn native_task_source_output_failure_unwinds_await_without_question_mark() {
    let generated = native("async fn Write(): int! { print(3) return ok(1) } async fn main(): null! { result = await Write() println(7) return ok(null) }");
    let anchor = "typedef struct KuString {";
    assert_eq!(generated.matches(anchor).count(), 1);
    // A real generated Print calls the standard flush and then receives a
    // deterministic I/O failure. No replacement Start/Await/cleanup callback.
    let source = generated.replacen(
        anchor,
        &format!(
            r#"
static int ku_fixture_flush(FILE* stream) {{
  int status=fflush(stream);
  return stream==stdout ? EOF : status;
}}
#define fflush ku_fixture_flush
{anchor}"#
        ),
        1,
    );
    let directory = TempDir::new("native-task-output-failure");
    let path = directory.path().join("program.c");
    fs::write(&path, source).unwrap();
    let Some(executable) = compile_harness(directory.path(), &path, "program") else {
        assert!(
            std::env::var_os("GITHUB_ACTIONS").is_none(),
            "CI must run native Task output failure"
        );
        return;
    };
    let output = run_bounded(
        Command::new(executable).current_dir(directory.path()),
        RUN_TIMEOUT,
        RUN_LIMITS,
    )
    .unwrap();
    assert!(!output.status.success());
    assert_eq!(output.stdout, b"3");
    assert_eq!(
        String::from_utf8(output.stderr).unwrap().replace('\r', ""),
        "output write failed\n"
    );
}

#[test]
fn native_task_source_diagnostic_write_failure_still_destroys_runtime() {
    let generated = native("async fn Child(): int! { return ok(1) } async fn main(): null! { child = Child() fail \"expected failure\" }");
    let anchor = "typedef struct KuString {";
    assert_eq!(generated.matches(anchor).count(), 1);
    let source = generated.replacen(
        anchor,
        &format!(
            r#"
static size_t ku_fixture_write(const void* data, size_t size, size_t count, FILE* stream) {{
  return stream==stderr ? 0 : fwrite(data,size,count,stream);
}}
#define fwrite ku_fixture_write
{anchor}"#
        ),
        1,
    );
    let done = "  return exit_code;\n}\n";
    assert_eq!(source.matches(done).count(), 1);
    let source = source.replacen(
        done,
        "  fputs(\"runtime destroyed\\n\",stdout);\n  return exit_code;\n}\n",
        1,
    );
    let directory = TempDir::new("native-task-diagnostic-failure");
    let path = directory.path().join("program.c");
    fs::write(&path, source).unwrap();
    let Some(executable) = compile_harness(directory.path(), &path, "program") else {
        assert!(
            std::env::var_os("GITHUB_ACTIONS").is_none(),
            "CI must run native Task diagnostic failure"
        );
        return;
    };
    let output = run_bounded(
        Command::new(executable).current_dir(directory.path()),
        RUN_TIMEOUT,
        RUN_LIMITS,
    )
    .unwrap();
    assert!(!output.status.success());
    assert_eq!(
        String::from_utf8(output.stdout).unwrap().replace('\r', ""),
        "runtime destroyed\n"
    );
    assert_eq!(
        String::from_utf8(output.stderr).unwrap().replace('\r', ""),
        "\n"
    );
}

#[test]
fn native_task_sleep_recognizes_only_imported_unshadowed_std_time_integer_calls() {
    use ku::ir::task::{SlotId, TaskTerminator};
    for import in [
        "import time from \"std.time\"",
        "import \"std.time\"",
        "import { time } from \"std\"",
    ] {
        // Exercise the lowerer's import boundary directly as well as the CLI's
        // separately covered synthetic std:time capability after expansion.
        let source = format!(
            "{import}\nasync fn main(): null! {{ ms = 1 time.sleep(ms)? return ok(null) }}"
        );
        let ast = Parser::new(Lexer::new(&source).lex().unwrap())
            .parse_program()
            .unwrap();
        let lowered = task_lower::lower_program(&ast).expect("supported standard time import");
        let function = &lowered.tasks.functions[0];
        let waits: Vec<_> = function
            .states
            .iter()
            .filter_map(|state| match state.terminator {
                TaskTerminator::Timer { millis, dst, .. } => Some((millis, dst)),
                _ => None,
            })
            .collect();
        assert_eq!(waits.len(), 1);
        assert_ne!(
            waits[0].0,
            SlotId(0),
            "milliseconds use an immutable materialized snapshot"
        );
        assert_ne!(waits[0].0, waits[0].1);
    }
    for source in [
        "async fn main(): null! { time.sleep(0)? return ok(null) }",
        "import time from \"./time.ku\"\nasync fn main(): null! { time.sleep(0)? return ok(null) }",
        "import clock from \"std.time\"\nasync fn main(): null! { clock.sleep(0)? return ok(null) }",
        "import time from \"std.time\"\nasync fn main(): null! { time = 1 time.sleep(0)? return ok(null) }",
        "import time from \"std.time\"\nasync fn Child(time: int): null! { time.sleep(0)? return ok(null) } async fn main(): null! { return ok(null) }",
        "import time from \"std.time\"\nasync fn main(): null! { time.now() return ok(null) }",
        "import time from \"std.time\"\nasync fn main(): null! { time.sleep(true)? return ok(null) }",
        "import time from \"std.time\"\nasync fn main(): null! { time.sleep({ milliseconds: 1 })? return ok(null) }",
    ] {
        let ast=Parser::new(Lexer::new(source).lex().unwrap()).parse_program().unwrap();
        assert!(task_lower::lower_program(&ast).is_err(),"must preserve rejected module/type boundary: {source}");
    }
}

#[test]
fn native_task_read_dir_owns_large_sorted_outputs_and_preserves_paths() {
    let source = r#"
import fs from "std.fs"
async fn Copy(values: [str]): [str]! { return ok(values) }
async fn List(path: str): [str]! {
    first = fs.read_dir(path)?
    again = fs.read_dir(path)?
    println(again.len())
    return ok(first)
}
async fn main(): null! {
    listed = (await List("listing"))?
    cloned = listed.clone()
    moved = (await Copy(cloned))?
    println(moved.len())
    println(listed.try_get(0)?)
    println(moved.try_get(81)?)
    empty = fs.read_dir("empty")?
    println(empty.len())
    try { fs.read_dir("missing")? } catch(err) { println(err.code) }
    return ok(null)
}
"#;
    let mut ast = Parser::new(Lexer::new(source).lex().unwrap())
        .parse_program()
        .unwrap();
    ast.items
        .retain(|item| !matches!(item, ku::ast::Item::Import(_)));
    ast.items.insert(
        0,
        ku::ast::Item::Module(ku::ast::ModuleDecl {
            name: "std:fs".into(),
            span: Default::default(),
        }),
    );
    Checker::new().check(&ast).unwrap();
    let lowered = task_lower::lower_program(&ast).unwrap();
    let generated = c::generate_native_task_c_source(&lowered, &Default::default()).unwrap();
    assert!(generated.contains("ku_task_host_blocking_read_dir("));
    let directory = TempDir::new("native-task-directory");
    fs::create_dir(directory.path().join("listing")).unwrap();
    fs::create_dir(directory.path().join("empty")).unwrap();
    for index in 0..80 {
        fs::write(directory.path().join(format!("listing/f{index:03}")), b"").unwrap();
    }
    fs::write(directory.path().join("listing/中文😀"), b"").unwrap();
    fs::write(directory.path().join("listing/é"), b"").unwrap();
    let path = directory.path().join("directory.c");
    fs::write(&path, generated).unwrap();
    let Some(executable) = compile_harness(directory.path(), &path, "directory") else {
        assert!(
            std::env::var_os("GITHUB_ACTIONS").is_none(),
            "CI requires directory Task execution"
        );
        return;
    };
    fs::remove_file(path).unwrap();
    let output = run_bounded(
        Command::new(executable).current_dir(directory.path()),
        RUN_TIMEOUT,
        RUN_LIMITS,
    )
    .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stderr.is_empty());
    let text = String::from_utf8(output.stdout)
        .unwrap()
        .replace('\r', "")
        .replace('\\', "/");
    let lines: Vec<_> = text.lines().collect();
    assert_eq!(lines.len(), 6, "{text}");
    assert_eq!(lines[0], "82");
    assert_eq!(lines[1], "82");
    assert!(lines[2].ends_with("listing/f000"), "{text}");
    assert!(lines[3].ends_with("listing/中文😀"), "{text}");
    assert_eq!(lines[4], "0");
    assert_eq!(lines[5], "read_dir_failed");
}

fn native_task_transport_source(source: &str) -> String {
    // Ku accepts embedded NUL bytes; its string escape set has no \0 spelling.
    let source = source.replace("\\0", "\0");
    let mut ast = Parser::new(Lexer::new(&source).lex().expect("transport source lexes"))
        .parse_program()
        .expect("transport source parses");
    ast.items
        .retain(|item| !matches!(item, ku::ast::Item::Import(_)));
    for name in ["std:net", "std:bytes", "std:time"] {
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
    let tasks = task_lower::lower_program(&ast).expect("transport source lowers");
    c::generate_native_task_c_source(&tasks, &Default::default()).expect("transport C generation")
}

#[test]
fn native_task_tcp_wait_yields_one_executor_and_preserves_partial_binary_data() {
    use std::io::{Read, Write};
    use std::net::{Shutdown, TcpListener};
    use std::time::{Duration, Instant};
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let source = format!(
        r#"
import net from "std.net"
import bytes from "std.bytes"
import time from "std.time"
async fn Tick(): null! {{ time.sleep(10)? println("tick") return ok(null) }}
async fn Exchange(port: int): null! {{
    client=net.client({{host:"127.0.0.1",port:port,tls:false,read_timeout_ms:2000}})?
    sent=bytes.from_str("A\0B")?
    client.write(sent)?
    println(sent.len())
    total=0
    sum=0
    while (total<3) {{
        part=client.read(3)?
        for index in part.len() {{ sum=sum+part.get(index)? }}
        total=total+part.len()
    }}
    println(total)
    println(sum)
    try {{ client.read(1)? }} catch(err) {{ println(err.code) }}
    client.close()
    return ok(null)
}}
async fn main(): null! {{
    exchange=Exchange({port})
    timer=Tick()
    timer_done=(await timer)?
    exchange_done=(await exchange)?
    return ok(null)
}}
"#
    );
    let mut generated = native_task_transport_source(&source);
    assert!(!generated.contains("#define KU_FEATURE_NATIVE_TLS 1"));
    assert!(generated.contains("#define KU_TASK_BLOCKING_NETWORK 1"));
    assert!(generated.contains("#define KU_TASK_BLOCKING_FILES 0"));
    // One executor makes a blocking call on that executor observable: Tick
    // must run before the delayed peer delivers the remaining response bytes.
    let start = generated
        .find("static size_t ku_task_root_worker_count(void) {")
        .unwrap();
    let end = start
        + generated[start..]
            .find("static uint64_t ku_task_root_deadline(")
            .unwrap();
    generated.replace_range(
        start..end,
        "static size_t ku_task_root_worker_count(void) { return 1u; }\n",
    );
    let directory = TempDir::new("native-task-tcp-yield");
    let path = directory.path().join("transport.c");
    fs::write(&path, generated).unwrap();
    let Some(executable) = compile_harness(directory.path(), &path, "transport") else {
        return;
    };
    listener.set_nonblocking(true).unwrap();
    let peer = std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut stream = loop {
            match listener.accept() {
                Ok((stream, _)) => break stream,
                Err(error)
                    if error.kind() == std::io::ErrorKind::WouldBlock
                        && Instant::now() < deadline =>
                {
                    std::thread::sleep(Duration::from_millis(2))
                }
                Err(error) => panic!("bounded Task transport accept: {error}"),
            }
        };
        // Accepted sockets may inherit the listener's nonblocking mode. This peer
        // uses bounded blocking I/O; a read timeout does not change that mode.
        stream.set_nonblocking(false).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        stream
            .set_write_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        let mut input = [0u8; 3];
        stream.read_exact(&mut input).unwrap();
        assert_eq!(&input, b"A\0B");
        std::thread::sleep(Duration::from_millis(100));
        stream.write_all(b"A").unwrap();
        std::thread::sleep(Duration::from_millis(30));
        stream.write_all(b"\0B").unwrap();
        stream.shutdown(Shutdown::Write).unwrap();
    });
    let output = run_bounded(
        Command::new(executable)
            .current_dir(directory.path())
            // The reactor scratch must live on the heap even with the minimum
            // supported thread stack; no capacity-sized poll arrays on stack.
            .env("KU_THREAD_STACK_BYTES", "262144"),
        RUN_TIMEOUT,
        RUN_LIMITS,
    )
    .unwrap();
    peer.join().expect("Task transport peer exits");
    assert!(
        output.status.success(),
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout).replace('\r', "");
    let lines: Vec<_> = stdout.lines().collect();
    assert_eq!(
        &lines[lines.len() - 3..],
        &["3", "131", "end_of_stream"],
        "{stdout:?}"
    );
    assert!(lines[..lines.len() - 3].contains(&"tick"));
    assert!(lines[..lines.len() - 3].contains(&"3"));
    assert!(output.stderr.is_empty());
}

#[test]
fn native_task_local_bytes_use_existing_errors_and_owned_results() {
    let source = r#"
import net from "std.net"
import bytes from "std.bytes"
async fn Numbers(): null! {
    input=bytes.from_array([65,0,255])?
    cloned=input.clone()
    println(input.len())
    println(cloned.get(1)?)
    println(cloned.get(2)?)
    try { cloned.to_str()? } catch(err) { println(err.code) }
    try { cloned.get(3)? } catch(err) { println(err.code) }
    return ok(null)
}
async fn More(): null! {
    try { bytes.from_array([256])? } catch(err) { println(err.code) }
    text=bytes.from_str("中文😀\0尾")?
    println(text.to_str()?)
    try { net.client({host:"127.0.0.1",port:0})? } catch(err) { println(err.code) }
    return ok(null)
}
async fn main(): null! {
    numbers_done=(await Numbers())?
    more_done=(await More())?
    return ok(null)
}
"#;
    let generated = native_task_transport_source(source);
    let directory = TempDir::new("native-task-local-bytes");
    let path = directory.path().join("bytes.c");
    fs::write(&path, generated).unwrap();
    let Some(executable) = compile_harness(directory.path(), &path, "bytes") else {
        return;
    };
    let output = run_bounded(
        Command::new(executable).current_dir(directory.path()),
        RUN_TIMEOUT,
        RUN_LIMITS,
    )
    .unwrap();
    assert!(
        output.status.success(),
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).replace('\r', ""),
        "3\n0\n255\ninvalid_utf8\nindex_out_of_bounds\ninvalid_byte\n中文😀\0尾\ninvalid_config\n"
    );
    assert!(output.stderr.is_empty());
}

#[test]
fn native_task_tcp_late_readiness_cannot_mutate_a_reused_job_generation() {
    let mut generated = native_task_transport_source(
        r#"
import net from "std.net"
async fn main(): null! {
    client=net.client({host:"127.0.0.1",port:9,tls:false})?
    client.close()
    return ok(null)
}
"#,
    )
    .replacen(
        "int main(void) {",
        "static int ku_unused_source_main(void) {",
        1,
    );
    let lock = "static int ku_task_blocking_lock(KuTaskBlockingPoolV1* pool) {";
    assert_eq!(generated.matches(lock).count(), 1);
    generated = generated.replacen(
        lock,
        r#"
static KU_THREAD_LOCAL size_t fixture_locks,fixture_depth,fixture_fail_after,fixture_net_scanned;
static int fixture_mode,fixture_bad_dispatch;
static size_t fixture_other_index,fixture_added_index;
static int fixture_original_lock(KuTaskBlockingPoolV1* pool);
static int ku_task_blocking_lock(KuTaskBlockingPoolV1* pool) {
  if(fixture_fail_after && !--fixture_fail_after)return -1;
  int status=fixture_original_lock(pool);
  if(!status) {fixture_locks++;fixture_depth++;} return status;
}
static int fixture_original_lock(KuTaskBlockingPoolV1* pool) {"#,
        1,
    );
    let unlock = "static void ku_task_blocking_unlock(KuTaskBlockingPoolV1* pool) {";
    assert_eq!(generated.matches(unlock).count(), 1);
    generated = generated.replacen(
        unlock,
        r#"
static void fixture_original_unlock(KuTaskBlockingPoolV1* pool);
static void ku_task_blocking_unlock(KuTaskBlockingPoolV1* pool) {
  if(!fixture_depth)abort();fixture_depth--;fixture_original_unlock(pool);
}
static void fixture_original_unlock(KuTaskBlockingPoolV1* pool) {"#,
        1,
    );
    let dispatch = "static uint32_t ku_task_net_dispatch(KuTaskBlockingPoolV1* pool,size_t owner,size_t index,uint64_t generation,uint32_t events) {";
    assert_eq!(generated.matches(dispatch).count(), 1);
    // Only control the unlocked interleaving. The original interests routine,
    // pool lock, stale-generation dispatch and shutdown all execute unchanged.
    generated = generated.replacen(dispatch, r#"
static KuNetClient fixture_clients[2];
static uint32_t fixture_original_dispatch(KuTaskBlockingPoolV1*,size_t,size_t,uint64_t,uint32_t);
static uint32_t ku_task_net_dispatch(KuTaskBlockingPoolV1* pool,size_t owner,size_t index,uint64_t generation,uint32_t events) {
  if(!fixture_mode)return fixture_original_dispatch(pool,owner,index,generation,events);
  if(fixture_depth) {fixture_bad_dispatch=1;return KU_TASK_DRIVER_INTERNAL;}
  if(fixture_mode==4)return KU_TASK_DRIVER_INTERNAL;
  if(fixture_mode==5) {fixture_fail_after=1u;return KU_TASK_DRIVER_OK;}
  if(ku_task_blocking_lock(pool))return KU_TASK_DRIVER_INTERNAL;
  KuTaskBlockingJobV1* job=&pool->jobs[index];
  if(fixture_mode==1) {
    uint64_t deadline=job->net_deadline;
    if(ku_task_blocking_net_state_locked(pool,job,KU_TASK_BLOCKING_DONE))abort();
    memset(job,0,sizeof(*job));job->generation=generation+1u;job->net_owner=owner+1u;
    job->kind=6u;job->net_requested=17;job->net_client=&fixture_clients[1];job->net_deadline=deadline+100u;
    if(ku_task_blocking_net_state_locked(pool,job,KU_TASK_BLOCKING_NET_WAIT))abort();
  } else if(fixture_mode==2) {
    if(ku_task_blocking_net_state_locked(pool,job,KU_TASK_BLOCKING_DONE))abort();
  } else if(fixture_mode==3) {
    if(ku_task_blocking_net_state_locked(pool,job,KU_TASK_BLOCKING_NET_STEP))abort();
    job->net_owner=owner+2u;
    if(ku_task_blocking_net_state_locked(pool,job,KU_TASK_BLOCKING_NET_WAIT))abort();
  } else if(fixture_mode>=6 && fixture_mode<=9) {
    if(ku_task_blocking_net_state_locked(pool,job,KU_TASK_BLOCKING_DONE))abort();
    KuTaskBlockingJobV1* other=&pool->jobs[fixture_mode==9 ? fixture_added_index:fixture_other_index];
    if(fixture_mode==6) {
      uint64_t next_generation=other->generation+1u,deadline=other->net_deadline;
      if(ku_task_blocking_net_state_locked(pool,other,KU_TASK_BLOCKING_DONE))abort();
      memset(other,0,sizeof(*other));other->generation=next_generation;other->net_owner=owner+1u;
      other->kind=6u;other->net_client=&fixture_clients[1];other->net_deadline=deadline;
      if(ku_task_blocking_net_state_locked(pool,other,KU_TASK_BLOCKING_NET_WAIT))abort();
    } else if(fixture_mode==7) {
      if(ku_task_blocking_net_state_locked(pool,other,KU_TASK_BLOCKING_DONE))abort();
    } else if(fixture_mode==8) {
      if(ku_task_blocking_net_state_locked(pool,other,KU_TASK_BLOCKING_NET_STEP))abort();
      other->net_owner=owner+2u;
      if(ku_task_blocking_net_state_locked(pool,other,KU_TASK_BLOCKING_NET_WAIT))abort();
    } else {
      memset(other,0,sizeof(*other));other->generation=100u;other->net_owner=owner+1u;
      other->kind=6u;other->net_client=&fixture_clients[1];other->net_deadline=job->net_deadline;
      if(ku_task_blocking_net_state_locked(pool,other,KU_TASK_BLOCKING_NET_WAIT))abort();
    }
  }
  ku_task_blocking_unlock(pool);return KU_TASK_DRIVER_OK;
}
static uint32_t fixture_original_dispatch(KuTaskBlockingPoolV1* pool,size_t owner,size_t index,uint64_t generation,uint32_t events) {"#, 1);
    // include_str! preserves a Windows checkout's CRLF in the reactor C file.
    // Normalize this fixture's injection text, without changing emitted logic.
    generated = generated.replace("\r\n", "\n");
    let scan = "    KuTaskBlockingJobV1* snapshot_job=&pool->jobs[member-1u];";
    assert_eq!(generated.matches(scan).count(), 1);
    generated = generated.replacen(scan, &format!("    fixture_net_scanned++;\n{scan}"), 1);
    let source = format!(
        "{generated}\n{}",
        r#"
#define CHECK(c) do {if(!(c)){fprintf(stderr,"late TCP readiness line %d: %s\n",__LINE__,#c);return 1;}}while(0)
static int fixture_prepare_wait(KuTaskBlockingPoolV1* pool,size_t index,size_t owner,uint64_t generation,int cancelled,uint64_t deadline) {
  KuTaskBlockingJobV1* job=&pool->jobs[index];
  if(job->state==KU_TASK_BLOCKING_NET_WAIT)
    CHECK(ku_task_blocking_net_state_locked(pool,job,KU_TASK_BLOCKING_DONE)==KU_TASK_DRIVER_OK);
  memset(job,0,sizeof(*job));job->generation=generation;job->net_owner=owner+1u;
  job->kind=6u;job->net_requested=17;job->cancelled=cancelled;
  job->net_client=&fixture_clients[0];job->net_deadline=deadline;
  CHECK(ku_task_blocking_net_state_locked(pool,job,KU_TASK_BLOCKING_NET_WAIT)==KU_TASK_DRIVER_OK);
  return 0;
}
static int fixture_members(KuTaskBlockingPoolV1* pool,size_t owner,const size_t* expected,size_t count) {
  KuTaskNetPollV1 items[8]={{0}};uint64_t deadline=UINT64_MAX;
  CHECK(ku_task_net_interests(pool,owner,items,8u,&deadline)==count);
  for(size_t i=0;i<count;i++) {
    size_t seen=0;
    for(size_t j=0;j<count;j++)if(items[j].slot==expected[i])seen++;
    CHECK(seen==1u && items[i].generation==pool->jobs[items[i].slot].generation);
  }
  return 0;
}
int main(void) {
  /* The fixed 1024-job table alone exceeds a Windows default 1MiB main
     stack. Production roots use static/heap storage; mirror that here. */
  KuTaskBlockingPoolV1* pool=(KuTaskBlockingPoolV1*)calloc(1,sizeof(*pool));
  CHECK(pool);uint64_t deadline=ku_task_driver_now_ms()+1000u;
  CHECK(ku_task_blocking_init(pool,KU_TASK_DRIVER_MAX_SLOTS,1u,deadline)==KU_TASK_DRIVER_OK);
  KuTaskNetPollV1 interests[2]={{0}};uint64_t next=UINT64_MAX;
  size_t before=fixture_locks;
  CHECK(ku_task_net_interests(NULL,0u,interests,2u,&next)==0u && fixture_locks==before);
  CHECK(ku_task_net_interests(pool,0u,interests,2u,&next)==0u);
  CHECK(fixture_locks-before==1u && !fixture_depth && next==UINT64_MAX);
  CHECK(!fixture_net_scanned && !pool->net_waiting[0]);
  fixture_fail_after=1u;
  CHECK(ku_task_net_interests(pool,0u,interests,2u,&next)==SIZE_MAX && !fixture_depth);
  CHECK(!ku_net_socket_startup());
  for(size_t i=0;i<2u;i++) {
    fixture_clients[i].socket_value=socket(AF_INET,SOCK_STREAM,IPPROTO_TCP);
    CHECK(fixture_clients[i].socket_value!=KU_NET_INVALID_SOCKET);
  }
  CHECK(!ku_task_blocking_lock(pool));
  /* A sole member in the last physical slot must cost one visited member,
     not capacity visits. No descriptor or slot address is renumbered. */
  size_t index=pool->capacity-1u;
  KuTaskBlockingJobV1* job=&pool->jobs[index];job->generation=2u;
  job->net_owner=1u;job->kind=6u;job->net_requested=17;job->net_deadline=deadline;
  job->net_client=&fixture_clients[0];
  CHECK(ku_task_blocking_net_state_locked(pool,job,KU_TASK_BLOCKING_NET_WAIT)==KU_TASK_DRIVER_OK);
  CHECK(pool->net_waiting[0]==1u);
  ku_task_blocking_unlock(pool);
  CHECK(ku_task_net_dispatch(pool,0u,index,1u,KU_TASK_NET_READ|KU_TASK_NET_ERROR)==KU_TASK_DRIVER_OK);
  CHECK(ku_task_net_dispatch(pool,1u,index,2u,KU_TASK_NET_READ)==KU_TASK_DRIVER_OK);
  CHECK(!ku_task_blocking_lock(pool));
  CHECK(job->generation==2u && job->state==KU_TASK_BLOCKING_NET_WAIT && job->net_requested==17 && !job->cancelled && !pool->fault);
  ku_task_blocking_unlock(pool);
  before=fixture_locks;next=UINT64_MAX;
  CHECK(ku_task_net_interests(pool,0u,interests,2u,&next)==1u);
  CHECK(fixture_locks-before==1u && !fixture_depth && next==deadline);
  CHECK(fixture_net_scanned==1u);
  CHECK(interests[0].generation==2u && interests[0].slot==index
    && interests[0].socket_value==(uintptr_t)fixture_clients[0].socket_value
    && interests[0].events==KU_TASK_NET_READ);
  before=fixture_locks;next=UINT64_MAX;
  CHECK(ku_task_net_interests(pool,1u,interests,2u,&next)==0u);
  CHECK(fixture_locks-before==1u && !fixture_depth && next==UINT64_MAX);
  CHECK(fixture_net_scanned==1u);
  CHECK(ku_task_net_interests(pool,0u,NULL,0u,&next)==SIZE_MAX && !fixture_depth);
  CHECK(!ku_task_blocking_lock(pool));CHECK(pool->fault==KU_TASK_DRIVER_LIMIT);pool->fault=0;
  ku_task_blocking_unlock(pool);
  for(int mode=1;mode<=5;mode++) {
    CHECK(!ku_task_blocking_lock(pool));
    CHECK(ku_task_blocking_net_state_locked(pool,job,KU_TASK_BLOCKING_DONE)==KU_TASK_DRIVER_OK);
    job->generation=10u+(uint64_t)mode;
    job->net_owner=1u;job->kind=6u;job->cancelled=1;
    job->net_client=&fixture_clients[0];job->net_deadline=deadline;
    CHECK(ku_task_blocking_net_state_locked(pool,job,KU_TASK_BLOCKING_NET_WAIT)==KU_TASK_DRIVER_OK);
    CHECK(pool->net_waiting[0]==1u && !pool->net_waiting[1]);
    ku_task_blocking_unlock(pool);
    fixture_mode=mode;next=UINT64_MAX;before=fixture_locks;
    size_t count=ku_task_net_interests(pool,0u,interests,2u,&next);
    CHECK(!fixture_depth && !fixture_bad_dispatch);
    if(mode==1) {
      /* A new generation is deliberately deferred to the next owner wake,
         never mistaken for the retired snapshot member. */
      CHECK(count==0u && next==UINT64_MAX);
    } else if(mode==2 || mode==3)CHECK(count==0u && next==UINT64_MAX);
    else CHECK(count==SIZE_MAX);
    CHECK(pool->net_waiting[0]==(mode==2 || mode==3 ? 0u:1u));
    CHECK(pool->net_waiting[1]==(mode==3 ? 1u:0u));
    CHECK(fixture_locks-before==(mode<=3 ? 3u : 1u));
    fixture_mode=0;
    if(mode==1) {
      next=UINT64_MAX;
      CHECK(ku_task_net_interests(pool,0u,interests,2u,&next)==1u);
      CHECK(interests[0].generation==12u && next==deadline+100u);
      CHECK(interests[0].socket_value==(uintptr_t)fixture_clients[1].socket_value);
    }
  }
  CHECK(!ku_task_blocking_lock(pool));
  CHECK(ku_task_blocking_net_state_locked(pool,job,KU_TASK_BLOCKING_NET_STEP)==KU_TASK_DRIVER_OK);
  CHECK(!pool->net_waiting[0]);
  CHECK(ku_task_blocking_net_state_locked(pool,job,KU_TASK_BLOCKING_NET_WAIT)==KU_TASK_DRIVER_OK);
  CHECK(pool->net_waiting[0]==1u);
  CHECK(ku_task_blocking_net_state_locked(pool,job,KU_TASK_BLOCKING_DONE)==KU_TASK_DRIVER_OK);
  memset(job,0,sizeof(*job));
  /* Head, middle and tail removal plus reinsertion preserve every remaining
     member exactly once. A repeated WAIT transition never duplicates a link. */
  const size_t nodes[]={index,5u,2u,0u};
  for(size_t i=0;i<4u;i++)CHECK(!fixture_prepare_wait(pool,nodes[i],0u,40u+i,0,deadline));
  CHECK(pool->net_head_plus_one[0]==1u && pool->net_waiting[0]==4u);
  CHECK(ku_task_blocking_net_state_locked(pool,&pool->jobs[2],KU_TASK_BLOCKING_NET_WAIT)==KU_TASK_DRIVER_OK);
  CHECK(pool->net_waiting[0]==4u);
  ku_task_blocking_unlock(pool);
  before=fixture_net_scanned;CHECK(!fixture_members(pool,0u,nodes,4u));CHECK(fixture_net_scanned-before==4u);
  next=UINT64_MAX;
  CHECK(ku_task_net_interests(pool,0u,interests,2u,&next)==SIZE_MAX && !fixture_depth);
  CHECK(!ku_task_blocking_lock(pool));CHECK(pool->fault==KU_TASK_DRIVER_LIMIT);pool->fault=0;
  /* All affected links are checked before the first removal write. */
  uint32_t previous=pool->jobs[5].net_previous_plus_one;
  uint32_t following=pool->jobs[5].net_next_plus_one;
  pool->jobs[index].net_previous_plus_one=1u;
  CHECK(ku_task_blocking_net_state_locked(pool,&pool->jobs[5],KU_TASK_BLOCKING_DONE)==KU_TASK_DRIVER_INTERNAL);
  CHECK(pool->jobs[5].state==KU_TASK_BLOCKING_NET_WAIT && pool->net_waiting[0]==4u);
  CHECK(pool->jobs[5].net_previous_plus_one==previous && pool->jobs[5].net_next_plus_one==following);
  CHECK(pool->jobs[2].net_next_plus_one==6u && pool->net_head_plus_one[0]==1u);
  pool->jobs[index].net_previous_plus_one=6u;pool->fault=0;
  CHECK(ku_task_blocking_net_state_locked(pool,&pool->jobs[2],KU_TASK_BLOCKING_DONE)==KU_TASK_DRIVER_OK);
  CHECK(ku_task_blocking_net_state_locked(pool,&pool->jobs[index],KU_TASK_BLOCKING_DONE)==KU_TASK_DRIVER_OK);
  CHECK(ku_task_blocking_net_state_locked(pool,&pool->jobs[0],KU_TASK_BLOCKING_DONE)==KU_TASK_DRIVER_OK);
  ku_task_blocking_unlock(pool);
  {const size_t expected[]={5u};CHECK(!fixture_members(pool,0u,expected,1u));}
  CHECK(!ku_task_blocking_lock(pool));
  CHECK(ku_task_blocking_net_state_locked(pool,&pool->jobs[2],KU_TASK_BLOCKING_NET_WAIT)==KU_TASK_DRIVER_OK);
  ku_task_blocking_unlock(pool);
  {const size_t expected[]={2u,5u};CHECK(!fixture_members(pool,0u,expected,2u));}
  CHECK(!ku_task_blocking_lock(pool));
  CHECK(ku_task_blocking_net_state_locked(pool,&pool->jobs[2],KU_TASK_BLOCKING_DONE)==KU_TASK_DRIVER_OK);
  CHECK(ku_task_blocking_net_state_locked(pool,&pool->jobs[5],KU_TASK_BLOCKING_DONE)==KU_TASK_DRIVER_OK);
  for(size_t i=0;i<4u;i++)memset(&pool->jobs[nodes[i]],0,sizeof(*job));
  ku_task_blocking_unlock(pool);
  /* Another captured slot can retire, be reused or change owner while the
     current dispatch is unlocked. An arrival was not in this pass's snapshot. */
  fixture_other_index=0u;fixture_added_index=2u;
  for(int mode=6;mode<=9;mode++) {
    CHECK(!ku_task_blocking_lock(pool));
    if(mode!=9)CHECK(!fixture_prepare_wait(pool,fixture_other_index,0u,60u+(uint64_t)mode,0,deadline));
    CHECK(!fixture_prepare_wait(pool,index,0u,80u+(uint64_t)mode,1,deadline));
    ku_task_blocking_unlock(pool);
    fixture_mode=mode;next=UINT64_MAX;before=fixture_net_scanned;
    CHECK(ku_task_net_interests(pool,0u,interests,2u,&next)==0u && !fixture_depth && !fixture_bad_dispatch);
    CHECK(fixture_net_scanned-before==(mode==9 ? 1u:2u));
    fixture_mode=0;
    if(mode==6) {const size_t expected[]={0u};CHECK(!fixture_members(pool,0u,expected,1u));}
    if(mode==7)CHECK(!fixture_members(pool,0u,NULL,0u));
    if(mode==8) {
      const size_t expected[]={0u};CHECK(!fixture_members(pool,0u,NULL,0u));
      CHECK(!fixture_members(pool,1u,expected,1u));
    }
    if(mode==9) {const size_t expected[]={2u};CHECK(!fixture_members(pool,0u,expected,1u));}
    CHECK(!ku_task_blocking_lock(pool));
    const size_t touched[]={index,fixture_other_index,fixture_added_index};
    for(size_t i=0;i<3u;i++) {
      KuTaskBlockingJobV1* touched_job=&pool->jobs[touched[i]];
      if(touched_job->state==KU_TASK_BLOCKING_NET_WAIT)
        CHECK(ku_task_blocking_net_state_locked(pool,touched_job,KU_TASK_BLOCKING_DONE)==KU_TASK_DRIVER_OK);
      memset(touched_job,0,sizeof(*touched_job));
    }
    ku_task_blocking_unlock(pool);
  }
  CHECK(!ku_task_blocking_lock(pool));
  for(size_t i=0;i<KU_TASK_DRIVER_MAX_WORKERS;i++)CHECK(!pool->net_waiting[i] && !pool->net_head_plus_one[i]);
  ku_task_blocking_unlock(pool);
  before=fixture_net_scanned;CHECK(!fixture_members(pool,0u,NULL,0u));CHECK(fixture_net_scanned==before);
  for(size_t i=0;i<2u;i++)ku_net_socket_close(fixture_clients[i].socket_value);
  CHECK(ku_task_blocking_finish(pool,deadline)==KU_TASK_DRIVER_OK);
  CHECK(pool->initialized==KU_TASK_DRIVER_STORAGE_ZERO && !pool->resources
      && !pool->resident && !pool->queued && !pool->running
      && pool->workers_joined==pool->workers_created);
  free(pool);
  puts("late-tcp-generation-ok");return 0;
}
"#
    );
    let directory = TempDir::new("tcp-late-generation");
    let path = directory.path().join("generation.c");
    fs::write(&path, source).unwrap();
    let Some(executable) = compile_harness(directory.path(), &path, "generation") else {
        return;
    };
    let output = run_bounded(&mut Command::new(executable), RUN_TIMEOUT, RUN_LIMITS).unwrap();
    assert!(
        output.status.success(),
        "late TCP readiness process failed: {} ({:?}, code {:?})\nstdout:\n{}\nstderr:\n{}",
        output.status,
        output.status,
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let expected: &[u8] = if cfg!(windows) {
        b"late-tcp-generation-ok\r\n"
    } else {
        b"late-tcp-generation-ok\n"
    };
    assert!(output.stdout.ends_with(expected));
}
