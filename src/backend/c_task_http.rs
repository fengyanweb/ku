//! Native HTTP's opaque bridge to the fixed-owner Task driver.
use super::{output::COutput, task, unsupported};
use crate::{
    error::KuResult,
    ir::task::{TaskProgram, TaskSlotType},
};

pub(super) fn is_handler(function: &crate::ir::task::TaskFunction) -> bool {
    if !matches!(&function.result,crate::ir::IrType::Result(inner) if crate::ir::task::http_response_type(inner))
    {
        return false;
    }
    let mut parameters = function.parameters.iter();
    let first = parameters.next();
    let capture = |slot: &crate::ir::task::SlotId| matches!(&function.slots[slot.0].ty,TaskSlotType::Value {ty,borrowed:false} if crate::ir::task::pg_capture_type(ty));
    if let Some(first) = first {
        let request = matches!(&function.slots[first.0].ty,TaskSlotType::Value {ty,borrowed:false} if crate::ir::task::http_request_type(ty));
        if !request && !capture(first) {
            return false;
        }
    }
    parameters.all(capture)
}

pub(super) fn emit(
    out: &mut COutput,
    tasks: &TaskProgram,
    program: &crate::ir::IrProgram,
) -> KuResult<()> {
    out.check()?;
    let blocking = task::uses_blocking_io(tasks);
    out.push_str(
        &BRIDGE
            .replace(
                "@BLOCKING_FIELD@",
                if blocking {
                    "KuTaskBlockingPoolV1* blocking;"
                } else {
                    ""
                },
            )
            .replace(
                "@BLOCKING_PREFLIGHT@",
                if blocking {
                    " + ku_task_blocking_fixed_bytes(task_capacity)"
                } else {
                    ""
                },
            )
            .replace(
                "@BLOCKING_THREADS@",
                if blocking {
                    "return total ? 1u : 0u;"
                } else {
                    "(void)total; return 0u;"
                },
            )
            .replace("@BLOCKING_INIT@", if blocking { BLOCKING_INIT } else { "" })
            .replace("@BLOCKING_FREE@", if blocking { BLOCKING_FREE } else { "" })
            .replace(
                "@BLOCKING_METRICS@",
                if blocking {
                    BLOCKING_METRICS
                } else {
                    "(void)runtime;"
                },
            )
            .replace(
                "@BLOCKING_DISPOSE@",
                if blocking {
                    "free(runtime->blocking);"
                } else {
                    ""
                },
            ),
    );
    if task::uses_blocking_net(tasks) {
        out.push_str(r#"
static size_t ku_http_task_net_capacity(void* raw) {
  KuHttpTaskRuntime* runtime=(KuHttpTaskRuntime*)raw;
  return runtime && runtime->blocking ? runtime->blocking->capacity:0u;
}
static size_t ku_http_task_net_interests(void* raw,size_t owner,KuTaskNetPollV1* items,size_t capacity,uint64_t* deadline) {
  KuHttpTaskRuntime* runtime=(KuHttpTaskRuntime*)raw;
  return runtime ? ku_task_net_interests(runtime->blocking,owner,items,capacity,deadline):0u;
}
static uint32_t ku_http_task_net_dispatch(void* raw,size_t owner,size_t slot,uint64_t generation,uint32_t events) {
  KuHttpTaskRuntime* runtime=(KuHttpTaskRuntime*)raw;
  return runtime ? ku_task_net_dispatch(runtime->blocking,owner,slot,generation,events):KU_TASK_DRIVER_INVALID_ARGUMENT;
}
"#);
    }
    for function in &tasks.functions {
        if !crate::ir::task::http_type(&function.result) {
            continue;
        }
        if !matches!(&function.result,crate::ir::IrType::Result(inner) if crate::ir::task::http_response_type(inner))
        {
            continue;
        }
        if !is_handler(function) {
            // A helper with another signature can still return a response;
            // only an actual registered handler receives a factory symbol.
            continue;
        }
        let id = function.id.0;
        let has_request=function.parameters.first().is_some_and(|slot| matches!(&function.slots[slot.0].ty,TaskSlotType::Value {ty,..} if crate::ir::task::http_request_type(ty)));
        let mut argument = if has_request {
            ",req".to_string()
        } else {
            String::new()
        };
        let setup = program
            .functions
            .iter()
            .find(|setup| setup.name == function.name)
            .ok_or_else(|| super::unsupported("missing native async handler setup binding"))?;
        if setup.captures.len() + usize::from(has_request) != function.parameters.len() {
            return Err(super::unsupported(
                "native async handler capture arity mismatch",
            ));
        }
        let mut captures = String::new();
        let mut capture_drops = String::new();
        if !setup.captures.is_empty() {
            captures.push_str(&format!("  if(!environment) {{ ku_drop_struct___ku_http_request(req); ku_http_task_handle_release(runtime,handle); return KU_TASK_DRIVER_INVALID_ARGUMENT; }}\n  KuEnv_{}* captured=(KuEnv_{}*)environment;\n",setup.id.0,setup.id.0));
        }
        for (index, (field, ty)) in setup.captures.iter().enumerate() {
            if !matches!(ty,crate::ir::IrType::Cell(inner) if matches!(inner.as_ref(),crate::ir::IrType::Named(name) if name=="__ku_pg_client"))
            {
                return Err(super::unsupported(
                    "unsupported native async handler capture",
                ));
            }
            let field = super::c_ident(field);
            captures.push_str(&format!("  KuTaskPgCapture capture_{index}={{0}};\n  if(!ku_task_pg_capture_acquire(captured->{field}->value,environment,deadline,&capture_{index})) {{\n{capture_drops}    ku_drop_struct___ku_http_request(req); ku_http_task_handle_release(runtime,handle); return KU_TASK_DRIVER_CLOSED;\n  }}\n"));
            argument.push_str(&format!(",&capture_{index}"));
            capture_drops.push_str(&format!("  ku_task_pg_capture_drop(&capture_{index});\n"));
        }
        out.push_str(&format!(
            r#"
static uint32_t ku_http_task_start_{id}(void* raw,size_t owner,void* environment,uint64_t deadline,
    KuStruct___ku_http_request* req,void** output) {{
  KuHttpTaskRuntime* runtime=(KuHttpTaskRuntime*)raw;
  if(!runtime || !req || !output || *output || owner>=runtime->driver.workers_created)
    return KU_TASK_DRIVER_INVALID_ARGUMENT;
  KuHttpTaskHandle* handle=ku_http_task_handle_acquire(runtime,owner);
  if(!handle) {{ ku_drop_struct___ku_http_request(req); return KU_TASK_DRIVER_LIMIT; }}
  if(environment) {{ ((KuEnvHeader*)environment)->retain(environment); handle->environment=environment; }}
{captures}
  size_t previous=ku_task_driver_entry_owner;
  ku_task_driver_entry_owner=owner+1u;
  uint32_t status=ku_task_{id}_start_value(&runtime->driver{argument},&handle->value);
{capture_drops}
  ku_task_driver_entry_owner=previous;
  ku_drop_struct___ku_http_request(req);
  if(status!=KU_TASK_DRIVER_OK) {{ ku_http_task_handle_release(runtime,handle); return status; }}
  /* Language Task construction represents a never-started rejection as an
   * inline Result. HTTP admission must return that original rejection before
   * publishing an execution handle. A LIVE root's later child error continues
   * through the normal Result path and is never reclassified or replayed. */
  if(handle->value.tag==KU_TASK_VALUE_INLINE_FAILED) {{
    uint32_t rejected=handle->value.rejection_code;
    uint32_t dropped=ku_task_value_drop(&handle->value,0u);
    ku_http_task_handle_release(runtime,handle);
    return dropped==KU_TASK_DRIVER_OK ? rejected : dropped;
  }}
  *output=handle; return KU_TASK_DRIVER_OK;
}}
"#
        ));
    }
    if tasks.functions.is_empty() {
        return Err(unsupported("HTTP async bridge requires Task bodies"));
    }
    out.check()
}

const BRIDGE: &str = r#"
static uint32_t ku_http_task_start_failure_reason(uint32_t status) {
  return status==KU_TASK_DRIVER_LIMIT ? KU_HTTP_ERROR_TASK_LIMIT :
    status==KU_TASK_DRIVER_CLOSED ? KU_HTTP_ERROR_TASK_CLOSED : KU_HTTP_ERROR_DRIVER;
}
typedef struct KuHttpTaskHandle {
  KuTaskValueV1 value;
  KuTaskDriverTicketV1 retired;
  KuStruct___ku_http_response response;
  KuHttpFailure failure;
  size_t retained_bytes,owner;
  uint32_t used,finishing,cancelled;
  void* environment;
} KuHttpTaskHandle;
typedef struct KuHttpTaskRuntime {
  KuTaskDriverV1 driver;
  KuTaskDriverSlotV1* slots;
  size_t* ring;
  KuHttpTaskHandle* handles;
  size_t capacity,active_handles,extra_fixed;
  uint32_t fixed_reserved;
  @BLOCKING_FIELD@
} KuHttpTaskRuntime;
static void ku_http_task_blocking_retired(void* raw) {
  KuHttpTaskRuntime* runtime=(KuHttpTaskRuntime*)raw;
  runtime->driver.event_wake(runtime->driver.event_context,0u);
}
static size_t ku_http_task_blocking_threads(size_t total) { @BLOCKING_THREADS@ }
static void ku_http_task_blocking_metrics(void* raw,uint64_t output[16]) {
  memset(output,0,16u*sizeof(*output));
  KuHttpTaskRuntime* runtime=(KuHttpTaskRuntime*)raw;
  @BLOCKING_METRICS@
}
static KuHttpTaskHandle* ku_http_task_handle_acquire(KuHttpTaskRuntime* runtime,size_t owner) {
  if(ku_task_driver_lock(&runtime->driver)) return NULL;
  KuHttpTaskHandle* handle=NULL;
  if(!runtime->driver.closing && !runtime->driver.fault)
    for(size_t i=0;i<runtime->capacity;i++) if(!runtime->handles[i].used) {
      handle=&runtime->handles[i]; handle->used=1; handle->owner=owner;
      runtime->active_handles++; break;
    }
  ku_task_driver_unlock(&runtime->driver); return handle;
}
static void ku_http_task_handle_release(KuHttpTaskRuntime* runtime,KuHttpTaskHandle* handle) {
  if(ku_task_driver_lock(&runtime->driver)) return;
  void* environment=NULL;
  if(!handle->used || !runtime->active_handles) runtime->driver.fault=KU_TASK_DRIVER_INTERNAL;
  else { runtime->active_handles--; environment=handle->environment; memset(handle,0,sizeof(*handle)); }
  ku_task_driver_unlock(&runtime->driver);
  /* A physical retirement receipt precedes every release of a live handle.
   * Frame cancellation may already have dropped its local capture token. */
  if(environment) ((KuEnvHeader*)environment)->release(environment);
}
static void* ku_http_task_runtime_new(size_t loops,size_t capacity,size_t byte_limit,
    void* event_context,void(*event_wake)(void*,size_t),uint64_t startup_deadline) {
  if(!capacity || capacity>KU_TASK_DRIVER_MAX_SLOTS) return NULL;
  size_t task_capacity=capacity>KU_TASK_DRIVER_MAX_SLOTS/2u ? KU_TASK_DRIVER_MAX_SLOTS : capacity*2u;
  if(task_capacity<1024u)task_capacity=1024u;
  /* Exclusive setup: reject known fixed storage before any backing allocation.
     attach_blocking and each reactor still take their actual budget receipts. */
  size_t startup_fixed=sizeof(KuHttpTaskRuntime)+task_capacity*(sizeof(KuTaskDriverSlotV1)+sizeof(size_t))
      +capacity*sizeof(KuHttpTaskHandle)@BLOCKING_PREFLIGHT@;
  if(startup_fixed>byte_limit)return NULL;
  KuHttpTaskRuntime* runtime=(KuHttpTaskRuntime*)calloc(1,sizeof(*runtime));
  if(!runtime) return NULL;
  runtime->capacity=capacity;
  runtime->slots=(KuTaskDriverSlotV1*)calloc(task_capacity,sizeof(*runtime->slots));
  runtime->ring=(size_t*)calloc(task_capacity,sizeof(*runtime->ring));
  runtime->handles=(KuHttpTaskHandle*)calloc(capacity,sizeof(*runtime->handles));
  if(!runtime->slots || !runtime->ring || !runtime->handles) goto ku_http_task_init_failed;
  /* attach_blocking accounts its own fixed storage exactly once. */
  runtime->extra_fixed=sizeof(*runtime)-sizeof(runtime->driver)+capacity*sizeof(*runtime->handles);
  uint32_t status=ku_task_driver_init_external(&runtime->driver,sizeof(runtime->driver),KU_TASK_DRIVER_ABI_VERSION,
      runtime->slots,task_capacity,runtime->ring,task_capacity,
      byte_limit,loops,startup_deadline,event_context,event_wake);
  if(status!=KU_TASK_DRIVER_OK) goto ku_http_task_init_failed;
  status=ku_task_driver_scratch_reserve(&runtime->driver,runtime->extra_fixed);
  if(status!=KU_TASK_DRIVER_OK) goto ku_http_task_init_failed;
  runtime->fixed_reserved=1;
  @BLOCKING_INIT@
  return runtime;
ku_http_task_init_failed:
  /* No task was admitted and no enclosing event-loop owner was started.
   * The caller never sees partial initialization as readiness. */
  if(runtime->driver.initialized==KU_TASK_DRIVER_STORAGE_LIVE) {
    (void)ku_task_driver_request_close(&runtime->driver,startup_deadline);
    if(!ku_task_driver_lock(&runtime->driver)) {
      for(size_t i=0;i<runtime->driver.workers_created;i++) {
        runtime->driver.workers[i].state=KU_TASK_DRIVER_WORKER_EXITED;
        runtime->driver.workers_exited++;
      }
      runtime->driver.workers_waiting=0;
      ku_task_driver_unlock(&runtime->driver);
    }
    if(runtime->fixed_reserved) {
      (void)ku_task_driver_scratch_refund(&runtime->driver,runtime->extra_fixed);
      runtime->fixed_reserved=0;
    }
    (void)ku_task_driver_join(&runtime->driver,startup_deadline);
    if(ku_task_driver_destroy(&runtime->driver)!=KU_TASK_DRIVER_OK) return NULL; /* Quarantine. */
  } else if(runtime->driver.initialized!=KU_TASK_DRIVER_STORAGE_ZERO) return NULL;
  free(runtime->handles); free(runtime->ring); free(runtime->slots); free(runtime); return NULL;
}
/* Token-authenticated management reads this bounded snapshot without the
 * HTTP group mutex; driver callbacks take the group mutex in the other order. */
static void ku_http_task_driver_budget(void* raw,uint64_t output[9]) {
  memset(output,0,9u*sizeof(*output));
  KuHttpTaskRuntime* runtime=(KuHttpTaskRuntime*)raw;
  if(!runtime || ku_task_driver_lock(&runtime->driver))return;
  KuTaskDriverV1* driver=&runtime->driver;
  output[0]=driver->capacity;output[1]=driver->resident;output[2]=driver->peak_resident;
  output[3]=driver->fixed_bytes;output[4]=driver->reserved_bytes;
  output[5]=driver->peak_reserved_bytes;output[6]=driver->byte_limit;
  output[7]=driver->admission_slot_rejections;output[8]=driver->admission_byte_rejections;
  ku_task_driver_unlock(driver);
}
static uint32_t ku_http_task_fixed_reserve(void* raw,size_t bytes) {
  return ku_task_driver_scratch_reserve(&((KuHttpTaskRuntime*)raw)->driver,bytes);
}
static uint32_t ku_http_task_fixed_refund(void* raw,size_t bytes) {
  return ku_task_driver_scratch_refund(&((KuHttpTaskRuntime*)raw)->driver,bytes);
}
static uint32_t ku_http_task_pump(void* raw,size_t owner,size_t batch,uint64_t* next_deadline) {
  if(!raw) return KU_TASK_DRIVER_INVALID_ARGUMENT;
  return ku_task_driver_pump(&((KuHttpTaskRuntime*)raw)->driver,owner,batch,next_deadline);
}
static uint32_t ku_http_task_compute_slice(void* raw,size_t owner,size_t credits,
    uint64_t origin_ms,uint64_t external_deadline,int (*interrupted)(void*),void* context,
    uint64_t* next_deadline,int* force_io) {
  *force_io=0;
  if (!raw) { *next_deadline=UINT64_MAX; return KU_TASK_DRIVER_PENDING; }
  KuTaskDriverSliceResultV1 result={0};
  uint32_t status=ku_task_driver_compute_slice(&((KuHttpTaskRuntime*)raw)->driver,owner,
      credits,origin_ms,external_deadline,interrupted,context,&result);
  if (status<=KU_TASK_DRIVER_PENDING) {
    *next_deadline=result.next_deadline;
    /* IDLE with no work must permit normal sleeping after this actual probe;
     * time/wake/boundary exits require a new full I/O opportunity first. */
    *force_io=result.work_credits!=0 || result.reason!=KU_TASK_SLICE_IDLE;
  }
  return status;
}
/* Per-owner actual Task states. Queue delay begins at ready publication, not
 * HTTP acceptance or time spent waiting for a dependency. */
static void ku_http_task_metrics(void* raw,size_t owner,uint64_t output[8]) {
  memset(output,0,8u*sizeof(*output));
  KuHttpTaskRuntime* runtime=(KuHttpTaskRuntime*)raw;
  if(!runtime || owner>=runtime->driver.workers_created || ku_task_driver_lock(&runtime->driver)) return;
  KuTaskDriverWorkerV1* worker=&runtime->driver.workers[owner];
  output[0]=worker->local_queued; output[5]=worker->polls;
  output[6]=worker->peak_queue_delay_ms;
  output[7]=worker->state==KU_TASK_DRIVER_WORKER_EXITED;
  for(size_t i=0;i<runtime->driver.capacity;i++) {
    KuTaskDriverSlotV1* slot=&runtime->slots[i];
    if(slot->state==KU_TASK_DRIVER_FREE || slot->owner_worker!=owner+1u) continue;
    output[3]++;
    if(slot->state==KU_TASK_DRIVER_PARKED || slot->state==KU_TASK_DRIVER_FAULTED) output[1]++;
    if(slot->state==KU_TASK_DRIVER_RUNNING) output[2]++;
    if(slot->timer_state!=KU_TASK_DRIVER_TIMER_EMPTY) output[4]++;
  }
  ku_task_driver_unlock(&runtime->driver);
}
/* Diagnostic snapshot only. Taking it has one normal lock acquisition; the
 * current snapshot's still-held interval is included by its later unlock. */
static void ku_http_task_driver_timing(void* raw,uint64_t output[10]) {
  memset(output,0,10u*sizeof(*output));
  KuHttpTaskRuntime* runtime=(KuHttpTaskRuntime*)raw;
  if (!runtime || ku_task_driver_lock(&runtime->driver)) return;
  KuTaskDriverV1* driver=&runtime->driver;
  output[0]=driver->timing_enabled; output[1]=driver->timing_lock_count;
  output[2]=driver->timing_lock_wait_ns; output[3]=driver->timing_max_lock_wait_ns;
  output[4]=driver->timing_lock_hold_ns; output[5]=driver->timing_max_lock_hold_ns;
  output[6]=driver->timing_condition_count; output[7]=driver->timing_condition_wait_ns;
  output[8]=driver->timing_max_condition_wait_ns; output[9]=driver->timing_clock_errors;
  ku_task_driver_unlock(driver);
}
static int ku_http_task_owner_exited(void* raw,size_t owner) {
  KuHttpTaskRuntime* runtime=(KuHttpTaskRuntime*)raw;
  if(!runtime || owner>=runtime->driver.workers_created || ku_task_driver_lock(&runtime->driver)) return 0;
  int exited=runtime->driver.workers[owner].state==KU_TASK_DRIVER_WORKER_EXITED;
  ku_task_driver_unlock(&runtime->driver); return exited;
}
static uint32_t ku_http_task_poll(void* raw,void** task,int cancel,uint64_t deadline,
    KuStruct___ku_http_response* output,size_t* response_charge) {
  ku_http_failure_current=(KuHttpFailure){0};
  KuHttpTaskRuntime* runtime=(KuHttpTaskRuntime*)raw;
  if(!runtime || !task || !*task || !output || !response_charge) return KU_TASK_DRIVER_INVALID_ARGUMENT;
  *response_charge=0;
  KuHttpTaskHandle* handle=(KuHttpTaskHandle*)*task;
  uintptr_t first=(uintptr_t)runtime->handles,at=(uintptr_t)handle;
  if(at<first || at-first>=runtime->capacity*sizeof(*handle) || (at-first)%sizeof(*handle)
      || !handle->used) return KU_TASK_DRIVER_INVALID_ARGUMENT;
  if(cancel) handle->cancelled=1;
  /* This private handle still owns its lease until take/drop below. A LIVE
   * control cannot have a completed payload; observe only its atomic phase.
   * Other phases, cancellation and retirement keep the locked path. */
  if(!handle->cancelled && !handle->finishing && handle->value.tag==KU_TASK_VALUE_LIVE
      && handle->value.ticket.driver==&runtime->driver
      && handle->value.owner.lease.control
      && ku_task_control_check_lease(&handle->value.owner.lease)==KU_TASK_CONTROL_OK
      && ku_task_control_atomic_load(&handle->value.owner.lease.control->phase)==KU_TASK_CONTROL_LIVE)
    return KU_TASK_DRIVER_PENDING;
  if(!handle->finishing) {
    if(handle->cancelled) {
      handle->retired=handle->value.ticket;
      uint32_t status=ku_task_value_drop(&handle->value,deadline);
      if(status!=KU_TASK_DRIVER_OK) return status;
      handle->finishing=1;
    } else {
      handle->failure.task_slot=(uint64_t)handle->value.ticket.slot;
      handle->failure.task_generation=handle->value.ticket.generation;
      handle->failure.function_id=handle->value.function_id;
      if(handle->value.tag==KU_TASK_VALUE_INLINE_FAILED) handle->failure.driver_status=handle->value.rejection_code;
      KuTaskAdapterOutcomeV1 outcome={0};
      uint32_t status;
      if(handle->value.tag==KU_TASK_VALUE_INLINE_FAILED) status=ku_task_value_take(&handle->value,NULL,&outcome);
      else {
        /* A readiness probe must not call take_result's wrapper_end: that
         * operation schedules actual payload progress. Repeating a Pending
         * take on every HTTP turn would wake an idle timer forever. */
        if(ku_task_driver_lock(&runtime->driver)) return KU_TASK_DRIVER_INTERNAL;
        KuTaskDriverSlotV1* waiting=ku_task_driver_find(&handle->value.ticket);
        uint32_t ready=waiting ? ku_task_driver_wait_ready_locked(waiting) : KU_TASK_DRIVER_STALE;
        ku_task_driver_unlock(&runtime->driver);
        if(ready==KU_TASK_DRIVER_PENDING) return KU_TASK_DRIVER_PENDING;
        if(ready!=KU_TASK_DRIVER_WAIT_READY) return ready;
        KuTaskAdapterTakeRequestV1 request={&outcome,NULL,0u,1u};
        status=ku_task_driver_take_result(&handle->value.ticket,&handle->value.owner.lease,&request);
        if(status==KU_TASK_CONTROL_OK) handle->retained_bytes=request.retained_bytes;
      }
      if(status==KU_TASK_CONTROL_PENDING)
        return KU_TASK_DRIVER_PENDING;
      if(status!=KU_TASK_CONTROL_OK) return status;
      if(outcome.result_kind!=8u) { ku_task_outcome_drop(&outcome); return KU_TASK_DRIVER_INTERNAL; }
      handle->response=ku_http_response_from_result(ku_result_move_struct___ku_http_response(&outcome.value.http_response));
      handle->failure.reason=ku_http_failure_current.reason;
      ku_task_outcome_drop(&outcome);
      handle->retired=handle->value.ticket;
      if(handle->value.tag!=KU_TASK_VALUE_EMPTY) {
        status=ku_task_value_drop(&handle->value,deadline);
        if(status!=KU_TASK_DRIVER_OK) return status;
      }
      handle->finishing=1;
    }
  }
  if(handle->retired.driver) {
    if(ku_task_driver_lock(&runtime->driver)) return KU_TASK_DRIVER_INTERNAL;
    KuTaskDriverSlotV1* slot=ku_task_driver_find(&handle->retired);
    ku_task_driver_unlock(&runtime->driver);
    if(slot) return KU_TASK_DRIVER_PENDING;
  }
  if(handle->cancelled) { ku_drop_struct___ku_http_response(&handle->response); *output=(KuStruct___ku_http_response){0}; }
  else {
    /* The HTTP buffer budget adopts the response before its Task receipt is
     * refunded, so ownership transfer never opens an uncharged window. */
    *response_charge=ku_http_task_response_admit(runtime->driver.event_context,&handle->response,&handle->failure);
    *output=ku_move_struct___ku_http_response(&handle->response);
  }
  if(handle->retained_bytes && ku_task_driver_scratch_refund(&runtime->driver,handle->retained_bytes)!=KU_TASK_DRIVER_OK)
    return KU_TASK_DRIVER_INTERNAL;
  ku_http_failure_current=handle->failure;
  ku_http_task_handle_release(runtime,handle); *task=NULL; return KU_TASK_DRIVER_OK;
}
static uint32_t ku_http_task_runtime_close(void* raw,uint64_t deadline) {
  if(!raw) return KU_TASK_DRIVER_INVALID_ARGUMENT;
  return ku_task_driver_request_close(&((KuHttpTaskRuntime*)raw)->driver,deadline);
}
static uint32_t ku_http_task_runtime_free(void* raw,uint64_t deadline) {
  KuHttpTaskRuntime* runtime=(KuHttpTaskRuntime*)raw;
  if(!runtime) return KU_TASK_DRIVER_INVALID_ARGUMENT;
  if(ku_task_driver_lock(&runtime->driver)) return KU_TASK_DRIVER_INTERNAL;
  int pending=runtime->active_handles || runtime->driver.resident || runtime->driver.running
      || runtime->driver.workers_exited!=runtime->driver.workers_created;
  ku_task_driver_unlock(&runtime->driver);
  if(pending) return KU_TASK_DRIVER_PENDING;
  @BLOCKING_FREE@
  if(runtime->fixed_reserved) {
    uint32_t status=ku_task_driver_scratch_refund(&runtime->driver,runtime->extra_fixed);
    if(status!=KU_TASK_DRIVER_OK) return status;
    runtime->fixed_reserved=0;
  }
  uint32_t status=ku_task_driver_join(&runtime->driver,deadline);
  if(status!=KU_TASK_DRIVER_OK) return status;
  status=ku_task_driver_destroy(&runtime->driver);
  if(status!=KU_TASK_DRIVER_OK) return status;
  @BLOCKING_DISPOSE@
  free(runtime->handles); free(runtime->ring); free(runtime->slots); free(runtime); return KU_TASK_DRIVER_OK;
}
"#;

const BLOCKING_INIT: &str = r#"
  runtime->blocking=(KuTaskBlockingPoolV1*)calloc(1,sizeof(*runtime->blocking));
  if(!runtime->blocking) goto ku_http_task_init_failed;
  status=ku_task_blocking_init(runtime->blocking,runtime->driver.capacity,1u,startup_deadline);
  if(status==KU_TASK_DRIVER_OK) {
    if(ku_task_blocking_lock(runtime->blocking)) return NULL;
    runtime->blocking->retirement_context=runtime;
    runtime->blocking->retirement_notify=ku_http_task_blocking_retired;
    ku_task_blocking_unlock(runtime->blocking);
  }
  if(status==KU_TASK_DRIVER_OK) status=ku_task_driver_attach_blocking(&runtime->driver,runtime->blocking,
      sizeof(*runtime->blocking),ku_task_blocking_close,ku_task_blocking_ready,
      runtime->blocking->storage,runtime->blocking->storage_bytes);
  if(status!=KU_TASK_DRIVER_OK) {
    (void)ku_task_blocking_finish(runtime->blocking,startup_deadline);
    if(runtime->blocking->initialized!=KU_TASK_DRIVER_STORAGE_ZERO) return NULL;
    free(runtime->blocking); runtime->blocking=NULL;
    goto ku_http_task_init_failed;
  }
"#;
const BLOCKING_METRICS: &str = r#"
  if(!runtime || !runtime->blocking || ku_task_blocking_lock(runtime->blocking))return;
  KuTaskBlockingPoolV1* pool=runtime->blocking;
  output[0]=pool->resident;output[1]=pool->queued;output[2]=pool->running;
  output[3]=pool->peak_resident;output[4]=pool->peak_queued;output[5]=pool->peak_running;
  output[6]=pool->accepted;output[7]=pool->rejected;output[8]=pool->finished;
  output[10]=pool->workers_created;output[11]=pool->workers_exited;
  for(size_t i=0;i<pool->capacity;i++) {
    KuTaskBlockingJobV1* job=&pool->jobs[i];
    if(job->state==KU_TASK_BLOCKING_FREE)continue;
    output[9]+=job->output_charge;
    if(job->cancelled && job->state==KU_TASK_BLOCKING_RUNNING)output[12]++;
#if KU_TASK_BLOCKING_NETWORK
    if(job->state==KU_TASK_BLOCKING_NET_WAIT)output[13]++;
    if(job->state==KU_TASK_BLOCKING_NET_STEP)output[14]++;
#endif
  }
  ku_task_blocking_unlock(pool);
"#;
const BLOCKING_FREE: &str = r#"
  if(runtime->blocking) {
    if(ku_task_blocking_lock(runtime->blocking)) return KU_TASK_DRIVER_INTERNAL;
    int busy=runtime->blocking->resident || runtime->blocking->running || runtime->blocking->queued
        || runtime->blocking->workers_exited!=runtime->blocking->workers_created;
    ku_task_blocking_unlock(runtime->blocking);
    if(busy) return KU_TASK_DRIVER_PENDING;
    uint32_t status=ku_task_blocking_finish(runtime->blocking,deadline);
    if(status!=KU_TASK_DRIVER_OK) return status;
  }
"#;
