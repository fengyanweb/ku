//! Fixed native blocking jobs. A parked source frame retains its actual job
//! until the worker has closed the file and stopped touching all job storage.

use super::output::COutput;
use crate::error::KuResult;

pub(super) fn emit(
    out: &mut COutput,
    enabled: bool,
    files: bool,
    directory: bool,
    network: bool,
    postgres: bool,
) -> KuResult<()> {
    out.push_str(TOKEN);
    if enabled {
        out.push_str(if files {
            "#define KU_TASK_BLOCKING_FILES 1\n"
        } else {
            "#define KU_TASK_BLOCKING_FILES 0\n"
        });
        out.push_str(if network {
            "#define KU_TASK_BLOCKING_NETWORK 1\n"
        } else {
            "#define KU_TASK_BLOCKING_NETWORK 0\n"
        });
        out.push_str(if directory {
            "#define KU_TASK_BLOCKING_DIRECTORY 1\n"
        } else {
            "#define KU_TASK_BLOCKING_DIRECTORY 0\n"
        });
        out.push_str(if postgres {
            "#define KU_TASK_BLOCKING_PG 1\n"
        } else {
            "#define KU_TASK_BLOCKING_PG 0\n"
        });
        out.push_str(RUNTIME);
        if postgres {
            out.push_str(include_str!("c_task_pg_runtime.c"));
        }
        if network {
            out.push_str(NETWORK_RUNTIME);
            out.push_str(include_str!("c_task_reactor.h"));
            out.push_str(include_str!("c_task_reactor.c"));
        }
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
       KU_TASK_BLOCKING_DONE=4u, KU_TASK_BLOCKING_NET_WAIT=5u,
       KU_TASK_BLOCKING_NET_STEP=6u };
typedef struct KuTaskBlockingPoolV1 KuTaskBlockingPoolV1;
#if KU_TASK_BLOCKING_NETWORK
/* submit_net validates all nine source fields before reserving a job. The
 * physical snapshot therefore needs three owned strings and five integers,
 * not nine general KuValue unions. TLS fits beside the presence mask below.
 * Snapshot payload allocation, its charge and retirement stay unchanged. */
typedef struct KuTaskBlockingNetConfigV1 {
  KuString strings[3]; /* host, tls_server_name, tls_ca_pem */
  int64_t integers[5]; /* port, connect/read/write timeout, max_read_bytes */
} KuTaskBlockingNetConfigV1;
#endif
typedef struct KuTaskBlockingJobV1 {
  uint64_t generation;
  uint32_t state, cancelled, kind;
  size_t output_charge;
  KuTaskDriverTicketV1 ticket;
#if KU_TASK_BLOCKING_FILES
  KuFsNativeChar* path;
#else
  void* path;
#endif
  KuString content;
  /* kind is fixed from BUILDING through DONE retirement. Only its matching
   * Result is ever published; inputs, physical operation state and exists_value
   * remain independent because they may outlive/ accompany that Result. */
  union {
    KuResult_null result;             /* write/exists/net write/net close */
    KuResult_str read_result;         /* file read */
#if KU_TASK_BLOCKING_DIRECTORY
    KuResult_array_str directory_result;
#endif
#if KU_TASK_BLOCKING_PG
    KuResult_pg_result pg_result;
#endif
#if KU_TASK_BLOCKING_NETWORK
    KuResult_net_client net_connect_result;
    KuResult_bytes net_read_result;
#endif
  };
  bool exists_value;
#if KU_TASK_BLOCKING_PG
  KuPgAsyncOp* pg_operation;
  KuTaskBlockingPoolV1* pg_pool;
  uint32_t pg_ready;
#endif
#if KU_TASK_BLOCKING_NETWORK
  KuTaskBlockingNetConfigV1 net_config;
  uint16_t net_present;
  bool net_tls;
  uint64_t net_deadline;
  int64_t net_requested;
  KuNetClient* net_client;
  KuBytes net_write;
  size_t net_owner,net_offset,net_address_count,net_address_index;
  void* net_addresses;
  uint32_t net_plain,net_resolved;
  uint32_t net_previous_plus_one,net_next_plus_one;
#endif
} KuTaskBlockingJobV1;
#if KU_TASK_BLOCKING_NETWORK
/* Private, validated field indices only; callers visit present fields. The
 * returned KuValue borrows the job's still-owned string snapshot, just as the
 * former inline KuValue array did while building the temporary config map. */
static KuValue ku_task_blocking_net_config_value(const KuTaskBlockingJobV1* job,size_t index) {
  KuValue value={0};
  if(index==0u || index==7u || index==8u) {
    value.tag=KU_STR;value.as.s=job->net_config.strings[index==0u ? 0u:index-6u];
  } else if(index==6u) {value.tag=KU_BOOL;value.as.b=job->net_tls;}
  else if(index>=1u && index<=5u) {value.tag=KU_INT;value.as.i=job->net_config.integers[index-1u];}
  return value;
}
static void ku_task_blocking_net_config_store(KuTaskBlockingJobV1* job,size_t index,KuValue value) {
  if(index==0u || index==7u || index==8u)
    job->net_config.strings[index==0u ? 0u:index-6u]=value.as.s;
  else if(index==6u)job->net_tls=value.as.b;
  else if(index>=1u && index<=5u)job->net_config.integers[index-1u]=value.as.i;
}
#endif
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
  void* retirement_context;
  void (*retirement_notify)(void*);
  KuTaskDriverMutexV1 mutex;
  KuTaskDriverConditionV1 condition;
#if !defined(_WIN32) && !defined(__APPLE__)
  pthread_condattr_t attributes;
#endif
  void* storage; size_t storage_bytes;
  KuTaskBlockingJobV1* jobs;
  size_t* ring;
#if KU_TASK_BLOCKING_NETWORK
  size_t net_waiting[KU_TASK_DRIVER_MAX_WORKERS];
  uint32_t net_head_plus_one[KU_TASK_DRIVER_MAX_WORKERS];
#endif
  KuTaskBlockingWorkerV1 workers[64u];
};
static size_t ku_task_blocking_fixed_bytes(size_t capacity) {
  if(capacity>(SIZE_MAX-sizeof(KuTaskBlockingPoolV1))/(sizeof(KuTaskBlockingJobV1)+sizeof(size_t)))return SIZE_MAX;
  return sizeof(KuTaskBlockingPoolV1)+capacity*(sizeof(KuTaskBlockingJobV1)+sizeof(size_t));
}
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
#if KU_TASK_BLOCKING_NETWORK
/* Called only under the pool mutex. NET_STEP does not contribute interests;
 * ownership/generation remains protected until its bounded step returns.
 * Zero means unlinked; the configured 16384-slot bound fits uint32_t indices.
 * Validate every affected node before editing any link or count. The private
 * pool is emitted with its callers in the same translation unit; this does not
 * change the public driver/frame ABI or add a capacity-sized initialization. */
