//! Fixed, generated source Task values and callback-only host operations.

use super::super::output::COutput;
use super::super::unsupported;
use crate::error::KuResult;
use crate::ir::task::TaskProgram;
use crate::ir::IrType;

pub(super) fn field(ty: &IrType) -> KuResult<&'static str> {
    match ty {
        IrType::Result(inner) => field(inner),
        IrType::Int => Ok("integer"),
        IrType::Bool => Ok("boolean"),
        IrType::Null => Ok("null_value"),
        IrType::Str => Ok("string"),
        _ => Err(unsupported("unsupported Task outcome type")),
    }
}
pub(super) fn kind(ty: &IrType) -> KuResult<u32> {
    match ty {
        IrType::Result(inner) => kind(inner),
        IrType::Int => Ok(1),
        IrType::Bool => Ok(2),
        IrType::Null => Ok(3),
        IrType::Str => Ok(4),
        _ => Err(unsupported("unsupported Task outcome type")),
    }
}
pub(super) fn emit(out: &mut COutput, tasks: &TaskProgram) -> KuResult<()> {
    out.check()?;
    out.push_str(HOST_ABI);
    if super::super::task::uses_blocking_io(tasks) {
        out.push_str(BLOCKING_HOST);
    }
    for function in &tasks.functions {
        out.check()?;
        for (entry, context) in [
            ("start_value", "KuTaskDriverV1* driver"),
            ("start_hosted", "const KuTaskAdapterHostV1* host"),
        ] {
            out.push_str(&format!(
                "static uint32_t ku_task_{}_{entry}({context}",
                function.id.0
            ));
            for (index, parameter) in function.parameters.iter().enumerate() {
                out.push_str(&format!(
                    ", {}* arg_{index}",
                    super::parameter_c_type(&function.slots[parameter.0].ty)?
                ));
            }
            out.push_str(", KuTaskValueV1* output);\n");
        }
    }
    out.check()
}

const HOST_ABI: &str = r#"
/* Source Task headers are unique owners; no copied live raw headers. Outcome
 * metadata is moved under the same payload claim as the Result itself. */
enum { KU_TASK_VALUE_EMPTY=0u, KU_TASK_VALUE_LIVE=1u, KU_TASK_VALUE_INLINE_FAILED=2u,
       KU_TASK_EXIT_USER_RESULT=1u, KU_TASK_EXIT_RUNTIME_FAILURE=2u };
typedef struct KuTaskValueV1 {
  uint32_t tag, result_kind;
  uint64_t function_id;
  KuTaskControlOwnerV1 owner;
  KuTaskDriverTicketV1 ticket;
  uint32_t rejection_code;
} KuTaskValueV1;
typedef struct KuTaskAdapterOutcomeV1 {
  uint32_t result_kind, exit_class, has_cleanup_deadline;
  uint64_t cleanup_deadline;
  union { KuResult_int integer; KuResult_bool boolean;
          KuResult_null null_value; KuResult_str string; } value;
} KuTaskAdapterOutcomeV1;
typedef struct KuTaskAdapterTakeRequestV1 {
  KuTaskAdapterOutcomeV1* outcome;
  const KuTaskDriverTicketV1* parent;
} KuTaskAdapterTakeRequestV1;
typedef struct KuTaskAdapterHostV1 {
  const KuTaskDriverTicketV1* ticket;
  KuTaskControlV1* control;
  KuTaskDriverWaitTokenV1* wait;
  size_t minimum_charge; /* Generated sizeof(parent instance), not user input. */
  KuTaskDriverTimerTokenV1* timer;
  /* Private generated-host extension. All code is emitted together; a caller
   * using only older operations may leave these tail fields zero. Allocation
   * operations require the current extension size and version explicitly. */
  size_t allocation_host_bytes;
  uint32_t allocation_host_abi;
  void* allocation_context;
  uint32_t (*reconcile_charge)(void*);
  KuTaskBlockingTokenV1* blocking;
} KuTaskAdapterHostV1;
#define KU_TASK_ALLOCATION_HOST_ABI_VERSION 1u
enum { KU_TASK_HOST_ALLOCATION_FAILED = 98u };
/* Zero is a valid function ID. Initialization lives in the owner's bitmap. */
static uint32_t ku_task_callable_move(uint32_t* source) {
  uint32_t moved=*source; *source=0; return moved;
}
static uint32_t ku_task_host_clone_string(KuTaskAdapterHostV1* host,
    KuString source, KuString* output) {
  if (!host || !output || host->allocation_host_bytes!=sizeof(*host)
      || host->allocation_host_abi!=KU_TASK_ALLOCATION_HOST_ABI_VERSION
      || !host->allocation_context || !host->reconcile_charge
      || !host->ticket || !host->control) return KU_TASK_DRIVER_INVALID_ARGUMENT;
  if (source.storage==KU_STRING_STATIC) {
    if (source.capacity || (source.len && !source.ptr)) return KU_TASK_DRIVER_INVALID_ARGUMENT;
    *output=source; return KU_TASK_DRIVER_OK;
  }
  if (source.storage!=KU_STRING_OWNED || source.len>source.capacity
      || (!source.ptr && (source.len || source.capacity))) return KU_TASK_DRIVER_INVALID_ARGUMENT;
  if (!source.ptr) { *output=(KuString){0}; return KU_TASK_DRIVER_OK; }
  uint32_t status=host->reconcile_charge(host->allocation_context);
  if (status!=KU_TASK_DRIVER_OK) return status;
  size_t capacity=source.len ? source.len : 1;
  status=ku_task_driver_grow_charge(host->ticket,capacity);
  if (status!=KU_TASK_DRIVER_OK) return status;
  uint8_t* bytes=(uint8_t*)malloc(capacity);
  if (!bytes) {
    status=host->reconcile_charge(host->allocation_context);
    return status==KU_TASK_DRIVER_OK ? KU_TASK_HOST_ALLOCATION_FAILED : status;
  }
  if (source.len) memcpy(bytes,source.ptr,source.len);
  *output=(KuString){bytes,source.len,capacity,KU_STRING_OWNED};
  return KU_TASK_DRIVER_OK;
}
/* Flat local arrays share the existing typed vector ABI. The caller registers
 * the returned vector in its persistent slot before cloning any string leaf. */
