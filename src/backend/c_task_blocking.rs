//! Fixed native blocking jobs. A parked source frame retains its actual job
//! until the worker has closed the file and stopped touching all job storage.

use super::output::COutput;
use crate::error::KuResult;

pub(super) fn emit(out: &mut COutput, enabled: bool) -> KuResult<()> {
    out.push_str(TOKEN);
    if enabled {
        out.push_str(RUNTIME);
    }
    out.check()
}

const TOKEN: &str = r#"
typedef struct KuTaskBlockingTokenV1 {
  void* pool;
  size_t slot, charged_bytes;
  uint64_t generation;
} KuTaskBlockingTokenV1;
"#;

const RUNTIME: &str = r#"
/* Private fixed pool: no source Task API, no unbounded job/thread allocation.
 * All callbacks into the driver run without its mutex. Notifications take
 * pool -> driver; the driver never takes pool while holding its mutex.
 * Inputs, including native path conversion temporaries, are prepaid before
 * malloc. The token conservatively retains the last actual input allocation
 * charge until DONE consumption, even after the worker has freed that input.
 * libc FILE buffers and OS thread stacks are outside this Task byte ledger. */
enum { KU_TASK_BLOCKING_OUT_OF_MEMORY=48u };
enum { KU_TASK_BLOCKING_FREE=0u, KU_TASK_BLOCKING_BUILDING=1u,
       KU_TASK_BLOCKING_QUEUED=2u, KU_TASK_BLOCKING_RUNNING=3u,
       KU_TASK_BLOCKING_DONE=4u };
typedef struct KuTaskBlockingPoolV1 KuTaskBlockingPoolV1;
typedef struct KuTaskBlockingJobV1 {
  uint64_t generation;
  uint32_t state, cancelled, kind;
  size_t output_charge;
  KuTaskDriverTicketV1 ticket;
  KuFsNativeChar* path;
  KuString content;
  KuResult_null result;
  KuResult_str read_result;
} KuTaskBlockingJobV1;
typedef struct KuTaskBlockingWorkerV1 {
  KuTaskBlockingPoolV1* pool;
  uint32_t exited, joined, closed;
#if defined(_WIN32)
  HANDLE thread;
#else
  pthread_t thread;
#endif
} KuTaskBlockingWorkerV1;
typedef struct KuTaskBlockingSnapshotV1 {
  size_t resident, queued, running, peak_resident, peak_queued, peak_running;
  uint64_t accepted, rejected, rejected_queue, finished;
  size_t workers_created, workers_exited, workers_joined;
  uint32_t closing, fault;
} KuTaskBlockingSnapshotV1;
struct KuTaskBlockingPoolV1 {
  uint32_t initialized, resources, closing, fault;
  size_t capacity, worker_target, workers_created, workers_exited, workers_joined;
  size_t head, queued, running, resident, peak_resident, peak_queued, peak_running;
  uint64_t accepted, rejected, rejected_queue, finished;
  KuTaskDriverMutexV1 mutex;
  KuTaskDriverConditionV1 condition;
#if !defined(_WIN32) && !defined(__APPLE__)
  pthread_condattr_t attributes;
#endif
  KuTaskBlockingJobV1 jobs[KU_TASK_DRIVER_MAX_SLOTS];
  size_t ring[KU_TASK_DRIVER_MAX_SLOTS];
  KuTaskBlockingWorkerV1 workers[KU_TASK_DRIVER_MAX_WORKERS];
};
static int ku_task_blocking_lock(KuTaskBlockingPoolV1* pool) {
#if defined(_WIN32)
  AcquireSRWLockExclusive(&pool->mutex); return 0;
#else
  return pthread_mutex_lock(&pool->mutex);
#endif
}
static void ku_task_blocking_unlock(KuTaskBlockingPoolV1* pool) {
#if defined(_WIN32)
  ReleaseSRWLockExclusive(&pool->mutex);
#else
  if (pthread_mutex_unlock(&pool->mutex)) pool->fault=KU_TASK_DRIVER_INTERNAL;
#endif
}
static void ku_task_blocking_signal(KuTaskBlockingPoolV1* pool) {
#if defined(_WIN32)
  WakeAllConditionVariable(&pool->condition);
#else
  if (pthread_cond_broadcast(&pool->condition)) pool->fault=KU_TASK_DRIVER_INTERNAL;
#endif
}
static int ku_task_blocking_wait(KuTaskBlockingPoolV1* pool, uint64_t deadline) {
#if defined(_WIN32)
  DWORD delay=INFINITE;
  if (deadline!=UINT64_MAX) {
    uint64_t now=ku_task_driver_now_ms();
    if (now==UINT64_MAX) return -1;
    if (now>=deadline) return 1;
    uint64_t remaining=deadline-now;
    delay=remaining>=(uint64_t)INFINITE ? INFINITE-1u : (DWORD)remaining;
  }
  if (SleepConditionVariableSRW(&pool->condition,&pool->mutex,delay,0)) return 0;
  return GetLastError()==ERROR_TIMEOUT ? 1 : -1;
#else
  if (deadline==UINT64_MAX) return pthread_cond_wait(&pool->condition,&pool->mutex) ? -1 : 0;
  uint64_t now=ku_task_driver_now_ms();
  if (now==UINT64_MAX) return -1;
  if (now>=deadline) return 1;
  uint64_t remaining=deadline-now;
  if (remaining>UINT64_C(86400000)) remaining=UINT64_C(86400000);
  struct timespec timeout;
#if defined(__APPLE__)
  timeout.tv_sec=(time_t)(remaining/1000u);
  timeout.tv_nsec=(long)((remaining%1000u)*1000000u);
  int status=pthread_cond_timedwait_relative_np(&pool->condition,&pool->mutex,&timeout);
#else
  if (clock_gettime(CLOCK_MONOTONIC,&timeout)) return -1;
  timeout.tv_sec+=(time_t)(remaining/1000u);
  timeout.tv_nsec+=(long)((remaining%1000u)*1000000u);
  if (timeout.tv_nsec>=1000000000L) { timeout.tv_sec++; timeout.tv_nsec-=1000000000L; }
  int status=pthread_cond_timedwait(&pool->condition,&pool->mutex,&timeout);
#endif
  return status==0 ? 0 : status==ETIMEDOUT ? 1 : -1;
#endif
}
typedef union KuTaskBlockingAllocationV1 {
  struct { size_t bytes; } allocation;
  long double alignment;
  void* pointer_alignment;
} KuTaskBlockingAllocationV1;
static void ku_task_blocking_free(void* pointer) {
  if (pointer) free(((KuTaskBlockingAllocationV1*)pointer)-1);
}
static KuError ku_task_blocking_error(const char* code, const char* message) {
  return ku_error_make(ku_string_static((const uint8_t*)"task",4),
      ku_string_static((const uint8_t*)code,strlen(code)),
      ku_string_static((const uint8_t*)message,strlen(message)));
}
/* Output buffers use plain malloc, so ownership can pass directly to the
 * ordinary KuString drop ABI. Only this worker accesses the two live entries.
 * The job charge remains mutex-protected even when the parent is cleaning up. */