static uint32_t ku_task_blocking_net_state_locked(KuTaskBlockingPoolV1* pool,
    KuTaskBlockingJobV1* job,uint32_t next) {
  int before=job->state==KU_TASK_BLOCKING_NET_WAIT;
  int after=next==KU_TASK_BLOCKING_NET_WAIT;
  if(before || after) {
    if(!pool->jobs || !pool->capacity || pool->capacity>KU_TASK_DRIVER_MAX_SLOTS
        || !job->net_owner || job->net_owner>KU_TASK_DRIVER_MAX_WORKERS
        || !job->generation)goto invalid;
    uintptr_t address=(uintptr_t)job,base=(uintptr_t)pool->jobs;
    if(address<base || (address-base)%sizeof(*job)
        || (address-base)/sizeof(*job)>=pool->capacity)goto invalid;
    uint32_t member=(uint32_t)((address-base)/sizeof(*job))+1u;
    size_t owner=job->net_owner-1u,count=pool->net_waiting[owner];
    uint32_t head=pool->net_head_plus_one[owner];
    if(count>pool->capacity || ((count==0)!=(head==0)) || head>pool->capacity)goto invalid;
    KuTaskBlockingJobV1* first=head ? &pool->jobs[head-1u]:NULL;
    if(first && (first->state!=KU_TASK_BLOCKING_NET_WAIT || !first->generation
        || first->net_owner!=job->net_owner || first->net_previous_plus_one))goto invalid;
    if(before) {
      uint32_t previous=job->net_previous_plus_one,following=job->net_next_plus_one;
      if(!count || previous>pool->capacity || following>pool->capacity
          || previous==member || following==member || (previous && previous==following)
          || ((previous==0)!=(head==member))
          || ((count==1)!=(previous==0 && following==0)))goto invalid;
      KuTaskBlockingJobV1* left=previous ? &pool->jobs[previous-1u]:NULL;
      KuTaskBlockingJobV1* right=following ? &pool->jobs[following-1u]:NULL;
      if(left && (left->state!=KU_TASK_BLOCKING_NET_WAIT || !left->generation || left->net_owner!=job->net_owner
          || left->net_next_plus_one!=member))goto invalid;
      if(right && (right->state!=KU_TASK_BLOCKING_NET_WAIT || !right->generation || right->net_owner!=job->net_owner
          || right->net_previous_plus_one!=member))goto invalid;
      if(!after) {
        if(left)left->net_next_plus_one=following;else pool->net_head_plus_one[owner]=following;
        if(right)right->net_previous_plus_one=previous;
        job->net_previous_plus_one=job->net_next_plus_one=0;
        pool->net_waiting[owner]=count-1u;
      }
    } else {
      if(count>=pool->capacity || job->net_previous_plus_one || job->net_next_plus_one
          || head==member)goto invalid;
      job->net_next_plus_one=head;
      if(first)first->net_previous_plus_one=member;
      pool->net_head_plus_one[owner]=member;pool->net_waiting[owner]=count+1u;
    }
  }
  job->state=next;return KU_TASK_DRIVER_OK;
invalid:
  pool->fault=KU_TASK_DRIVER_INTERNAL;return KU_TASK_DRIVER_INTERNAL;
}
#endif
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
 * ordinary KuString drop ABI. Only this worker accesses the bounded allocation metadata.
 * The job charge remains mutex-protected even when the parent is cleaning up. */