static uint32_t ku_task_host_allocate_array(KuTaskAdapterHostV1* host,
    size_t count, size_t element_bytes, void** output) {
  if (!host || !output || !element_bytes || count>64u
      || host->allocation_host_bytes!=sizeof(*host)
      || host->allocation_host_abi!=KU_TASK_ALLOCATION_HOST_ABI_VERSION
      || !host->allocation_context || !host->reconcile_charge
      || !host->ticket || !host->control) return KU_TASK_DRIVER_INVALID_ARGUMENT;
  *output=NULL;
  if (!count) return KU_TASK_DRIVER_OK;
  if (count>SIZE_MAX/element_bytes) return KU_TASK_DRIVER_LIMIT;
  uint32_t status=host->reconcile_charge(host->allocation_context);
  if (status!=KU_TASK_DRIVER_OK) return status;
  size_t bytes=count*element_bytes;
  status=ku_task_driver_grow_charge(host->ticket,bytes);
  if (status!=KU_TASK_DRIVER_OK) return status;
  void* data=malloc(bytes);
  if (!data) {
    status=host->reconcile_charge(host->allocation_context);
    return status==KU_TASK_DRIVER_OK ? KU_TASK_HOST_ALLOCATION_FAILED : status;
  }
  memset(data,0,bytes); *output=data; return KU_TASK_DRIVER_OK;
}
static void ku_task_adapter_fault(KuTaskDriverV1* driver, int clock_fault);
enum { KU_TASK_HOST_TIMER_OVERFLOW = 97u };
/* One fixed registration is owned by the executing instance. A pending poll
 * reads its token, so neither the argument nor absolute deadline is renewed. */
