//! Fixed, generated source Task values and callback-only host operations.

use super::super::output::COutput;
use super::super::{c_type, unsupported};
use crate::error::KuResult;
use crate::ir::task::{TaskProgram, TaskSlotType};
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
    for function in &tasks.functions {
        out.check()?;
        out.push_str(&format!(
            "static uint32_t ku_task_{}_start_value(KuTaskDriverV1* driver",
            function.id.0
        ));
        for (index, parameter) in function.parameters.iter().enumerate() {
            let TaskSlotType::Value { ty, .. } = &function.slots[parameter.0].ty else {
                return Err(unsupported("Task parameters cannot be Task values"));
            };
            out.push_str(&format!(", {}* arg_{index}", c_type(ty)?));
        }
        out.push_str(", KuTaskValueV1* output);\n");
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
} KuTaskAdapterHostV1;
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