typedef struct KuTaskBlockingOutputEntryV1 { void* pointer; size_t bytes; } KuTaskBlockingOutputEntryV1;
typedef struct KuTaskBlockingOutputV1 {
  KuTaskBlockingPoolV1* pool;
  KuTaskBlockingJobV1* job;
  void* pointers[2];
  size_t bytes[2];
  uint32_t status;
  KuTaskBlockingOutputEntryV1* entries;
  size_t count, capacity, metadata_bytes;
} KuTaskBlockingOutputV1;
static uint32_t ku_task_blocking_output_reserve(KuTaskBlockingOutputV1* output,size_t bytes) {
  if (ku_task_blocking_lock(output->pool)) return KU_TASK_DRIVER_INTERNAL;
  KuTaskBlockingJobV1* job=output->job;
  uint32_t status=job->cancelled || output->pool->closing ? KU_TASK_DRIVER_CLOSED
      : bytes>SIZE_MAX-job->output_charge ? KU_TASK_DRIVER_LIMIT
      : ku_task_driver_blocking_grow_charge(&job->ticket,bytes);
  if (status==KU_TASK_DRIVER_OK) job->output_charge+=bytes;
  ku_task_blocking_unlock(output->pool); return status;
}
static void ku_task_blocking_output_refund(KuTaskBlockingOutputV1* output,size_t bytes) {
  if (ku_task_blocking_lock(output->pool)) { output->status=KU_TASK_DRIVER_INTERNAL; return; }
  if (bytes>output->job->output_charge) output->status=KU_TASK_DRIVER_INTERNAL;
  else {
    uint32_t status=ku_task_driver_blocking_refund_charge(&output->job->ticket,bytes);
    if (status==KU_TASK_DRIVER_OK) output->job->output_charge-=bytes;
    else output->status=status;
  }
  ku_task_blocking_unlock(output->pool);
}
static void* ku_task_blocking_output_allocate(void* raw, size_t bytes) {
  KuTaskBlockingOutputV1* output=(KuTaskBlockingOutputV1*)raw;
  if (output->status!=KU_TASK_DRIVER_OK) return NULL;
  if (output->job->kind==4u) {
    if (output->count==output->capacity) {
      /* At most 10000 paths, an old/new vector and two platform temporaries. */
      const size_t limit=10004u;
      size_t next=output->capacity ? output->capacity*2u : 16u;
      if (next>limit) next=limit;
      if (next<=output->capacity || next>SIZE_MAX/sizeof(*output->entries)) {
        output->status=KU_TASK_DRIVER_LIMIT; return NULL;
      }
      size_t metadata=next*sizeof(*output->entries);
      output->status=ku_task_blocking_output_reserve(output,metadata);
      if (output->status!=KU_TASK_DRIVER_OK) return NULL;
      KuTaskBlockingOutputEntryV1* grown=(KuTaskBlockingOutputEntryV1*)malloc(metadata);
      if (!grown) {
        ku_task_blocking_output_refund(output,metadata);
        if (output->status==KU_TASK_DRIVER_OK) output->status=KU_TASK_BLOCKING_OUT_OF_MEMORY;
        return NULL;
      }
      if (output->count) memcpy(grown,output->entries,output->count*sizeof(*grown));
      free(output->entries);
      if (output->metadata_bytes) ku_task_blocking_output_refund(output,output->metadata_bytes);
      output->entries=grown; output->capacity=next; output->metadata_bytes=metadata;
      if (output->status!=KU_TASK_DRIVER_OK) return NULL;
    }
  }
  output->status=ku_task_blocking_output_reserve(output,bytes);
  if (output->status!=KU_TASK_DRIVER_OK) return NULL;
  void* pointer=malloc(bytes);
  if (!pointer) {
    ku_task_blocking_output_refund(output,bytes);
    if (output->status==KU_TASK_DRIVER_OK) output->status=KU_TASK_BLOCKING_OUT_OF_MEMORY;
    return NULL;
  }
  if (output->job->kind==4u) {
    output->entries[output->count++]=(KuTaskBlockingOutputEntryV1){pointer,bytes}; return pointer;
  }
  for (size_t i=0;i<2;i++) if (!output->pointers[i]) {
    output->pointers[i]=pointer; output->bytes[i]=bytes; return pointer;
  }
  free(pointer); ku_task_blocking_output_refund(output,bytes);
  output->status=KU_TASK_DRIVER_INTERNAL; return NULL;
}
static void ku_task_blocking_output_release(void* raw, void* pointer) {
  if (!pointer) return;
  KuTaskBlockingOutputV1* output=(KuTaskBlockingOutputV1*)raw;
  if (output->job->kind==4u) {
    /* Builder teardown is reverse insertion order; vector growth releases
     * only O(log N) older buffers. No directory-wide pairwise traversal. */
    for (size_t i=output->count;i>0;i--) if (output->entries[i-1u].pointer==pointer) {
      size_t bytes=output->entries[i-1u].bytes; free(pointer);
      output->entries[i-1u]=output->entries[--output->count];
      ku_task_blocking_output_refund(output,bytes); return;
    }
  } else for (size_t i=0;i<2;i++) if (output->pointers[i]==pointer) {
    size_t bytes=output->bytes[i]; free(pointer);
    output->pointers[i]=NULL; output->bytes[i]=0;
    ku_task_blocking_output_refund(output,bytes); return;
  }
  output->status=KU_TASK_DRIVER_INTERNAL;
}
#if KU_TASK_BLOCKING_NETWORK
/* The Result storage is shared. Dropping another variant would interpret its
 * payload/error at the wrong offsets; kind, not zero-filled inactive fields,
 * identifies the sole owner on every normal/error/cancel retirement path. */
static void ku_task_blocking_net_drop_result(KuTaskBlockingJobV1* job) {
  if (job->kind==5u) ku_result_drop_net_client(&job->net_connect_result);
  else if (job->kind==6u) ku_result_drop_bytes(&job->net_read_result);
  else ku_result_drop_null(&job->result);
}
static void ku_task_blocking_net_drop_config(KuTaskBlockingJobV1* job) {
  static const unsigned fields[3]={0u,7u,8u};
  for(size_t i=0;i<3u;i++)if(job->net_present & (uint16_t)(1u<<fields[i])) {
    ku_task_blocking_free(job->net_config.strings[i].ptr);
    job->net_config.strings[i]=(KuString){0};
  }
}
static void ku_task_net_resolve(KuTaskBlockingJobV1*,KuObject*,KuTaskBlockingOutputV1*);
static void ku_task_blocking_net_run(KuTaskBlockingJobV1* job, int cancelled,
    KuTaskBlockingOutputV1* allocation) {
  uint64_t previous_deadline=__ku_handler_deadline;
  uint64_t previous_cleanup=__ku_handler_cleanup_deadline;
  int previous_timeout=__ku_handler_timed_out;
  unsigned long previous_unwind=__ku_handler_unwind_depth;
  __ku_handler_deadline=job->net_deadline;
  __ku_handler_cleanup_deadline=0; __ku_handler_timed_out=0; __ku_handler_unwind_depth=0;
  KuNetAllocatorV1 allocator={allocation,ku_task_blocking_output_allocate,ku_task_blocking_output_release};
  KuError error=cancelled ? ku_task_blocking_error("cancelled","network job cancelled before operation") : (KuError){0};
  if (job->kind==5u) {
    if (cancelled) job->net_connect_result=(KuResult_net_client){false,NULL,error};
    else {
      static const char* names[9]={"host","port","connect_timeout_ms","read_timeout_ms","write_timeout_ms","max_read_bytes","tls","tls_server_name","tls_ca_pem"};
      KuEntry entries[16]; memset(entries,0,sizeof(entries));
      KuObject config={0,16,entries};
      for (size_t i=0;i<9u;i++) if (job->net_present & (uint16_t)(1u<<i)) {
        KuString key=ku_string_static((const uint8_t*)names[i],strlen(names[i]));
        size_t at=(size_t)ku_obj_hash(key)&15u;
        while (entries[at].used) at=(at+1u)&15u;
        entries[at]=(KuEntry){key,ku_task_blocking_net_config_value(job,i),true}; config.len++;
      }
      if(job->net_plain) ku_task_net_resolve(job,&config,allocation);
      else job->net_connect_result=ku_net_client_with_allocator(&config,&allocator);
    }
  } else if (job->kind==6u) {
    job->net_read_result=cancelled ? (KuResult_bytes){false,{0},error}
        : ku_net_read_with_allocator(job->net_client,job->net_requested,&allocator);
  } else if (job->kind==7u) {
    job->result=cancelled ? (KuResult_null){false,0,error}
        : ku_net_write(job->net_client,job->net_write);
  } else {
    /* Close has taken the exclusive source owner; no worker may borrow it. */
    if (job->net_client) ku_net_close(job->net_client);
    job->net_client=NULL; job->result=(KuResult_null){true,0,{0}};
  }
  __ku_handler_deadline=previous_deadline; __ku_handler_cleanup_deadline=previous_cleanup;
  __ku_handler_timed_out=previous_timeout; __ku_handler_unwind_depth=previous_unwind;
  /* Config and write snapshots use the private prepaid prefix allocator. */
  ku_task_blocking_net_drop_config(job);
  ku_task_blocking_free(job->net_write.ptr); job->net_write=(KuBytes){0};
  if (allocation->status!=KU_TASK_DRIVER_OK) {
    ku_task_blocking_net_drop_result(job);
    error=ku_task_blocking_error(allocation->status==KU_TASK_DRIVER_LIMIT ? "memory_limit"
        : allocation->status==KU_TASK_DRIVER_CLOSED ? "cancelled"
        : allocation->status==KU_TASK_BLOCKING_OUT_OF_MEMORY ? "out_of_memory" : "internal_error",
        "network output allocation was rejected");
    if (job->kind==5u) job->net_connect_result=(KuResult_net_client){false,NULL,error};
    else if (job->kind==6u) job->net_read_result=(KuResult_bytes){false,{0},error};
    else job->result=(KuResult_null){false,0,error};
  }
}
#endif

