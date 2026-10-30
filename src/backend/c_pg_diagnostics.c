/* Private bounded acquisition-failure witnesses. Emitted before HTTP so the
 * authenticated management channel can export them without runtime I/O.
 * Failure publication never waits for this ring: concurrent export/capture
 * loses a sample and increments an explicit counter instead of blocking PG.
 * No SQL, parameters, connection strings, pointers or server text are stored. */
#define KU_PG_DIAGNOSTICS 1
#include <stdarg.h>
static void ku_pg_diagnostics_sync_acquire_locked(KuPgClient*,unsigned long long,unsigned long long,unsigned long long,unsigned long long,int,unsigned long long,PGconn*);
#if defined(_WIN32)
#include <windows.h>
typedef volatile LONG64 KuPgDiagAtomic;
static uint64_t ku_pg_diag_load(KuPgDiagAtomic* value) { return (uint64_t)InterlockedCompareExchange64(value,0,0); }
static void ku_pg_diag_store(KuPgDiagAtomic* value,uint64_t next) { InterlockedExchange64(value,(LONG64)next); }
static uint64_t ku_pg_diag_increment(KuPgDiagAtomic* value) { return (uint64_t)InterlockedIncrement64(value); }
static volatile LONG ku_pg_diag_guard;
static int ku_pg_diag_try_lock(void) { return InterlockedCompareExchange(&ku_pg_diag_guard,1,0)==0; }
static void ku_pg_diag_unlock(void) { InterlockedExchange(&ku_pg_diag_guard,0); }
#else
#include <stdatomic.h>
typedef _Atomic uint64_t KuPgDiagAtomic;
static uint64_t ku_pg_diag_load(KuPgDiagAtomic* value) { return atomic_load_explicit(value,memory_order_relaxed); }
static void ku_pg_diag_store(KuPgDiagAtomic* value,uint64_t next) { atomic_store_explicit(value,next,memory_order_relaxed); }
static uint64_t ku_pg_diag_increment(KuPgDiagAtomic* value) { return atomic_fetch_add_explicit(value,1,memory_order_relaxed)+1u; }
static atomic_flag ku_pg_diag_guard=ATOMIC_FLAG_INIT;
static int ku_pg_diag_try_lock(void) { return !atomic_flag_test_and_set_explicit(&ku_pg_diag_guard,memory_order_acquire); }
static void ku_pg_diag_unlock(void) { atomic_flag_clear_explicit(&ku_pg_diag_guard,memory_order_release); }
#endif
typedef struct KuPgDiagnosticLease {
  /* Identity fields are set before operation publication and then immutable.
   * Lease/notification fields are protected by the owning PG pool mutex. */
  uint64_t task_slot,task_generation,http_task_slot,http_task_generation;
  uint64_t owner,job_generation,parent_depth,parent_chain_missing;
  uint64_t acquired_ms,published,consumed;
  KuPgDiagAtomic phase,last_progress_ms,progress_events;
  /* Independent atomic observations; zero means that stage was not observed. */
  KuPgDiagAtomic worker_enqueue_ms,worker_start_ms,worker_finish_ms;
} KuPgDiagnosticLease;
typedef struct KuPgDiagnosticMember {
  uint64_t index,connection,in_use,generation,task_slot,task_generation;
  uint64_t http_task_slot,http_task_generation,owner,phase,last_progress_ms;
  uint64_t acquired_ms,published,consumed,notified,in_flight,progress_events;
  uint64_t job_generation,worker_enqueue_ms,worker_start_ms,worker_finish_ms;
} KuPgDiagnosticMember;
typedef struct KuPgDiagnosticSnapshot {
  uint64_t sequence,observed_ms,started_ms,last_progress_ms,deadline_ms;
  uint64_t acquire_deadline_ms,connect_deadline_ms,remaining_ms,phase;
  uint64_t task_slot,task_generation,http_task_slot,http_task_generation,owner,job_generation;
  uint64_t parent_depth,parent_chain_missing,task_phase,task_phase_missing,sync_identity_missing;
  uint64_t worker_enqueue_ms,worker_start_ms,worker_finish_ms;
  uint64_t interest,events,steps,progress_events,published,consumed,lease_slot,lease_generation,acquired_ms;
  uint64_t pool_size,active,leased,waiters,async_operations,idle,empty,connecting,reset,reset_unknown,owner_missing;
  uint64_t reconnect_not_before_ms,connect_failures,closing,sent,connect_ready,libpq_status,libpq_status_missing;
  uint64_t queue_head_task_slot,queue_head_task_generation,queue_head_notified;
  uint64_t slot_samples,waiter_samples,slots_truncated,waiters_truncated,lease_generation_saturated;
  KuPgDiagnosticMember slots[8],queue[8];
} KuPgDiagnosticSnapshot;
static struct {
  KuPgDiagAtomic failures,contention_drops,export_contention,serialization_truncations;
  uint64_t count,changes,overwritten,change_capacity_drops;
  KuPgDiagnosticSnapshot samples[8];
} ku_pg_diagnostics;
/* Eight fixed records: first failure, up to six state-change witnesses, latest.
 * Only existing failure capture calls this helper. No periodic sampling, SQL,
 * allocation or callbacks are added to a successful query's owner turn. */