typedef struct KuTaskBlockingOutputV1 {
  KuTaskBlockingPoolV1* pool;
  KuTaskBlockingJobV1* job;
  void* pointers[2];
  size_t bytes[2];
  uint32_t status;
} KuTaskBlockingOutputV1;
static void* ku_task_blocking_output_allocate(void* raw, size_t bytes) {
  KuTaskBlockingOutputV1* output=(KuTaskBlockingOutputV1*)raw;
  if (output->status!=KU_TASK_DRIVER_OK) return NULL;
  if (ku_task_blocking_lock(output->pool)) { output->status=KU_TASK_DRIVER_INTERNAL; return NULL; }
  KuTaskBlockingJobV1* job=output->job;
  uint32_t status=job->cancelled || output->pool->closing ? KU_TASK_DRIVER_CLOSED
      : bytes>SIZE_MAX-job->output_charge ? KU_TASK_DRIVER_LIMIT
      : ku_task_driver_blocking_grow_charge(&job->ticket,bytes);
  if (status==KU_TASK_DRIVER_OK) job->output_charge+=bytes;
  ku_task_blocking_unlock(output->pool);
  if (status!=KU_TASK_DRIVER_OK) { output->status=status; return NULL; }
  void* pointer=malloc(bytes);
  if (!pointer) { output->status=KU_TASK_BLOCKING_OUT_OF_MEMORY; return NULL; }
  for (size_t i=0;i<2;i++) if (!output->pointers[i]) {
    output->pointers[i]=pointer; output->bytes[i]=bytes; return pointer;
  }
  free(pointer); output->status=KU_TASK_DRIVER_INTERNAL; return NULL;
}
static void ku_task_blocking_output_release(void* raw, void* pointer) {
  if (!pointer) return;
  KuTaskBlockingOutputV1* output=(KuTaskBlockingOutputV1*)raw;
  for (size_t i=0;i<2;i++) if (output->pointers[i]==pointer) {
    size_t bytes=output->bytes[i]; free(pointer);
    output->pointers[i]=NULL; output->bytes[i]=0;
    if (ku_task_blocking_lock(output->pool)) { output->status=KU_TASK_DRIVER_INTERNAL; return; }
    if (bytes>output->job->output_charge) output->status=KU_TASK_DRIVER_INTERNAL;
    else {
      uint32_t status=ku_task_driver_blocking_refund_charge(&output->job->ticket,bytes);
      if (status==KU_TASK_DRIVER_OK) output->job->output_charge-=bytes;
      else output->status=status;
    }
    ku_task_blocking_unlock(output->pool); return;
  }
  output->status=KU_TASK_DRIVER_INTERNAL;
}
static void ku_task_blocking_worker(KuTaskBlockingWorkerV1* worker) {
  KuTaskBlockingPoolV1* pool=worker->pool;
  if (ku_task_blocking_lock(pool)) return;
  for (;;) {
    while (!pool->queued && !pool->closing) {
      if (ku_task_blocking_wait(pool,UINT64_MAX)<0) {
        pool->fault=KU_TASK_DRIVER_INTERNAL; pool->closing=1; break;
      }
    }
    if (!pool->queued && pool->closing) break;
    size_t index=pool->ring[pool->head];
    pool->head=(pool->head+1u)%pool->capacity; pool->queued--;
    KuTaskBlockingJobV1* job=&pool->jobs[index];
    if (job->state!=KU_TASK_BLOCKING_QUEUED) {
      pool->fault=KU_TASK_DRIVER_INTERNAL; pool->closing=1; break;
    }
    job->state=KU_TASK_BLOCKING_RUNNING; pool->running++;
    if (pool->running>pool->peak_running) pool->peak_running=pool->running;
    KuFsNativeChar* path=job->path;
    KuString content=job->content;
    uint32_t start=ku_task_driver_blocking_can_start(&job->ticket);
    int cancelled=job->cancelled || pool->closing || start!=KU_TASK_DRIVER_OK;
    int closing=pool->closing;
    ku_task_blocking_unlock(pool);
    KuError error=cancelled ? ku_task_blocking_error(
        closing ? "blocking_pool_stopped" : "cancelled",
        closing ? "blocking pool is closing" : "blocking job was cancelled before file operation") : (KuError){0};
    uint32_t kind=job->kind;
    KuString read_value={0};
    KuTaskBlockingOutputV1 allocation={pool,job,{NULL,NULL},{0,0},KU_TASK_DRIVER_OK};
    KuFsAllocatorV1 allocator={&allocation,ku_task_blocking_output_allocate,ku_task_blocking_output_release};
    bool ok=false;
    if (!cancelled) {
      ok=kind==1u ? ku_fs_read_native_impl(path,&read_value,&error,&allocator)
          : ku_fs_write_native_impl(path,content,&error);
      if (allocation.status!=KU_TASK_DRIVER_OK) {
        if (read_value.ptr) { ku_task_blocking_output_release(&allocation,read_value.ptr); read_value=(KuString){0}; }
        ok=false;
        error=ku_task_blocking_error(
          allocation.status==KU_TASK_DRIVER_LIMIT ? "memory_limit" : allocation.status==KU_TASK_BLOCKING_OUT_OF_MEMORY ? "out_of_memory" : allocation.status==KU_TASK_DRIVER_CLOSED ? "cancelled" : "internal_error",
          allocation.status==KU_TASK_DRIVER_LIMIT ? "blocking output exceeds runtime byte budget" : allocation.status==KU_TASK_BLOCKING_OUT_OF_MEMORY ? "blocking output allocation failed" : allocation.status==KU_TASK_DRIVER_CLOSED ? "blocking read was cancelled before buffer growth" : "blocking output accounting failed");
      }
    }
    /* No driver mutex, source frame, or original View storage is used here. */
    ku_task_blocking_free(path);
    ku_task_blocking_free(content.ptr);
    if (ku_task_blocking_lock(pool)) return;
    job=&pool->jobs[index];
    job->path=NULL; job->content=(KuString){0};
    if (kind==1u) job->read_result=(KuResult_str){ok,read_value,error};
    else job->result=(KuResult_null){ok,0,error};
    job->state=KU_TASK_BLOCKING_DONE; pool->running--;
    if (pool->finished!=UINT64_MAX) pool->finished++;
    /* The consumer takes this same mutex. It cannot clear/reuse the job until
     * this final notification has finished all driver/job storage accesses. */
    uint32_t wake=ku_task_driver_wake(&job->ticket);
    if (wake!=KU_TASK_DRIVER_OK) pool->fault=KU_TASK_DRIVER_INTERNAL;
    ku_task_blocking_signal(pool);
    /* The next iteration accesses pool only, never this retired job. */
  }
  worker->exited=1; pool->workers_exited++;
  ku_task_blocking_signal(pool);
  ku_task_blocking_unlock(pool);
  /* Last storage access; exclusive join/destroy may proceed after this. */
}
#if defined(_WIN32)
static unsigned __stdcall ku_task_blocking_thread(void* raw) {
  ku_task_blocking_worker((KuTaskBlockingWorkerV1*)raw); return 0;
}
#else
static void* ku_task_blocking_thread(void* raw) {
  ku_task_blocking_worker((KuTaskBlockingWorkerV1*)raw); return NULL;
}
#endif
/* All pools in this generated file share the executable-relative base.
 * Concurrent setup refuses the in-progress setup rather than allowing workers
 * to race its old static initialization. State 2 also records a failed base
 * attempt, preserving absolute-path support and the old relative-path error. */