#if KU_TASK_BLOCKING_PG
static void ku_task_pg_connect_run(KuTaskBlockingJobV1* job,int cancelled);
#endif
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
    void* path=job->path;
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
#if KU_TASK_BLOCKING_DIRECTORY
    KuResult_array_str directory_result={0};
#endif
    KuTaskBlockingOutputV1 allocation={pool,job,{NULL,NULL},{0,0},KU_TASK_DRIVER_OK,NULL,0,0,0};
#if KU_TASK_BLOCKING_FILES
    KuFsAllocatorV1 allocator={&allocation,ku_task_blocking_output_allocate,ku_task_blocking_output_release};
#endif
    bool ok=false,exists_value=false;
#if KU_TASK_BLOCKING_PG
    if (kind==9u) ku_task_pg_connect_run(job,cancelled);
    else
#endif
#if KU_TASK_BLOCKING_NETWORK
    if (kind>=5u) ku_task_blocking_net_run(job,cancelled,&allocation);
    else
#endif
    {
#if KU_TASK_BLOCKING_FILES
    if (!cancelled) {
#if KU_TASK_BLOCKING_DIRECTORY
      if (kind==4u) {
        directory_result=ku_fs_read_dir_native_impl(path,&allocator);
        ok=directory_result.ok; error=directory_result.error;
      } else
#endif
      if (kind==3u) {
        exists_value=ku_fs_exists_native_impl(path);
        ok=true;
      } else {
        ok=kind==1u ? ku_fs_read_native_impl(path,&read_value,&error,&allocator)
            : ku_fs_write_native_impl(path,content,&error);
      }
      if (allocation.status!=KU_TASK_DRIVER_OK) {
        if (read_value.ptr) { ku_task_blocking_output_release(&allocation,read_value.ptr); read_value=(KuString){0}; }
        ok=false;
        error=ku_task_blocking_error(
          allocation.status==KU_TASK_DRIVER_LIMIT ? "memory_limit" : allocation.status==KU_TASK_BLOCKING_OUT_OF_MEMORY ? "out_of_memory" : allocation.status==KU_TASK_DRIVER_CLOSED ? "cancelled" : "internal_error",
          allocation.status==KU_TASK_DRIVER_LIMIT ? "blocking output exceeds runtime byte budget" : allocation.status==KU_TASK_BLOCKING_OUT_OF_MEMORY ? "blocking output allocation failed" : allocation.status==KU_TASK_DRIVER_CLOSED ? "blocking read was cancelled before buffer growth" : "blocking output accounting failed");
      }
    }
#endif
    }
    /* Tracking storage is private to this worker, never part of a Ku value.
     * The directory implementation already freed every output on failure. */
    free(allocation.entries);
    if (allocation.metadata_bytes) ku_task_blocking_output_refund(&allocation,allocation.metadata_bytes);
#if KU_TASK_BLOCKING_DIRECTORY
    if (kind==4u) {
      if (!ok) { directory_result.value=(KuArray_str){0}; directory_result.error=error; }
      directory_result.ok=ok;
    }
#endif
    /* No driver mutex, source frame, or original View storage is used here. */
    ku_task_blocking_free(path);
    ku_task_blocking_free(content.ptr);
    if (ku_task_blocking_lock(pool)) return;
    job=&pool->jobs[index];
    job->path=NULL; job->content=(KuString){0};
#if KU_TASK_BLOCKING_DIRECTORY
    if (kind==4u) job->directory_result=directory_result;
    else
#endif
    if (kind==1u) job->read_result=(KuResult_str){ok,read_value,error};
    else if (kind<5u) job->result=(KuResult_null){ok,0,error};
    job->exists_value=exists_value;
#if KU_TASK_BLOCKING_NETWORK
    if(ku_task_blocking_net_state_locked(pool,job,
#if KU_TASK_BLOCKING_PG
        job->kind==9u ||
#endif
        job->net_resolved ? KU_TASK_BLOCKING_NET_WAIT : KU_TASK_BLOCKING_DONE)!=KU_TASK_DRIVER_OK)
      job->state=KU_TASK_BLOCKING_DONE;
#else
    job->state=KU_TASK_BLOCKING_DONE;
#endif
    pool->running--;
    if (job->state==KU_TASK_BLOCKING_DONE && pool->finished!=UINT64_MAX) pool->finished++;
    /* The consumer takes this same mutex. It cannot clear/reuse the job until
     * this final notification has finished all driver/job storage accesses. */
    uint32_t wake=ku_task_driver_wake(&job->ticket);
    if (wake!=KU_TASK_DRIVER_OK) pool->fault=KU_TASK_DRIVER_INTERNAL;
    ku_task_blocking_signal(pool);
    /* The next iteration accesses pool only, never this retired job. */
  }
  worker->exited=1; pool->workers_exited++;
  ku_task_blocking_signal(pool);
  if(pool->retirement_notify) pool->retirement_notify(pool->retirement_context);
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
#if KU_TASK_BLOCKING_FILES
  size_t expected=0;
  if (ku_task_control_atomic_cas(&ku_task_blocking_base_prepared,&expected,1)) {
    ku_fs_prepare_blocking_base();
    ku_task_control_atomic_store(&ku_task_blocking_base_prepared,2);
    return KU_TASK_DRIVER_OK;
  }
  return expected==2 ? KU_TASK_DRIVER_OK : KU_TASK_DRIVER_INVALID_STATE;