static uint64_t ku_pg_diag_worker_stage(uint64_t queued,uint64_t started,uint64_t finished) {
  return (queued!=0u)|((uint64_t)(started!=0u)<<1u)|((uint64_t)(finished!=0u)<<2u);
}
static void ku_pg_diag_member_categories(KuPgDiagnosticMember* members,size_t count,uint64_t* phases,uint64_t* workers) {
  *phases=*workers=0;
  for(size_t i=0;i<count;i++) {
    uint64_t phase=members[i].phase;
    *phases|=UINT64_C(1)<<(phase<63u?phase:63u);
    *workers|=UINT64_C(1)<<ku_pg_diag_worker_stage(members[i].worker_enqueue_ms,members[i].worker_start_ms,members[i].worker_finish_ms);
  }
}
static uint64_t ku_pg_diag_pool_class(KuPgDiagnosticSnapshot* s) {
  return !s->active?0u:s->active>=s->pool_size?2u:1u;
}
/* Choose semantic categories, not one category per request identity, queue
 * count or timestamp. Absolute times and full generations remain in records.
 * Member sets summarize observed stages; they are not complete thread traces. */
static int ku_pg_diag_same_state(KuPgDiagnosticSnapshot* a,KuPgDiagnosticSnapshot* b) {
  if(a->phase!=b->phase || ku_pg_diag_pool_class(a)!=ku_pg_diag_pool_class(b)
      || !!a->waiters!=!!b->waiters || !!a->connecting!=!!b->connecting
      || !!a->reset!=!!b->reset || !!a->idle!=!!b->idle || !!a->empty!=!!b->empty
      || !!a->closing!=!!b->closing || !!a->owner_missing!=!!b->owner_missing
      || !!a->connect_ready!=!!b->connect_ready || !!a->sent!=!!b->sent
      || ku_pg_diag_worker_stage(a->worker_enqueue_ms,a->worker_start_ms,a->worker_finish_ms)
          !=ku_pg_diag_worker_stage(b->worker_enqueue_ms,b->worker_start_ms,b->worker_finish_ms))return 0;
  uint64_t ap,aw,bp,bw;
  ku_pg_diag_member_categories(a->slots,a->slot_samples,&ap,&aw);
  ku_pg_diag_member_categories(b->slots,b->slot_samples,&bp,&bw);
  if(ap!=bp || aw!=bw)return 0;
  ku_pg_diag_member_categories(a->queue,a->waiter_samples,&ap,&aw);
  ku_pg_diag_member_categories(b->queue,b->waiter_samples,&bp,&bw);
  return ap==bp && aw==bw;
}
static KuPgDiagnosticSnapshot* ku_pg_diag_latest_locked(void) { return &ku_pg_diagnostics.samples[7]; }
static void ku_pg_diag_retain_locked(KuPgDiagnosticSnapshot* next) {
  KuPgDiagnosticSnapshot* latest=ku_pg_diag_latest_locked();
  if(!ku_pg_diagnostics.count)ku_pg_diagnostics.samples[0]=*next;
  else {
    int represented=ku_pg_diag_same_state(&ku_pg_diagnostics.samples[0],next);
    for(size_t i=1;!represented && i<=ku_pg_diagnostics.changes;i++)represented=ku_pg_diag_same_state(&ku_pg_diagnostics.samples[i],next);
    if(!represented) {
      if(ku_pg_diagnostics.changes<6u)ku_pg_diagnostics.samples[++ku_pg_diagnostics.changes]=*next;
      else if(ku_pg_diagnostics.change_capacity_drops!=UINT64_MAX)ku_pg_diagnostics.change_capacity_drops++;
    }
    int kept=latest->sequence==ku_pg_diagnostics.samples[0].sequence;
    for(size_t i=1;!kept && i<=ku_pg_diagnostics.changes;i++)kept=latest->sequence==ku_pg_diagnostics.samples[i].sequence;
    if(!kept && ku_pg_diagnostics.overwritten!=UINT64_MAX)ku_pg_diagnostics.overwritten++;
  }
  *latest=*next;
  size_t last=ku_pg_diagnostics.changes;
  ku_pg_diagnostics.count=1u+last+(latest->sequence!=ku_pg_diagnostics.samples[last].sequence);
}
static int ku_pg_diag_append(char* text,size_t capacity,size_t* used,const char* format,...) {
  if(*used>=capacity)return 0;
  va_list args;va_start(args,format);int n=vsnprintf(text+*used,capacity-*used,format,args);va_end(args);
  if(n<0 || (size_t)n>=capacity-*used)return 0;
  *used+=(size_t)n;return 1;
}
static int ku_pg_diag_member_json(char* text,size_t capacity,size_t* used,KuPgDiagnosticMember* m,int comma) {
  return ku_pg_diag_append(text,capacity,used,
      "%s{\"index\":%llu,\"connection\":%llu,\"in_use\":%llu,\"generation\":%llu,\"task_slot\":%llu,\"task_generation\":%llu,\"http_task_slot\":%llu,\"http_task_generation\":%llu,\"owner\":%llu,\"phase\":%llu,\"last_progress_ms\":%llu,\"acquired_ms\":%llu,\"published\":%llu,\"consumed\":%llu,\"notified\":%llu,\"in_flight\":%llu,\"progress_events\":%llu,\"job_generation\":%llu,\"worker_enqueue_ms\":%llu,\"worker_start_ms\":%llu,\"worker_finish_ms\":%llu}",
      comma?",":"",(unsigned long long)m->index,(unsigned long long)m->connection,(unsigned long long)m->in_use,
      (unsigned long long)m->generation,(unsigned long long)m->task_slot,(unsigned long long)m->task_generation,
      (unsigned long long)m->http_task_slot,(unsigned long long)m->http_task_generation,(unsigned long long)m->owner,
      (unsigned long long)m->phase,(unsigned long long)m->last_progress_ms,(unsigned long long)m->acquired_ms,
      (unsigned long long)m->published,(unsigned long long)m->consumed,(unsigned long long)m->notified,(unsigned long long)m->in_flight,(unsigned long long)m->progress_events,
      (unsigned long long)m->job_generation,(unsigned long long)m->worker_enqueue_ms,
      (unsigned long long)m->worker_start_ms,(unsigned long long)m->worker_finish_ms);
}
/* Returns a complete JSON member (without the leading comma), or zero only if
 * even the explicit truncated witness does not fit the caller's buffer. */