static KuAtomicRefcount ku_task_blocking_base_prepared;
static uint32_t ku_task_blocking_prepare_base(void) {
  size_t expected=0;
  if (ku_task_control_atomic_cas(&ku_task_blocking_base_prepared,&expected,1)) {
    ku_fs_prepare_blocking_base();
    ku_task_control_atomic_store(&ku_task_blocking_base_prepared,2);
    return KU_TASK_DRIVER_OK;
  }
  return expected==2 ? KU_TASK_DRIVER_OK : KU_TASK_DRIVER_INVALID_STATE;
}
static uint32_t ku_task_blocking_init(KuTaskBlockingPoolV1* pool, size_t capacity,
    size_t workers, uint64_t deadline) {
  if (!pool || !capacity || capacity>KU_TASK_DRIVER_MAX_SLOTS || !workers
      || workers>KU_TASK_DRIVER_MAX_WORKERS || deadline==UINT64_MAX)
    return KU_TASK_DRIVER_INVALID_ARGUMENT;
  if (!ku_task_frame_zero_bytes(pool,sizeof(*pool))) return KU_TASK_DRIVER_INVALID_STATE;
  uint32_t prepared=ku_task_blocking_prepare_base();
  if (prepared!=KU_TASK_DRIVER_OK) return prepared;
  pool->initialized=KU_TASK_DRIVER_STORAGE_REAPING;
  pool->capacity=capacity; pool->worker_target=workers; pool->closing=1;
#if defined(_WIN32)
  InitializeSRWLock(&pool->mutex); pool->resources|=KU_TASK_DRIVER_SYNC_MUTEX;
  InitializeConditionVariable(&pool->condition); pool->resources|=KU_TASK_DRIVER_SYNC_STATE;
#else
  if (pthread_mutex_init(&pool->mutex,NULL)) return KU_TASK_DRIVER_INTERNAL;
  pool->resources|=KU_TASK_DRIVER_SYNC_MUTEX;
#if defined(__APPLE__)
  if (pthread_cond_init(&pool->condition,NULL)) return KU_TASK_DRIVER_INTERNAL;
  pool->resources|=KU_TASK_DRIVER_SYNC_STATE;
#else
  if (pthread_condattr_init(&pool->attributes)) return KU_TASK_DRIVER_INTERNAL;
  pool->resources|=KU_TASK_DRIVER_SYNC_ATTRIBUTES;
  if (pthread_condattr_setclock(&pool->attributes,CLOCK_MONOTONIC)
      || pthread_cond_init(&pool->condition,&pool->attributes)) return KU_TASK_DRIVER_INTERNAL;
  pool->resources|=KU_TASK_DRIVER_SYNC_STATE;
  if (pthread_condattr_destroy(&pool->attributes)) return KU_TASK_DRIVER_INTERNAL;
  pool->resources&=~KU_TASK_DRIVER_SYNC_ATTRIBUTES;
#endif
#endif
  if (ku_task_blocking_lock(pool)) return KU_TASK_DRIVER_INTERNAL;
  pool->initialized=KU_TASK_DRIVER_STORAGE_LIVE; pool->closing=0;
  uint32_t status=KU_TASK_DRIVER_OK;
  for (size_t i=0;i<workers;i++) {
    uint64_t now=ku_task_driver_now_ms();
    if (now==UINT64_MAX || now>=deadline) {
      status=now==UINT64_MAX ? KU_TASK_DRIVER_INTERNAL : KU_TASK_DRIVER_SHUTDOWN_TIMEOUT; break;
    }
    KuTaskBlockingWorkerV1* worker=&pool->workers[i]; worker->pool=pool;
#if defined(_WIN32)
    worker->thread=(HANDLE)_beginthreadex(NULL,0,ku_task_blocking_thread,worker,0,NULL);
    if (!worker->thread) { status=KU_TASK_DRIVER_INTERNAL; break; }
#else
    if (pthread_create(&worker->thread,NULL,ku_task_blocking_thread,worker)) {
      status=KU_TASK_DRIVER_INTERNAL; break;
    }
#endif
    pool->workers_created++;
  }
  if (status==KU_TASK_DRIVER_OK) {
    uint64_t now=ku_task_driver_now_ms();
    if (now==UINT64_MAX || now>=deadline)
      status=now==UINT64_MAX ? KU_TASK_DRIVER_INTERNAL : KU_TASK_DRIVER_SHUTDOWN_TIMEOUT;
  }
  if (status!=KU_TASK_DRIVER_OK) { pool->closing=1; pool->fault=status; ku_task_blocking_signal(pool); }
  ku_task_blocking_unlock(pool); return status;
}
static uint32_t ku_task_blocking_close(void* raw) {
  KuTaskBlockingPoolV1* pool=(KuTaskBlockingPoolV1*)raw;
  if (!pool) return KU_TASK_DRIVER_INVALID_ARGUMENT;
  if (pool->initialized==KU_TASK_DRIVER_STORAGE_ZERO || pool->initialized==KU_TASK_DRIVER_STORAGE_REAPING)
    return KU_TASK_DRIVER_OK;
  if (pool->initialized!=KU_TASK_DRIVER_STORAGE_LIVE) return KU_TASK_DRIVER_INVALID_STATE;
  if (ku_task_blocking_lock(pool)) return KU_TASK_DRIVER_INTERNAL;
  pool->closing=1; ku_task_blocking_signal(pool);
  ku_task_blocking_unlock(pool); return KU_TASK_DRIVER_OK;
}
static uint32_t ku_task_blocking_ready(void* raw) {
  KuTaskBlockingPoolV1* pool=(KuTaskBlockingPoolV1*)raw;
  return pool && pool->initialized==KU_TASK_DRIVER_STORAGE_ZERO
      ? KU_TASK_DRIVER_OK : KU_TASK_DRIVER_PENDING;
}
/* Exclusive lifecycle; timeout preserves every live object and original D.
 * POSIX join follows all last-storage-access EXITED witnesses, retaining the
 * same final OS-return scheduling boundary as the driver, not a hard deadline.
 * Windows waits use only the shared absolute D and can reap already-ready
 * handles even when that D has expired. */