#else
  return KU_TASK_DRIVER_OK;
#endif
}
static uint32_t ku_task_blocking_init(KuTaskBlockingPoolV1* pool, size_t capacity,
    size_t workers, uint64_t deadline) {
  if (!pool || !capacity || capacity>KU_TASK_DRIVER_MAX_SLOTS || !workers
      || workers>64u || deadline==UINT64_MAX)
    return KU_TASK_DRIVER_INVALID_ARGUMENT;
  if (!ku_task_frame_zero_bytes(pool,sizeof(*pool))) return KU_TASK_DRIVER_INVALID_STATE;
  uint32_t prepared=ku_task_blocking_prepare_base();
  if (prepared!=KU_TASK_DRIVER_OK) return prepared;
  pool->initialized=KU_TASK_DRIVER_STORAGE_REAPING;
  pool->capacity=capacity; pool->worker_target=workers; pool->closing=1;
  size_t fixed=ku_task_blocking_fixed_bytes(capacity);
  if(fixed==SIZE_MAX)return KU_TASK_DRIVER_LIMIT;
  pool->storage_bytes=fixed-sizeof(*pool);
  pool->storage=calloc(1,pool->storage_bytes);
  if(!pool->storage)return KU_TASK_BLOCKING_OUT_OF_MEMORY;
  pool->jobs=(KuTaskBlockingJobV1*)pool->storage;
  pool->ring=(size_t*)((uint8_t*)pool->storage+capacity*sizeof(*pool->jobs));
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
    worker->thread=(HANDLE)_beginthreadex(NULL,
#if defined(KU_RUNTIME_RESOURCE_CONFIG)
        (unsigned)ku_runtime_thread_stack_bytes(),
#else
        0,
#endif
        ku_task_blocking_thread,worker,0,NULL);
    if (!worker->thread) { status=KU_TASK_DRIVER_INTERNAL; break; }
#else
    pthread_attr_t attributes;
    if (pthread_attr_init(&attributes)) { status=KU_TASK_DRIVER_INTERNAL; break; }
#if defined(KU_RUNTIME_RESOURCE_CONFIG)
    if (pthread_attr_setstacksize(&attributes,ku_runtime_thread_stack_bytes())) {
      (void)pthread_attr_destroy(&attributes); status=KU_TASK_DRIVER_INTERNAL; break;
    }
#endif
    int creation=pthread_create(&worker->thread,&attributes,ku_task_blocking_thread,worker);
    int destroyed=pthread_attr_destroy(&attributes);
    if (creation) {
      status=KU_TASK_DRIVER_INTERNAL; break;
    }
    if (destroyed) { pool->workers_created++; status=KU_TASK_DRIVER_INTERNAL; break; }
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
  free(pool->storage);pool->storage=NULL;pool->storage_bytes=0;pool->jobs=NULL;pool->ring=NULL;
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
/* QUEUED has no OS operation or worker reference. Remove its fixed node and
 * free owned snapshots now, even when every worker is blocked elsewhere.
 * Its driver obligation remains until the sole executor consumes DONE. */
static uint32_t ku_task_blocking_cancel_queued(KuTaskBlockingPoolV1* pool,KuTaskBlockingJobV1* job,size_t index) {
  job->cancelled=1;
#if KU_TASK_BLOCKING_PG
  if(job->kind==9u) {
    job->pg_ready=1;
    if(job->state!=KU_TASK_BLOCKING_QUEUED)return KU_TASK_DRIVER_OK;
    /* A queued connect has no physical worker reference and can be detached
     * below. A running resolver keeps the original physical retirement rule. */
  }
#endif
  if(job->state!=KU_TASK_BLOCKING_QUEUED)return KU_TASK_DRIVER_OK;
  size_t offset=0;
  while(offset<pool->queued && pool->ring[(pool->head+offset)%pool->capacity]!=index)offset++;
  if(offset==pool->queued)return KU_TASK_DRIVER_INTERNAL;
  for(size_t i=offset;i+1u<pool->queued;i++)
    pool->ring[(pool->head+i)%pool->capacity]=pool->ring[(pool->head+i+1u)%pool->capacity];
  pool->queued--;
#if KU_TASK_BLOCKING_PG
  if(job->kind==9u) {
    job->pg_operation->connected=(KuPgConnectAttempt){NULL,KU_PG_CONNECT_TIMED_OUT};
    job->pg_operation->connect_ready=1;
    uint32_t status=ku_task_blocking_net_state_locked(pool,job,KU_TASK_BLOCKING_NET_WAIT);
    if(status==KU_TASK_DRIVER_OK)status=ku_task_driver_wake(&job->ticket);
    ku_task_blocking_signal(pool);return status;
  }
#endif
  ku_task_blocking_free(job->path);job->path=NULL;
  ku_task_blocking_free(job->content.ptr);job->content=(KuString){0};
  KuError error=ku_task_blocking_error("cancelled","queued operation was cancelled before execution");
#if KU_TASK_BLOCKING_NETWORK
  if(job->kind>=5u) {
    ku_task_blocking_net_drop_config(job);
    ku_task_blocking_free(job->net_write.ptr);job->net_write=(KuBytes){0};
    ku_drop_net_client(&job->net_client);
    if(job->kind==5u)job->net_connect_result=(KuResult_net_client){false,NULL,error};
    else if(job->kind==6u)job->net_read_result=(KuResult_bytes){false,{0},error};
    else job->result=(KuResult_null){false,0,error};
  } else
#endif
#if KU_TASK_BLOCKING_DIRECTORY
  if(job->kind==4u)job->directory_result=(KuResult_array_str){false,{0},error};
  else
#endif
  if(job->kind==1u)job->read_result=(KuResult_str){false,{0},error};
  else job->result=(KuResult_null){false,0,error};
  job->state=KU_TASK_BLOCKING_DONE;
  if(pool->finished!=UINT64_MAX)pool->finished++;
  uint32_t status=ku_task_driver_wake(&job->ticket);
  if(status!=KU_TASK_DRIVER_OK)pool->fault=KU_TASK_DRIVER_INTERNAL;
  ku_task_blocking_signal(pool);return status;
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
  if (status==KU_TASK_DRIVER_OK) status=ku_task_blocking_cancel_queued(pool,job,token->slot);
  ku_task_blocking_signal(pool); ku_task_blocking_unlock(pool);
  return status;
}
static uint32_t ku_task_blocking_take(const KuTaskDriverTicketV1* ticket,
    KuTaskBlockingTokenV1* token, int cancel, uint32_t kind, void* owner_output, void* output) {
  KuTaskBlockingPoolV1* pool=(KuTaskBlockingPoolV1*)token->pool;
  if (!pool || !output || pool->initialized!=KU_TASK_DRIVER_STORAGE_LIVE) return KU_TASK_DRIVER_INVALID_ARGUMENT;
#if KU_TASK_BLOCKING_NETWORK
  if ((kind==6u || kind==7u) && (!owner_output || *(KuNetClient**)owner_output)) return KU_TASK_DRIVER_INVALID_ARGUMENT;
#else
  (void)owner_output;
#endif
  if (ku_task_blocking_lock(pool)) return KU_TASK_DRIVER_INTERNAL;
  KuTaskBlockingJobV1* job=ku_task_blocking_find(pool,token);
  uint32_t status=!job || job->ticket.driver!=ticket->driver || job->ticket.slot!=ticket->slot
      || job->ticket.generation!=ticket->generation ? KU_TASK_DRIVER_STALE : KU_TASK_DRIVER_PENDING;
  if (status==KU_TASK_DRIVER_PENDING) {
    if (cancel) {
      uint32_t cancelled=ku_task_blocking_cancel_queued(pool,job,token->slot);
      if(cancelled!=KU_TASK_DRIVER_OK) {ku_task_blocking_unlock(pool);return cancelled;}
    }
    if (job->state==KU_TASK_BLOCKING_DONE) {
      if (kind!=2u && kind!=job->kind) { ku_task_blocking_unlock(pool); return KU_TASK_DRIVER_INVALID_ARGUMENT; }
      status=ku_task_driver_blocking_retired(ticket);
      if (status!=KU_TASK_DRIVER_OK) {
        pool->fault=KU_TASK_DRIVER_INTERNAL; ku_task_blocking_unlock(pool); return status;
      }
      if (kind==2u) {
#if KU_TASK_BLOCKING_PG
        if (job->kind==9u) {
          if(job->pg_result.ok)ku_drop_pg_result(&job->pg_result.value);else ku_error_drop(&job->pg_result.error);
        } else
#endif
#if KU_TASK_BLOCKING_NETWORK
        if (job->kind>=5u) {
          ku_drop_net_client(&job->net_client);
          ku_task_blocking_net_drop_result(job);
        } else
#endif
#if KU_TASK_BLOCKING_DIRECTORY
        if (job->kind==4u) ku_result_drop_array_str(&job->directory_result);
        else
#endif
        if (job->kind==1u) ku_result_drop_str(&job->read_result);
        else ku_result_drop_null(&job->result);
      }
#if KU_TASK_BLOCKING_PG
      else if (kind==9u) *(KuResult_pg_result*)output=job->pg_result;
#endif
#if KU_TASK_BLOCKING_DIRECTORY
      else if (kind==4u) *(KuResult_array_str*)output=job->directory_result;
#endif
#if KU_TASK_BLOCKING_NETWORK
      else if (kind==5u) *(KuResult_net_client*)output=job->net_connect_result;
      else if (kind>=6u) {
        if (!owner_output && kind!=8u) { ku_task_blocking_unlock(pool); return KU_TASK_DRIVER_INVALID_ARGUMENT; }
        if (owner_output) *(KuNetClient**)owner_output=ku_move_net_client(&job->net_client);
        if (kind==6u) *(KuResult_bytes*)output=job->net_read_result;
        else *(KuResult_null*)output=job->result;
      }
#endif
      else if (kind==1u) *(KuResult_str*)output=job->read_result;
      else if (kind==3u) *(KuResult_bool*)output=(KuResult_bool){job->result.ok,job->exists_value,job->result.error};
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
  return ku_task_blocking_take(ticket,token,cancel,0u,NULL,output);
}
static uint32_t ku_task_blocking_read_string(const KuTaskDriverTicketV1* ticket,
    KuTaskBlockingTokenV1* token, KuResult_str* output) {
  return ku_task_blocking_take(ticket,token,0,1u,NULL,output);
}
static uint32_t ku_task_blocking_read_bool(const KuTaskDriverTicketV1* ticket,
    KuTaskBlockingTokenV1* token, KuResult_bool* output) {
  return ku_task_blocking_take(ticket,token,0,3u,NULL,output);
}
#if KU_TASK_BLOCKING_DIRECTORY
static uint32_t ku_task_blocking_read_directory(const KuTaskDriverTicketV1* ticket,
    KuTaskBlockingTokenV1* token, KuResult_array_str* output) {
  return ku_task_blocking_take(ticket,token,0,4u,NULL,output);
}
#endif
static uint32_t ku_task_blocking_discard(const KuTaskDriverTicketV1* ticket,
    KuTaskBlockingTokenV1* token) {
  uint32_t sentinel=0; return ku_task_blocking_take(ticket,token,1,2u,NULL,&sentinel);
}
#if KU_TASK_BLOCKING_FILES
static uint32_t ku_task_blocking_submit_impl(const KuTaskDriverTicketV1* ticket,
    KuTaskControlV1* control, KuTaskBlockingTokenV1* token,
    KuString path, KuString content, void* context,
    uint32_t (*reconcile)(void*), uint32_t kind, KuResult_null* output) {
  if ((kind!=0u && kind!=1u && kind!=3u && (kind!=4u || !KU_TASK_BLOCKING_DIRECTORY)) || !ticket || !control || !token || token->pool || token->charged_bytes || !output
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
      native=ku_fs_native_path_with_allocator(path,&error,kind==4u ? "read_dir_failed" : kind ? "read_failed" : "write_failed",kind ? "failed to read: invalid path" : "failed to write: invalid path",&allocator);
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
  /* A valid exists call reports invalid native paths as false. Pool,
   * allocation and accounting failures retain their Task error payload. */
  if (kind==3u && status==KU_TASK_DRIVER_OK) {
    ku_error_drop(&error);
    *output=(KuResult_null){true,0,{0}};
  } else *output=(KuResult_null){false,0,error};
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
#if KU_TASK_BLOCKING_DIRECTORY
static uint32_t ku_task_blocking_submit_read_dir(const KuTaskDriverTicketV1* ticket,
    KuTaskControlV1* control, KuTaskBlockingTokenV1* token,
    KuString path, void* context, uint32_t (*reconcile)(void*), KuResult_array_str* output) {
  if (!output) return KU_TASK_DRIVER_INVALID_ARGUMENT;
  KuResult_null submitted={0};
  uint32_t status=ku_task_blocking_submit_impl(ticket,control,token,path,(KuString){0},context,reconcile,4u,&submitted);
  if (status==KU_TASK_DRIVER_OK) *output=(KuResult_array_str){false,{0},submitted.error};
  return status;
}
#endif
static uint32_t ku_task_blocking_submit_exists(const KuTaskDriverTicketV1* ticket,
    KuTaskControlV1* control, KuTaskBlockingTokenV1* token,
    KuString path, void* context, uint32_t (*reconcile)(void*), KuResult_bool* output) {
  if (!output) return KU_TASK_DRIVER_INVALID_ARGUMENT;
  KuResult_null submitted={0};
  uint32_t status=ku_task_blocking_submit_impl(ticket,control,token,path,(KuString){0},context,reconcile,3u,&submitted);
  if (status==KU_TASK_DRIVER_OK) *output=(KuResult_bool){submitted.ok,false,submitted.error};
  return status;
}
#endif
"#;

const NETWORK_RUNTIME: &str = r#"
/* A source frame never lends a live transport pointer to a blocking worker.
 * The job owns that pointer until DONE; cancellation may clear the NULL frame
 * slot first, then discards the real owner only after physical completion.
 * DNS and native TLS-library allocations are external to this byte ledger.
 * The bounded TLS ciphertext pending buffer is also not yet charged here. */
static void ku_task_blocking_net_error_result(uint32_t kind, void* output, KuError error) {
  if (kind==5u) *(KuResult_net_client*)output=(KuResult_net_client){false,NULL,error};
  else if (kind==6u) *(KuResult_bytes*)output=(KuResult_bytes){false,{0},error};
  else *(KuResult_null*)output=(KuResult_null){false,0,error};
}
static uint32_t ku_task_blocking_take_net(const KuTaskDriverTicketV1* ticket,
    KuTaskBlockingTokenV1* token, uint32_t kind, KuNetClient** client, void* output) {
  if (kind<5u || kind>8u) return KU_TASK_DRIVER_INVALID_ARGUMENT;
  return ku_task_blocking_take(ticket,token,0,kind,client,output);
}
static uint32_t ku_task_blocking_submit_net(const KuTaskDriverTicketV1* ticket,
    KuTaskControlV1* control, KuTaskBlockingTokenV1* token, uint32_t kind,
    const KuValue fields[9], uint16_t present, KuNetClient** client,
    KuBytes data, int64_t requested, void* context,
    uint32_t (*reconcile)(void*), void* output) {
  if (!ticket || !ticket->driver || !control || !token || token->pool || token->charged_bytes
      || kind<5u || kind>8u || !context || !reconcile || !output
      || (kind==5u && (!fields || (present&3u)!=3u || (present&~511u)))
      || (kind!=5u && !client)) return KU_TASK_DRIVER_INVALID_ARGUMENT;
  if (kind==7u && ((data.len && !data.ptr) || data.len>(size_t)KU_BYTES_MAX_LENGTH)) {
    ku_task_blocking_net_error_result(kind,output,ku_net_error("invalid_bytes","net write received invalid or oversized bytes"));
    return KU_TASK_DRIVER_OK;
  }
  if (kind==5u) for (size_t i=0;i<9u;i++) if (present & (uint16_t)(1u<<i)) {
    KuValueTag expected=(i==0u || i==7u || i==8u) ? KU_STR : i==6u ? KU_BOOL : KU_INT;
    if (fields[i].tag!=expected || (expected==KU_STR && fields[i].as.s.len && !fields[i].as.s.ptr))
      return KU_TASK_DRIVER_INVALID_ARGUMENT;
  }
  KuTaskBlockingPoolV1* pool=(KuTaskBlockingPoolV1*)ticket->driver->blocking_pool;
  if (!pool || pool->initialized!=KU_TASK_DRIVER_STORAGE_LIVE) {
    ku_task_blocking_net_error_result(kind,output,ku_task_blocking_error("blocking_pool_stopped","blocking pool is not available"));
    return KU_TASK_DRIVER_OK;
  }
  /* Capture once before queue admission: queue time consumes this same D.
   * The existing handler's TLS deadline is included by this shared helper. */
  uint32_t timeout=KU_NET_DEFAULT_TIMEOUT_MS;
  if (kind==5u && (present & 4u) && fields[2].as.i>=1 && fields[2].as.i<=KU_NET_MAX_TIMEOUT_MS)
    timeout=(uint32_t)fields[2].as.i;
  else if (kind==6u && *client) timeout=(*client)->read_timeout_ms;
  else if (kind==7u && *client) timeout=(*client)->write_timeout_ms;
  uint64_t deadline=ku_net_deadline_after_ms(timeout);
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
    ku_task_blocking_net_error_result(kind,output,ku_task_blocking_error(status==KU_TASK_DRIVER_LIMIT ? "blocking_queue_full" : "blocking_pool_stopped",
        status==KU_TASK_DRIVER_LIMIT ? "blocking queue is full" : "blocking pool is closing"));
    return KU_TASK_DRIVER_OK;
  }
  KuTaskBlockingJobV1* job=&pool->jobs[index];
  job->generation++; job->state=KU_TASK_BLOCKING_BUILDING; job->ticket=*ticket;
  job->kind=kind; job->net_deadline=deadline; job->net_requested=requested;
  *token=(KuTaskBlockingTokenV1){pool,index,0,job->generation};
  pool->resident++; if (pool->resident>pool->peak_resident) pool->peak_resident=pool->resident;
  ku_task_blocking_unlock(pool);
  status=ku_task_driver_blocking_register(ticket);
  int registered=status==KU_TASK_DRIVER_OK;
  KuTaskBlockingBuilderV1 builder={ticket,token,context,reconcile,status};
  KuValue copied[9]; memset(copied,0,sizeof(copied));
  uint8_t* write_copy=NULL;
  if (registered) {
    builder.status=reconcile(context);
    if (kind==5u) for (size_t i=0;i<9u && builder.status==KU_TASK_DRIVER_OK;i++) {
      if (!(present & (uint16_t)(1u<<i))) continue;
      copied[i]=fields[i];
      if (fields[i].tag==KU_STR) {
        KuString source=fields[i].as.s;
        copied[i].as.s=(KuString){0};
        if (source.len) {
          uint8_t* bytes=(uint8_t*)ku_task_blocking_allocate(&builder,source.len);
          if (!bytes) break;
          memcpy(bytes,source.ptr,source.len);
          copied[i].as.s=(KuString){bytes,source.len,source.len,KU_STRING_OWNED};
        }
      }
    }
    if (kind==7u && data.len && builder.status==KU_TASK_DRIVER_OK) {
      write_copy=(uint8_t*)ku_task_blocking_allocate(&builder,data.len);
      if (write_copy) memcpy(write_copy,data.ptr,data.len);
    }
  }
  status=builder.status;
  if (status!=KU_TASK_DRIVER_OK) goto ku_net_blocking_rejected;
  if (kind!=5u && *client && sizeof(KuNetClient)>SIZE_MAX-token->charged_bytes) {
    status=KU_TASK_DRIVER_LIMIT; goto ku_net_blocking_rejected;
  }
  if (ku_task_blocking_lock(pool)) return KU_TASK_DRIVER_INTERNAL;
  if (pool->closing || pool->fault || ku_task_control_atomic_load(&control->phase)!=KU_TASK_CONTROL_LIVE) {
    status=KU_TASK_DRIVER_CLOSED; ku_task_blocking_unlock(pool); goto ku_net_blocking_rejected;
  }
  job=&pool->jobs[index];
  if(ku_task_driver_lock(ticket->driver)) { ku_task_blocking_unlock(pool); return KU_TASK_DRIVER_INTERNAL; }
  KuTaskDriverSlotV1* owner_slot=ku_task_driver_find(ticket);
  if(owner_slot) job->net_owner=owner_slot->owner_worker;
  int reactor_attached=job->net_owner && (ticket->driver->external_loops || ticket->driver->io_wait);
  ku_task_driver_unlock(ticket->driver);
  job->net_plain=reactor_attached && (kind==5u ? !(present & 64u) || !fields[6].as.b : !*client || !(*client)->tls_enabled);
  status=ku_task_blocking_net_state_locked(pool,job,job->net_plain && kind!=5u
      ? KU_TASK_BLOCKING_NET_WAIT : KU_TASK_BLOCKING_QUEUED);
  if(status!=KU_TASK_DRIVER_OK) {
    ku_task_blocking_unlock(pool);goto ku_net_blocking_rejected;
  }
  if (kind==5u) {
    for(size_t i=0;i<9u;i++)if(present & (uint16_t)(1u<<i))
      ku_task_blocking_net_config_store(job,i,copied[i]);
    job->net_present=present;
  }
  else {
    /* No allocation or fallible operation after this physical owner move. The
     * existing driver reservation previously counted the very same frame owner. */
    job->net_client=ku_move_net_client(client);
    if (job->net_client) token->charged_bytes+=sizeof(KuNetClient);
    job->net_write=(KuBytes){write_copy,data.len,data.len,KU_BYTES_OWNED};
  }
  if(job->state==KU_TASK_BLOCKING_QUEUED) {
    pool->ring[(pool->head+pool->queued)%pool->capacity]=index; pool->queued++;
  }
  if (pool->queued>pool->peak_queued) pool->peak_queued=pool->queued;
  if (pool->accepted!=UINT64_MAX) pool->accepted++;
  ku_task_blocking_signal(pool);
  if(job->state==KU_TASK_BLOCKING_NET_WAIT) (void)ku_task_driver_wake(ticket);
  ku_task_blocking_unlock(pool);
  status=ku_task_driver_set_intent(ticket,KU_TASK_DRIVER_WAIT);
  return status==KU_TASK_DRIVER_OK ? KU_TASK_DRIVER_PENDING : status;
ku_net_blocking_rejected:
  for (size_t i=0;i<9u;i++) if ((present & (uint16_t)(1u<<i)) && copied[i].tag==KU_STR)
    ku_task_blocking_release(&builder,copied[i].as.s.ptr);
  ku_task_blocking_release(&builder,write_copy);
  if (builder.status!=KU_TASK_DRIVER_OK) status=builder.status;
  if (ku_task_blocking_lock(pool)) return KU_TASK_DRIVER_INTERNAL;
  job=&pool->jobs[index]; uint64_t generation=job->generation;
  memset(job,0,sizeof(*job)); job->generation=generation;
  pool->resident--; if (pool->rejected!=UINT64_MAX) pool->rejected++;
  ku_task_blocking_signal(pool); ku_task_blocking_unlock(pool);
  if (registered && ku_task_driver_blocking_retired(ticket)!=KU_TASK_DRIVER_OK) return KU_TASK_DRIVER_INTERNAL;
  *token=(KuTaskBlockingTokenV1){0};
  uint32_t reconciled=reconcile(context);
  if (reconciled!=KU_TASK_DRIVER_OK) return reconciled;
  ku_task_blocking_net_error_result(kind,output,ku_task_blocking_error(
      status==KU_TASK_DRIVER_LIMIT ? "memory_limit" : status==KU_TASK_BLOCKING_OUT_OF_MEMORY ? "out_of_memory" : "blocking_pool_stopped",
      status==KU_TASK_DRIVER_LIMIT ? "network input exceeds runtime byte budget" : status==KU_TASK_BLOCKING_OUT_OF_MEMORY ? "network input allocation failed" : "blocking pool is closing"));
  return KU_TASK_DRIVER_OK;
}
"#;