static uint32_t ku_task_host_timer(KuTaskAdapterHostV1* host, int64_t millis,
    KuResult_null* output) {
  if (!host || !host->ticket || !host->control || !host->timer || !output)
    return KU_TASK_CONTROL_INVALID_ARGUMENT;
  if (ku_task_control_atomic_load(&host->control->phase)!=KU_TASK_CONTROL_LIVE)
    return KU_TASK_CONTROL_PENDING;
  uint32_t status;
  if (host->timer->driver) {
    status=ku_task_driver_timer_read(host->timer);
  } else {
    if (millis<0) {
      *output=(KuResult_null){false,0,ku_error_make(
        ku_string_static((const uint8_t*)"time",4),
        ku_string_static((const uint8_t*)"invalid_duration",16),
        ku_string_static((const uint8_t*)"duration must be >= 0",21))};
      return KU_TASK_CONTROL_OK;
    }
    uint64_t now=ku_task_driver_now_ms();
    if (now==UINT64_MAX) {
      ku_task_adapter_fault(host->ticket->driver,1);
      return KU_TASK_CONTROL_INVALID_STATE;
    }
    if ((uint64_t)millis>=UINT64_MAX-now) return KU_TASK_HOST_TIMER_OVERFLOW;
    status=ku_task_driver_timer_arm(host->ticket,now+(uint64_t)millis,host->timer);
  }
  if (status==KU_TASK_DRIVER_PENDING) {
    if (ku_task_driver_set_intent(host->ticket,KU_TASK_DRIVER_WAIT)!=KU_TASK_DRIVER_OK)
      return KU_TASK_CONTROL_INVALID_STATE;
    return KU_TASK_CONTROL_PENDING;
  }
  if (status==KU_TASK_DRIVER_WAIT_ABORTED) return KU_TASK_CONTROL_PENDING;
  if (status!=KU_TASK_DRIVER_WAIT_READY) return KU_TASK_CONTROL_INVALID_STATE;
  if (ku_task_driver_timer_detach(host->timer)!=KU_TASK_DRIVER_OK)
    return KU_TASK_CONTROL_INVALID_STATE;
  *output=(KuResult_null){true,0,{0}};
  return KU_TASK_CONTROL_OK;
}
static uint32_t ku_task_value_check(KuTaskValueV1* value);
static void ku_task_adapter_fault(KuTaskDriverV1* driver, int clock_fault);
static int ku_task_outcome_empty_string(KuString value) {
  return !value.ptr && !value.len && !value.capacity && !value.storage;
}
static int ku_task_outcome_empty(KuTaskAdapterOutcomeV1* output, uint32_t kind) {
  if (output->result_kind || output->exit_class || output->has_cleanup_deadline || output->cleanup_deadline) return 0;
  KuError* error;
  switch(kind) {
    case 1: if (output->value.integer.ok || output->value.integer.value) return 0; error=&output->value.integer.error; break;
    case 2: if (output->value.boolean.ok || output->value.boolean.value) return 0; error=&output->value.boolean.error; break;
    case 3: if (output->value.null_value.ok || output->value.null_value.value) return 0; error=&output->value.null_value.error; break;
    case 4: if (output->value.string.ok || !ku_task_outcome_empty_string(output->value.string.value)) return 0; error=&output->value.string.error; break;
    default: return 0;
  }
  return ku_task_outcome_empty_string(error->domain) && ku_task_outcome_empty_string(error->code) && ku_task_outcome_empty_string(error->message);
}
static void ku_task_outcome_drop(KuTaskAdapterOutcomeV1* output) {
  switch(output->result_kind) {
    case 1: ku_result_drop_int(&output->value.integer); break;
    case 2: ku_result_drop_bool(&output->value.boolean); break;
    case 3: ku_result_drop_null(&output->value.null_value); break;
    case 4: ku_result_drop_str(&output->value.string); break;
    default: break;
  }
  *output=(KuTaskAdapterOutcomeV1){0};
}
static KuError ku_task_value_error(uint32_t code) {
  const char* name = code == KU_TASK_DRIVER_CLOSED ? "runtime_stopped"
      : code == 48u ? "out_of_memory" : "too_many_tasks";
  const char* message = code == KU_TASK_DRIVER_CLOSED ? "task runtime is closing"
      : code == 48u ? "task allocation failed" : "task admission limit exceeded";
  return ku_error_make(ku_string_static((const uint8_t*)"task",4),
      ku_string_static((const uint8_t*)name,strlen(name)),
      ku_string_static((const uint8_t*)message,strlen(message)));
}
static uint32_t ku_task_value_move(KuTaskValueV1* output, KuTaskValueV1* source) {
  if (!ku_task_frame_storage_valid(output,sizeof(*output),sizeof(*output),KU_TASK_FRAME_ALIGNOF(KuTaskValueV1))
      || !ku_task_frame_storage_valid(source,sizeof(*source),sizeof(*source),KU_TASK_FRAME_ALIGNOF(KuTaskValueV1))
      || ku_task_frame_ranges_overlap(output,sizeof(*output),source,sizeof(*source))) return KU_TASK_DRIVER_INVALID_ARGUMENT;
  if (output->tag || output->owner.lease.control || output->ticket.driver) return KU_TASK_DRIVER_INVALID_STATE;
  uint32_t status=ku_task_value_check(source);
  if (status != KU_TASK_DRIVER_OK) return status;
  *output=*source; *source=(KuTaskValueV1){0};
  return KU_TASK_DRIVER_OK;
}
static uint32_t ku_task_value_drop(KuTaskValueV1* value, uint64_t deadline) {
  uint32_t checked=ku_task_value_check(value);
  if (checked != KU_TASK_DRIVER_OK) return checked;
  if (value->tag==KU_TASK_VALUE_INLINE_FAILED) { *value=(KuTaskValueV1){0}; return KU_TASK_DRIVER_OK; }
  checked=ku_task_driver_owner_drop(&value->ticket,&value->owner,deadline);
  if (checked==KU_TASK_DRIVER_OK) *value=(KuTaskValueV1){0};
  return checked;
}
static uint32_t ku_task_value_take(KuTaskValueV1* value,
    const KuTaskDriverTicketV1* parent, KuTaskAdapterOutcomeV1* output) {
  uint32_t checked=ku_task_value_check(value);
  if (checked!=KU_TASK_DRIVER_OK) return checked;
  if (!ku_task_frame_storage_valid(output,sizeof(*output),sizeof(*output),KU_TASK_FRAME_ALIGNOF(KuTaskAdapterOutcomeV1))
      || ku_task_frame_ranges_overlap(value,sizeof(*value),output,sizeof(*output))) return KU_TASK_DRIVER_INVALID_ARGUMENT;
  if (value->tag==KU_TASK_VALUE_INLINE_FAILED) {
    if (!ku_task_outcome_empty(output,value->result_kind)) return KU_TASK_DRIVER_INVALID_ARGUMENT;
    KuError error=ku_task_value_error(value->rejection_code);
    output->result_kind=value->result_kind; output->exit_class=KU_TASK_EXIT_USER_RESULT;
    switch(value->result_kind) {
      case 1: output->value.integer.error=error; break;
      case 2: output->value.boolean.error=error; break;
      case 3: output->value.null_value.error=error; break;
      case 4: output->value.string.error=error; break;
      default: return KU_TASK_DRIVER_INVALID_ARGUMENT;
    }
    *value=(KuTaskValueV1){0}; return KU_TASK_DRIVER_OK;
  }
  KuTaskAdapterTakeRequestV1 request={output,parent};
  return ku_task_driver_take_result(&value->ticket,&value->owner.lease,&request);
}
static uint64_t ku_task_host_deadline(KuTaskAdapterHostV1* host, uint64_t inherited, int create) {
  uint64_t deadline=inherited;
  if (create && deadline==UINT64_MAX) {
    uint64_t now=ku_task_driver_now_ms();
    if (now==UINT64_MAX) { ku_task_adapter_fault(host->ticket->driver,1); deadline=0; }
    else deadline=now>UINT64_MAX-1001u ? UINT64_MAX-1u : now+1000u;
  }
  size_t phase=ku_task_control_atomic_load(&host->control->phase);
  if (ku_task_control_is_requested(phase) || phase==KU_TASK_CONTROL_CANCELLED || phase==KU_TASK_CONTROL_TIMED_OUT)
    deadline=ku_task_driver_min(deadline,ku_task_control_cleanup_deadline(host->control));
  KuTaskDriverV1* driver=host->ticket->driver;
  if (!ku_task_driver_lock(driver)) {
    if (driver->closing) deadline=ku_task_driver_min(deadline,driver->shutdown_deadline);
    ku_task_driver_unlock(driver);
  } else { ku_task_adapter_fault(driver,0); deadline=0; }
  return deadline;
}
static uint32_t ku_task_host_await(KuTaskAdapterHostV1* host, KuTaskValueV1* value,
    KuTaskAdapterOutcomeV1* output) {
  if (host->wait->driver) {
    KuTaskDriverWaitSnapshotV1 snapshot={0};
    uint32_t read=ku_task_driver_wait_read(host->wait,&snapshot);
    if (read!=KU_TASK_DRIVER_OK) return read;
    if (snapshot.state==KU_TASK_DRIVER_WAIT_ARMED) {
      if (ku_task_driver_set_intent(host->ticket,KU_TASK_DRIVER_WAIT)!=KU_TASK_DRIVER_OK) return KU_TASK_DRIVER_INTERNAL;
      return KU_TASK_CONTROL_PENDING;
    }
    read=ku_task_driver_wait_detach(host->wait);
    if (read!=KU_TASK_DRIVER_OK) return read;
    if (snapshot.outcome!=KU_TASK_DRIVER_WAIT_READY && snapshot.outcome!=KU_TASK_DRIVER_WAIT_ABORTED)
      return snapshot.outcome;
  }
  if (ku_task_control_atomic_load(&host->control->phase)!=KU_TASK_CONTROL_LIVE) return KU_TASK_CONTROL_PENDING;
  uint32_t result=ku_task_value_take(value,host->ticket,output);
  if (result==KU_TASK_CONTROL_PENDING) {
    result=ku_task_driver_wait_arm(host->ticket,&value->ticket,&value->owner.lease,host->wait);
    if (result==KU_TASK_DRIVER_WAIT_READY) result=ku_task_value_take(value,host->ticket,output);
    else if (result==KU_TASK_DRIVER_PENDING) return KU_TASK_CONTROL_PENDING;
    else if (result==KU_TASK_DRIVER_WAIT_ABORTED) return KU_TASK_CONTROL_PENDING;
  }
  if (result==KU_TASK_CONTROL_CANCELLED || result==KU_TASK_CONTROL_TIMED_OUT) {
    uint64_t deadline=ku_task_control_cleanup_deadline(value->owner.lease.control);
    uint32_t requested=ku_task_driver_cancel_bound(host->ticket,result,deadline);
    if (requested!=KU_TASK_CONTROL_OK && requested!=KU_TASK_CONTROL_PENDING) return requested;
    return KU_TASK_CONTROL_PENDING;
  }
  if (result==KU_TASK_CONTROL_OK && value->tag==KU_TASK_VALUE_LIVE) {
    uint32_t dropped=ku_task_value_drop(value,ku_task_host_deadline(host,UINT64_MAX,0));
    if (dropped!=KU_TASK_DRIVER_OK) { ku_task_outcome_drop(output); return dropped; }
  }
  return result;
}
"#;