static uint32_t ku_task_blocking_finish(KuTaskBlockingPoolV1* pool, uint64_t deadline) {
  if (!pool || deadline==UINT64_MAX) return KU_TASK_DRIVER_INVALID_ARGUMENT;
  if (pool->initialized==KU_TASK_DRIVER_STORAGE_ZERO)
    return pool->fault ? pool->fault : KU_TASK_DRIVER_OK;
  if (pool->initialized==KU_TASK_DRIVER_STORAGE_LIVE) {
    uint32_t closed=ku_task_blocking_close(pool);
    if (closed!=KU_TASK_DRIVER_OK) return closed;
    if (ku_task_blocking_lock(pool)) return KU_TASK_DRIVER_INTERNAL;
    while (pool->workers_exited!=pool->workers_created || pool->resident || pool->running || pool->queued) {
      int waited=ku_task_blocking_wait(pool,deadline);
      if (waited) { ku_task_blocking_unlock(pool); return waited<0 ? KU_TASK_DRIVER_INTERNAL : KU_TASK_DRIVER_SHUTDOWN_TIMEOUT; }
    }
    ku_task_blocking_unlock(pool);
  }
  for (size_t i=0;i<pool->workers_created;i++) {
    KuTaskBlockingWorkerV1* worker=&pool->workers[i];
    if (!worker->exited) return KU_TASK_DRIVER_PENDING;
    if (!worker->joined) {
#if defined(_WIN32)
      DWORD waited=WaitForSingleObject(worker->thread,0);
      if (waited==WAIT_TIMEOUT) {
        uint64_t now=ku_task_driver_now_ms();
        if (now==UINT64_MAX) return KU_TASK_DRIVER_INTERNAL;
        uint64_t remaining=deadline>now ? deadline-now : 0;
        DWORD delay=remaining>=(uint64_t)INFINITE ? INFINITE-1u : (DWORD)remaining;
        waited=WaitForSingleObject(worker->thread,delay);
      }
      if (waited!=WAIT_OBJECT_0) return waited==WAIT_TIMEOUT ? KU_TASK_DRIVER_SHUTDOWN_TIMEOUT : KU_TASK_DRIVER_INTERNAL;
#else
      if (pthread_join(worker->thread,NULL)) return KU_TASK_DRIVER_INTERNAL;
#endif
      worker->joined=1; pool->workers_joined++;
    }
    if (!worker->closed) {
#if defined(_WIN32)
      if (!CloseHandle(worker->thread)) return KU_TASK_DRIVER_INTERNAL;
#endif
      worker->closed=1;
    }
  }
  uint32_t failure=pool->fault;
  pool->initialized=KU_TASK_DRIVER_STORAGE_REAPING;
#if !defined(_WIN32)
#if !defined(__APPLE__)
  if (pool->resources&KU_TASK_DRIVER_SYNC_ATTRIBUTES) {
    if (pthread_condattr_destroy(&pool->attributes)) return KU_TASK_DRIVER_INTERNAL;
    pool->resources&=~KU_TASK_DRIVER_SYNC_ATTRIBUTES;
  }
#endif
  if (pool->resources&KU_TASK_DRIVER_SYNC_STATE) {
    if (pthread_cond_destroy(&pool->condition)) return KU_TASK_DRIVER_INTERNAL;
    pool->resources&=~KU_TASK_DRIVER_SYNC_STATE;
  }
  if (pool->resources&KU_TASK_DRIVER_SYNC_MUTEX) {
    if (pthread_mutex_destroy(&pool->mutex)) return KU_TASK_DRIVER_INTERNAL;
    pool->resources&=~KU_TASK_DRIVER_SYNC_MUTEX;
  }
#else
  pool->resources=0;
#endif
  pool->initialized=KU_TASK_DRIVER_STORAGE_ZERO;
  return failure ? failure : KU_TASK_DRIVER_OK;
}
static uint32_t ku_task_blocking_snapshot(KuTaskBlockingPoolV1* pool,
    KuTaskBlockingSnapshotV1* output) {
  if (!pool || !output || pool->initialized!=KU_TASK_DRIVER_STORAGE_LIVE
      || ku_task_frame_ranges_overlap(pool,sizeof(*pool),output,sizeof(*output))) return KU_TASK_DRIVER_INVALID_ARGUMENT;
  if (ku_task_blocking_lock(pool)) return KU_TASK_DRIVER_INTERNAL;
  *output=(KuTaskBlockingSnapshotV1){pool->resident,pool->queued,pool->running,
      pool->peak_resident,pool->peak_queued,pool->peak_running,pool->accepted,
      pool->rejected,pool->rejected_queue,pool->finished,pool->workers_created,
      pool->workers_exited,pool->workers_joined,pool->closing,pool->fault};
  ku_task_blocking_unlock(pool); return KU_TASK_DRIVER_OK;
}
typedef struct KuTaskBlockingBuilderV1 {
  const KuTaskDriverTicketV1* ticket;
  KuTaskBlockingTokenV1* token;
  void* context;
  uint32_t (*reconcile)(void*);
  uint32_t status;
} KuTaskBlockingBuilderV1;
static void* ku_task_blocking_allocate(void* raw, size_t bytes) {
  KuTaskBlockingBuilderV1* builder=(KuTaskBlockingBuilderV1*)raw;
  if (builder->status!=KU_TASK_DRIVER_OK) return NULL;
  if (bytes>SIZE_MAX-sizeof(KuTaskBlockingAllocationV1)) { builder->status=KU_TASK_DRIVER_LIMIT; return NULL; }
  size_t total=bytes+sizeof(KuTaskBlockingAllocationV1);
  builder->status=ku_task_driver_grow_charge(builder->ticket,total);
  if (builder->status!=KU_TASK_DRIVER_OK) return NULL;
  KuTaskBlockingAllocationV1* allocation=(KuTaskBlockingAllocationV1*)malloc(total);
  if (!allocation) {
    builder->status=builder->reconcile(builder->context);
    if (builder->status==KU_TASK_DRIVER_OK) builder->status=KU_TASK_BLOCKING_OUT_OF_MEMORY;
    return NULL;
  }
  allocation->allocation.bytes=total;
  builder->token->charged_bytes+=total;
  return allocation+1;
}
static void ku_task_blocking_release(void* raw, void* pointer) {
  if (!pointer) return;
  KuTaskBlockingBuilderV1* builder=(KuTaskBlockingBuilderV1*)raw;
  KuTaskBlockingAllocationV1* allocation=((KuTaskBlockingAllocationV1*)pointer)-1;
  size_t bytes=allocation->allocation.bytes;
  free(allocation);
  if (bytes>builder->token->charged_bytes) { builder->status=KU_TASK_DRIVER_INTERNAL; return; }
  builder->token->charged_bytes-=bytes;
  uint32_t status=builder->reconcile(builder->context);
  if (status!=KU_TASK_DRIVER_OK) builder->status=status;
}
static KuTaskBlockingJobV1* ku_task_blocking_find(KuTaskBlockingPoolV1* pool,
    const KuTaskBlockingTokenV1* token) {
  if (token->pool!=pool || token->slot>=pool->capacity || !token->generation) return NULL;
  KuTaskBlockingJobV1* job=&pool->jobs[token->slot];
  return job->generation==token->generation && job->state!=KU_TASK_BLOCKING_FREE ? job : NULL;
}
static uint32_t ku_task_blocking_reconcile_charge(const KuTaskDriverTicketV1* ticket,
    const KuTaskBlockingTokenV1* token, size_t instance_bytes, size_t bytes) {
  if (!token->pool) return ku_task_driver_reconcile_charge(ticket,instance_bytes,bytes);
  KuTaskBlockingPoolV1* pool=(KuTaskBlockingPoolV1*)token->pool;
  if (ku_task_blocking_lock(pool)) return KU_TASK_DRIVER_INTERNAL;
  KuTaskBlockingJobV1* job=ku_task_blocking_find(pool,token);
  uint32_t status=!job || job->ticket.driver!=ticket->driver || job->ticket.slot!=ticket->slot
      || job->ticket.generation!=ticket->generation ? KU_TASK_DRIVER_STALE
      : job->output_charge>SIZE_MAX-bytes ? KU_TASK_DRIVER_LIMIT
      : ku_task_driver_reconcile_charge(ticket,instance_bytes,bytes+job->output_charge);
  ku_task_blocking_unlock(pool); return status;
}
/* All token access belongs to the sole parent executor. No pool worker keeps
 * a token/frame pointer. read(DONE) is the physical job handoff checkpoint. */