static size_t ku_pg_diagnostics_json(char* text,size_t capacity) {
  /* The longest closing trailer including full-width counters and NUL fits
   * in 128 bytes. Do not spend it on a partially formatted sample. */
  const size_t trailer_reserve=128u;
  size_t used=0,sample_capacity=capacity>trailer_reserve?capacity-trailer_reserve:0;
  int locked=ku_pg_diag_try_lock();
  uint64_t total=locked?ku_pg_diagnostics.count:0,exported=0;int truncated=0;
  if(!locked)ku_pg_diag_increment(&ku_pg_diagnostics.export_contention);
  uint64_t failures=ku_pg_diag_load(&ku_pg_diagnostics.failures);
  uint64_t drops=ku_pg_diag_load(&ku_pg_diagnostics.contention_drops);
  int ok=ku_pg_diag_append(text,sample_capacity,&used,
      "\"pg_acquire_failures\":{\"sample_limit\":8,\"member_limit\":8,\"sample_order\":\"first_then_state_changes_then_latest\",\"sampling\":\"acquire_failure_only\",\"worker_timing\":\"independent_atomic_observations_zero_is_missing\",\"counter_wrap\":\"uint64\",\"failures\":%llu,\"contention_drops\":%llu,\"export_busy\":%s,\"export_contention\":%llu,\"serialization_truncations\":%llu,\"overwritten\":%llu,\"change_capacity_drops\":%llu,\"fixed_bytes\":%llu,\"correlation\":\"http_task_slot_and_generation_join_errors_samples\",\"missing\":[\"thread_cpu_and_stack\",\"driver_send_delay\",\"sync_lease_owner\",\"driver_ready_wait_state\",\"request_id_requires_http_join\"],\"samples\":[",
      (unsigned long long)failures,(unsigned long long)drops,locked?"false":"true",
      (unsigned long long)ku_pg_diag_load(&ku_pg_diagnostics.export_contention),
      (unsigned long long)ku_pg_diag_load(&ku_pg_diagnostics.serialization_truncations),
      (unsigned long long)(locked?ku_pg_diagnostics.overwritten:0),
      (unsigned long long)(locked?ku_pg_diagnostics.change_capacity_drops:0),
      (unsigned long long)(sizeof(ku_pg_diagnostics)+sizeof(ku_pg_diag_guard)));
  for(uint64_t i=0;locked && ok && i<total;i++) {
    size_t sample_start=used;
    /* The first failure survives repeated errors and a small response budget.
     * Roll back only an incomplete object; keep complete earlier witnesses. */
    uint64_t at=i<=ku_pg_diagnostics.changes?i:7u;
    KuPgDiagnosticSnapshot* s=&ku_pg_diagnostics.samples[at];
#define KU_PG_DIAG_FIELD(name) do { if(ok)ok=ku_pg_diag_append(text,sample_capacity,&used,"\"" #name "\":%llu,",(unsigned long long)s->name); } while(0)
    ok=ku_pg_diag_append(text,sample_capacity,&used,"%s{\"code\":\"acquire_timeout\",",i?",":"");
    KU_PG_DIAG_FIELD(sequence);KU_PG_DIAG_FIELD(observed_ms);KU_PG_DIAG_FIELD(started_ms);KU_PG_DIAG_FIELD(last_progress_ms);
    KU_PG_DIAG_FIELD(deadline_ms);KU_PG_DIAG_FIELD(acquire_deadline_ms);KU_PG_DIAG_FIELD(connect_deadline_ms);KU_PG_DIAG_FIELD(remaining_ms);KU_PG_DIAG_FIELD(phase);
    KU_PG_DIAG_FIELD(task_slot);KU_PG_DIAG_FIELD(task_generation);KU_PG_DIAG_FIELD(http_task_slot);KU_PG_DIAG_FIELD(http_task_generation);
    KU_PG_DIAG_FIELD(owner);KU_PG_DIAG_FIELD(job_generation);KU_PG_DIAG_FIELD(parent_depth);KU_PG_DIAG_FIELD(parent_chain_missing);
    KU_PG_DIAG_FIELD(worker_enqueue_ms);KU_PG_DIAG_FIELD(worker_start_ms);KU_PG_DIAG_FIELD(worker_finish_ms);
    KU_PG_DIAG_FIELD(task_phase);KU_PG_DIAG_FIELD(task_phase_missing);KU_PG_DIAG_FIELD(sync_identity_missing);KU_PG_DIAG_FIELD(interest);KU_PG_DIAG_FIELD(events);KU_PG_DIAG_FIELD(steps);KU_PG_DIAG_FIELD(progress_events);
    KU_PG_DIAG_FIELD(published);KU_PG_DIAG_FIELD(consumed);KU_PG_DIAG_FIELD(lease_slot);KU_PG_DIAG_FIELD(lease_generation);KU_PG_DIAG_FIELD(acquired_ms);
    KU_PG_DIAG_FIELD(pool_size);KU_PG_DIAG_FIELD(active);KU_PG_DIAG_FIELD(leased);KU_PG_DIAG_FIELD(waiters);KU_PG_DIAG_FIELD(async_operations);KU_PG_DIAG_FIELD(idle);KU_PG_DIAG_FIELD(empty);
    KU_PG_DIAG_FIELD(connecting);KU_PG_DIAG_FIELD(reset);KU_PG_DIAG_FIELD(reset_unknown);KU_PG_DIAG_FIELD(owner_missing);KU_PG_DIAG_FIELD(reconnect_not_before_ms);KU_PG_DIAG_FIELD(connect_failures);
    KU_PG_DIAG_FIELD(closing);KU_PG_DIAG_FIELD(sent);KU_PG_DIAG_FIELD(connect_ready);KU_PG_DIAG_FIELD(libpq_status);KU_PG_DIAG_FIELD(libpq_status_missing);
    KU_PG_DIAG_FIELD(queue_head_task_slot);KU_PG_DIAG_FIELD(queue_head_task_generation);KU_PG_DIAG_FIELD(queue_head_notified);
    KU_PG_DIAG_FIELD(slots_truncated);KU_PG_DIAG_FIELD(waiters_truncated);KU_PG_DIAG_FIELD(lease_generation_saturated);
#undef KU_PG_DIAG_FIELD
    if(ok)ok=ku_pg_diag_append(text,sample_capacity,&used,"\"slots\":[");
    for(uint64_t j=0;ok && j<s->slot_samples;j++)ok=ku_pg_diag_member_json(text,sample_capacity,&used,&s->slots[j],j!=0);
    if(ok)ok=ku_pg_diag_append(text,sample_capacity,&used,"],\"queue\":[");
    for(uint64_t j=0;ok && j<s->waiter_samples;j++)ok=ku_pg_diag_member_json(text,sample_capacity,&used,&s->queue[j],j!=0);
    if(ok)ok=ku_pg_diag_append(text,sample_capacity,&used,"]}");
    if(!ok) {used=sample_start;text[used]=0;truncated=1;ok=1;break;}
    exported++;
  }
  if(locked)ku_pg_diag_unlock();
  if(ok && (!truncated || exported)) {
    if(ku_pg_diag_append(text,capacity,&used,
        "],\"truncated\":%s,\"exported_samples\":%llu,\"omitted_samples\":%llu,\"sample_count_known\":%s}",
        truncated?"true":"false",(unsigned long long)exported,(unsigned long long)(total-exported),locked?"true":"false")) {
      if(truncated)ku_pg_diag_increment(&ku_pg_diagnostics.serialization_truncations);
      return used;
    }
  }
  ku_pg_diag_increment(&ku_pg_diagnostics.serialization_truncations);used=0;
  return ku_pg_diag_append(text,capacity,&used,"\"pg_acquire_failures\":{\"truncated\":true,\"failures\":%llu,\"contention_drops\":%llu,\"exported_samples\":0,\"omitted_samples\":%llu,\"sample_count_known\":%s,\"samples\":[]}",(unsigned long long)failures,(unsigned long long)drops,(unsigned long long)total,locked?"true":"false")?used:0;
}