const BLOCKING_HOST: &str = r#"
static uint32_t ku_task_host_blocking_write(KuTaskAdapterHostV1* host,
    KuString path, KuString content, KuResult_null* output) {
  if (!host || !host->ticket || !host->control || !host->blocking || !output
      || host->allocation_host_bytes!=sizeof(*host)
      || host->allocation_host_abi!=KU_TASK_ALLOCATION_HOST_ABI_VERSION
      || !host->allocation_context || !host->reconcile_charge)
    return KU_TASK_CONTROL_INVALID_ARGUMENT;
  if (ku_task_control_atomic_load(&host->control->phase)!=KU_TASK_CONTROL_LIVE)
    return KU_TASK_CONTROL_PENDING;
  uint32_t status=host->blocking->pool
      ? ku_task_blocking_read(host->ticket,host->blocking,0,output)
      : ku_task_blocking_submit(host->ticket,host->control,host->blocking,
          path,content,host->allocation_context,host->reconcile_charge,output);
  if (status==KU_TASK_DRIVER_OK
      && host->reconcile_charge(host->allocation_context)!=KU_TASK_DRIVER_OK)
    return KU_TASK_CONTROL_INVALID_STATE;
  return status==KU_TASK_DRIVER_PENDING ? KU_TASK_CONTROL_PENDING
      : status==KU_TASK_DRIVER_OK ? KU_TASK_CONTROL_OK : KU_TASK_CONTROL_INVALID_STATE;
}
static uint32_t ku_task_host_blocking_read(KuTaskAdapterHostV1* host,
    KuString path, KuResult_str* output) {
  if (!host || !host->ticket || !host->control || !host->blocking || !output
      || host->allocation_host_bytes!=sizeof(*host)
      || host->allocation_host_abi!=KU_TASK_ALLOCATION_HOST_ABI_VERSION
      || !host->allocation_context || !host->reconcile_charge) return KU_TASK_CONTROL_INVALID_ARGUMENT;
  if (ku_task_control_atomic_load(&host->control->phase)!=KU_TASK_CONTROL_LIVE) return KU_TASK_CONTROL_PENDING;
  uint32_t status=host->blocking->pool
      ? ku_task_blocking_read_string(host->ticket,host->blocking,output)
      : ku_task_blocking_submit_read(host->ticket,host->control,host->blocking,path,host->allocation_context,host->reconcile_charge,output);
  /* Reconciliation follows the caller setting its Result initialization bit.
   * Until then the physical reservation remains conservatively charged. */
  return status==KU_TASK_DRIVER_PENDING ? KU_TASK_CONTROL_PENDING
      : status==KU_TASK_DRIVER_OK ? KU_TASK_CONTROL_OK : KU_TASK_CONTROL_INVALID_STATE;
}
"#;