static uint32_t ku_task_blocking_cancel(const KuTaskDriverTicketV1* ticket,
    KuTaskBlockingTokenV1* token) {
  KuTaskBlockingPoolV1* pool=(KuTaskBlockingPoolV1*)token->pool;
  if (!pool || pool->initialized!=KU_TASK_DRIVER_STORAGE_LIVE) return KU_TASK_DRIVER_INVALID_ARGUMENT;
  if (ku_task_blocking_lock(pool)) return KU_TASK_DRIVER_INTERNAL;
  KuTaskBlockingJobV1* job=ku_task_blocking_find(pool,token);
  uint32_t status=!job || job->ticket.driver!=ticket->driver || job->ticket.slot!=ticket->slot
      || job->ticket.generation!=ticket->generation ? KU_TASK_DRIVER_STALE : KU_TASK_DRIVER_OK;
  if (status==KU_TASK_DRIVER_OK) job->cancelled=1;
  ku_task_blocking_signal(pool); ku_task_blocking_unlock(pool);
  return status;
}
static uint32_t ku_task_blocking_take(const KuTaskDriverTicketV1* ticket,
    KuTaskBlockingTokenV1* token, int cancel, uint32_t kind, void* output) {
  KuTaskBlockingPoolV1* pool=(KuTaskBlockingPoolV1*)token->pool;
  if (!pool || !output || pool->initialized!=KU_TASK_DRIVER_STORAGE_LIVE) return KU_TASK_DRIVER_INVALID_ARGUMENT;
  if (ku_task_blocking_lock(pool)) return KU_TASK_DRIVER_INTERNAL;
  KuTaskBlockingJobV1* job=ku_task_blocking_find(pool,token);
  uint32_t status=!job || job->ticket.driver!=ticket->driver || job->ticket.slot!=ticket->slot
      || job->ticket.generation!=ticket->generation ? KU_TASK_DRIVER_STALE : KU_TASK_DRIVER_PENDING;
  if (status==KU_TASK_DRIVER_PENDING) {
    if (cancel) job->cancelled=1;
    if (job->state==KU_TASK_BLOCKING_DONE) {
      if (kind!=2u && kind!=job->kind) { ku_task_blocking_unlock(pool); return KU_TASK_DRIVER_INVALID_ARGUMENT; }
      status=ku_task_driver_blocking_retired(ticket);
      if (status!=KU_TASK_DRIVER_OK) {
        pool->fault=KU_TASK_DRIVER_INTERNAL; ku_task_blocking_unlock(pool); return status;
      }
      if (kind==2u) {
        if (job->kind==1u) ku_result_drop_str(&job->read_result);
        else ku_result_drop_null(&job->result);
      } else if (kind==1u) *(KuResult_str*)output=job->read_result;
      else *(KuResult_null*)output=job->result;
      uint64_t generation=job->generation;
      memset(job,0,sizeof(*job)); job->generation=generation;
      pool->resident--;
      *token=(KuTaskBlockingTokenV1){0};
      ku_task_blocking_signal(pool);
    } else if (job->state==KU_TASK_BLOCKING_BUILDING) status=KU_TASK_DRIVER_INVALID_STATE;
  }
  ku_task_blocking_unlock(pool);
  if (status==KU_TASK_DRIVER_PENDING) {
    uint32_t intent=ku_task_driver_set_intent(ticket,KU_TASK_DRIVER_WAIT);
    if (intent!=KU_TASK_DRIVER_OK) return intent;
  }
  return status;
}
static uint32_t ku_task_blocking_read(const KuTaskDriverTicketV1* ticket,
    KuTaskBlockingTokenV1* token, int cancel, KuResult_null* output) {
  return ku_task_blocking_take(ticket,token,cancel,0u,output);
}
static uint32_t ku_task_blocking_read_string(const KuTaskDriverTicketV1* ticket,
    KuTaskBlockingTokenV1* token, KuResult_str* output) {
  return ku_task_blocking_take(ticket,token,0,1u,output);
}
static uint32_t ku_task_blocking_discard(const KuTaskDriverTicketV1* ticket,
    KuTaskBlockingTokenV1* token) {
  uint32_t sentinel=0; return ku_task_blocking_take(ticket,token,1,2u,&sentinel);
}
static uint32_t ku_task_blocking_submit_impl(const KuTaskDriverTicketV1* ticket,
    KuTaskControlV1* control, KuTaskBlockingTokenV1* token,
    KuString path, KuString content, void* context,
    uint32_t (*reconcile)(void*), uint32_t kind, KuResult_null* output) {
  if (!ticket || !control || !token || token->pool || token->charged_bytes || !output
      || !context || !reconcile) return KU_TASK_DRIVER_INVALID_ARGUMENT;
  KuError error={0};
  if (!kind && !ku_fs_write_validate(content,&error)) { *output=(KuResult_null){false,0,error}; return KU_TASK_DRIVER_OK; }
  KuTaskBlockingPoolV1* pool=(KuTaskBlockingPoolV1*)ticket->driver->blocking_pool;
  if (!pool || pool->initialized!=KU_TASK_DRIVER_STORAGE_LIVE) {
    *output=(KuResult_null){false,0,ku_task_blocking_error("blocking_pool_stopped","blocking pool is not available")};
    return KU_TASK_DRIVER_OK;
  }
  if (ku_task_blocking_lock(pool)) return KU_TASK_DRIVER_INTERNAL;
  uint32_t status=pool->closing || pool->fault || pool->workers_created!=pool->worker_target
      ? KU_TASK_DRIVER_CLOSED : pool->resident>=pool->capacity ? KU_TASK_DRIVER_LIMIT : KU_TASK_DRIVER_OK;
  size_t index=0;
  if (status==KU_TASK_DRIVER_OK) {
    for (;index<pool->capacity;index++) if (pool->jobs[index].state==KU_TASK_BLOCKING_FREE
        && pool->jobs[index].generation!=UINT64_MAX) break;
    if (index==pool->capacity) status=KU_TASK_DRIVER_LIMIT;
  }
  if (status!=KU_TASK_DRIVER_OK) {
    if (pool->rejected!=UINT64_MAX) pool->rejected++;
    if (status==KU_TASK_DRIVER_LIMIT && pool->rejected_queue!=UINT64_MAX) pool->rejected_queue++;
    ku_task_blocking_unlock(pool);
    *output=(KuResult_null){false,0,ku_task_blocking_error(status==KU_TASK_DRIVER_LIMIT ? "blocking_queue_full" : "blocking_pool_stopped",
        status==KU_TASK_DRIVER_LIMIT ? "blocking queue is full" : "blocking pool is closing")};
    return KU_TASK_DRIVER_OK;
  }
  KuTaskBlockingJobV1* job=&pool->jobs[index];
  job->generation++; job->state=KU_TASK_BLOCKING_BUILDING; job->ticket=*ticket; job->kind=kind;
  *token=(KuTaskBlockingTokenV1){pool,index,0,job->generation};
  pool->resident++; if (pool->resident>pool->peak_resident) pool->peak_resident=pool->resident;
  ku_task_blocking_unlock(pool);
  status=ku_task_driver_blocking_register(ticket);
  int registered=status==KU_TASK_DRIVER_OK;
  KuTaskBlockingBuilderV1 builder={ticket,token,context,reconcile,status};
  KuFsAllocatorV1 allocator={&builder,ku_task_blocking_allocate,ku_task_blocking_release};
  KuFsNativeChar* native=NULL;
  uint8_t* copy=NULL;
  if (registered) {
    status=reconcile(context);
    builder.status=status;
    if (status==KU_TASK_DRIVER_OK)
      native=ku_fs_native_path_with_allocator(path,&error,kind ? "read_failed" : "write_failed",kind ? "failed to read: invalid path" : "failed to write: invalid path",&allocator);
    if (native && builder.status==KU_TASK_DRIVER_OK && content.len) {
      copy=(uint8_t*)ku_task_blocking_allocate(&builder,content.len);
      if (copy) memcpy(copy,content.ptr,content.len);
    }
  }
  status=builder.status;
  if (!native || (content.len && !copy)) {
    if (status==KU_TASK_DRIVER_OK && !native && !error.message.ptr) status=KU_TASK_DRIVER_INTERNAL;
    goto ku_blocking_rejected;
  }
  if (ku_task_blocking_lock(pool)) return KU_TASK_DRIVER_INTERNAL;
  if (pool->closing || pool->fault || ku_task_control_atomic_load(&control->phase)!=KU_TASK_CONTROL_LIVE) {
    status=KU_TASK_DRIVER_CLOSED; ku_task_blocking_unlock(pool); goto ku_blocking_rejected;
  }
  job=&pool->jobs[index];
  job->path=native; job->content=(KuString){copy,content.len,content.len,KU_STRING_OWNED};
  job->state=KU_TASK_BLOCKING_QUEUED;
  pool->ring[(pool->head+pool->queued)%pool->capacity]=index; pool->queued++;
  if (pool->queued>pool->peak_queued) pool->peak_queued=pool->queued;
  if (pool->accepted!=UINT64_MAX) pool->accepted++;
  ku_task_blocking_signal(pool);
  ku_task_blocking_unlock(pool);
  status=ku_task_driver_set_intent(ticket,KU_TASK_DRIVER_WAIT);
  return status==KU_TASK_DRIVER_OK ? KU_TASK_DRIVER_PENDING : status;
ku_blocking_rejected:
  ku_task_blocking_release(&builder,native);
  ku_task_blocking_release(&builder,copy);
  if (builder.status!=KU_TASK_DRIVER_OK) status=builder.status;
  if (ku_task_blocking_lock(pool)) return KU_TASK_DRIVER_INTERNAL;
  job=&pool->jobs[index];
  uint64_t generation=job->generation;
  memset(job,0,sizeof(*job)); job->generation=generation;
  pool->resident--; if (pool->rejected!=UINT64_MAX) pool->rejected++;
  ku_task_blocking_signal(pool); ku_task_blocking_unlock(pool);
  if (registered && ku_task_driver_blocking_retired(ticket)!=KU_TASK_DRIVER_OK) return KU_TASK_DRIVER_INTERNAL;
  *token=(KuTaskBlockingTokenV1){0};
  uint32_t reconciled=reconcile(context);
  if (reconciled!=KU_TASK_DRIVER_OK) return reconciled;
  if (status!=KU_TASK_DRIVER_OK) error=ku_task_blocking_error(
      status==KU_TASK_DRIVER_LIMIT ? "memory_limit" : status==KU_TASK_BLOCKING_OUT_OF_MEMORY ? "out_of_memory" : "blocking_pool_stopped",
      status==KU_TASK_DRIVER_LIMIT ? "blocking input exceeds runtime byte budget" : status==KU_TASK_BLOCKING_OUT_OF_MEMORY ? "blocking input allocation failed" : "blocking pool is closing");
  *output=(KuResult_null){false,0,error};
  return KU_TASK_DRIVER_OK;
}
static uint32_t ku_task_blocking_submit(const KuTaskDriverTicketV1* ticket,
    KuTaskControlV1* control, KuTaskBlockingTokenV1* token,
    KuString path, KuString content, void* context,
    uint32_t (*reconcile)(void*), KuResult_null* output) {
  return ku_task_blocking_submit_impl(ticket,control,token,path,content,context,reconcile,0u,output);
}
static uint32_t ku_task_blocking_submit_read(const KuTaskDriverTicketV1* ticket,
    KuTaskControlV1* control, KuTaskBlockingTokenV1* token,
    KuString path, void* context, uint32_t (*reconcile)(void*), KuResult_str* output) {
  KuResult_null submitted={0};
  uint32_t status=ku_task_blocking_submit_impl(ticket,control,token,path,(KuString){0},context,reconcile,1u,&submitted);
  if (status==KU_TASK_DRIVER_OK) *output=(KuResult_str){false,{0},submitted.error};
  return status;
}
"#;
