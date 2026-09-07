//! Generated, internal R4 typed factories over the existing R1/R2/R3 contracts.
//!
//! The bounded source Task path uses these adapters, without a manual Task API. A caller
//! exclusively owns each handle/header it passes, and every deep Owned buffer
//! remains valid, uniquely owned and disjoint from other live allocations. We
//! validate representable ranges and header aliases, not arbitrary raw pointers.
//! Source Start/Await and final scope drain reuse the fixed driver registration.
//! Active result charge follows successful hosted takes; dynamic allocators and
//! total RSS accounting remain outside this primitive-only implementation.

use crate::error::KuResult;
use crate::ir::task::{TaskFunction, TaskProgram, TaskSlotType};
use crate::ir::IrType;

use super::output::COutput;
use super::task::empty_value_expr;
use super::{c_move_value, c_type, c_type_suffix, unsupported};

#[path = "c_task_host.rs"]
mod host;

pub(super) fn emit_host(out: &mut COutput, tasks: &TaskProgram) -> KuResult<()> {
    host::emit(out, tasks)
}
pub(super) fn outcome_field(ty: &IrType) -> KuResult<&'static str> {
    host::field(ty)
}

pub(super) fn emit_adapters(out: &mut COutput, tasks: &TaskProgram) -> KuResult<()> {
    if tasks.functions.is_empty() {
        return Ok(());
    }
    out.check()?;
    out.push_str(ADAPTER_ABI);
    for function in &tasks.functions {
        out.check()?;
        emit_function(out, function)?;
    }
    emit_value_dispatch(out, tasks)?;
    out.check()
}

fn emit_value_dispatch(out: &mut COutput, tasks: &TaskProgram) -> KuResult<()> {
    out.push_str("static uint32_t ku_task_value_check(KuTaskValueV1* value) {\n  if (!ku_task_frame_storage_valid(value,sizeof(*value),sizeof(*value),KU_TASK_FRAME_ALIGNOF(KuTaskValueV1))) return KU_TASK_DRIVER_INVALID_ARGUMENT;\n  if (value->result_kind<1u || value->result_kind>4u) return KU_TASK_DRIVER_INVALID_ARGUMENT;\n  if (value->tag==KU_TASK_VALUE_INLINE_FAILED) return !value->owner.lease.control && !value->ticket.driver ? KU_TASK_DRIVER_OK : KU_TASK_DRIVER_INVALID_STATE;\n  if (value->tag!=KU_TASK_VALUE_LIVE) return KU_TASK_DRIVER_INVALID_STATE;\n  uint32_t checked=ku_task_control_check_lease(&value->owner.lease);\n  if (checked!=KU_TASK_CONTROL_OK) return checked;\n  KuTaskControlV1* control=value->owner.lease.control;\n  switch(value->function_id) {\n");
    for function in &tasks.functions {
        out.check()?;
        let id = function.id.0;
        out.push_str(&format!("  case {id}: if (value->result_kind!={}u || control->context!=(void*)control || control->operations.resume!=ku_task_{id}_resume || control->operations.take_payload!=ku_task_{id}_take_payload) return KU_TASK_DRIVER_INVALID_ARGUMENT; break;\n",host::kind(&function.result)?));
    }
    out.push_str("  default: return KU_TASK_DRIVER_INVALID_ARGUMENT;\n  }\n  checked=ku_task_driver_check_ticket(&value->ticket);\n  if (checked!=KU_TASK_DRIVER_OK) return checked;\n  KuTaskDriverV1* driver=value->ticket.driver;\n  if (ku_task_driver_lock(driver)) return KU_TASK_DRIVER_INTERNAL;\n  KuTaskDriverSlotV1* slot=ku_task_driver_find(&value->ticket);\n  checked=slot && slot->binding==control ? KU_TASK_DRIVER_OK : KU_TASK_DRIVER_STALE;\n  ku_task_driver_unlock(driver);\n  return checked;\n}\n");
    out.check()
}

fn supported_type(ty: &IrType) -> bool {
    match ty {
        IrType::Int | IrType::Bool | IrType::Null | IrType::Str => true,
        IrType::Result(inner) => matches!(
            **inner,
            IrType::Int | IrType::Bool | IrType::Null | IrType::Str
        ),
        _ => false,
    }
}

fn owned(ty: &IrType) -> bool {
    matches!(ty, IrType::Str | IrType::Result(_))
}

// The generated branch itself guards the access: inactive Result fields must
// not be validated, charged or dereferenced, even if their bytes look unusual.
fn active_strings(ty: &IrType, place: &str, body: &impl Fn(&str) -> String) -> String {
    match ty {
        IrType::Str => body(place),
        IrType::Result(inner) => format!(
            "  if (({place}).ok) {{\n{}  }} else {{\n{}{}{}  }}\n",
            active_strings(inner, &format!("({place}).value"), body),
            body(&format!("({place}).error.domain")),
            body(&format!("({place}).error.code")),
            body(&format!("({place}).error.message")),
        ),
        _ => String::new(),
    }
}

fn emit_function(out: &mut COutput, function: &TaskFunction) -> KuResult<()> {
    let IrType::Result(inner) = &function.result else {
        return Err(unsupported(
            "native task adapter requires a primitive Result",
        ));
    };
    if !supported_type(&function.result) {
        return Err(unsupported(
            "native task adapter requires a primitive Result",
        ));
    }
    let mut parameters = Vec::with_capacity(function.parameters.len());
    for slot in &function.parameters {
        let Some(slot_type) = function.slots.get(slot.0) else {
            return Err(unsupported("native task adapter parameter slot is missing"));
        };
        let TaskSlotType::Value { ty, borrowed } = &slot_type.ty else {
            return Err(unsupported("native Task parameters cannot be Task values"));
        };
        if *borrowed || !supported_type(ty) || slot.0 >= 64 {
            return Err(unsupported("native task adapter parameter is unsupported"));
        }
        parameters.push((slot.0, ty));
    }

    let id = function.id.0.to_string();
    let result_type = c_type(&function.result)?;
    let mut declarations = String::new();
    let mut arguments = String::new();
    let mut preflight = String::new();
    let mut charge = String::new();
    let mut restore = String::new();
    let mut rejected_drops = String::new();
    for (index, (slot, ty)) in parameters.iter().enumerate() {
        let c_ty = c_type(ty)?;
        declarations.push_str(&format!(", {c_ty}* arg_{index}"));
        arguments.push_str(&format!(", arg_{index}"));
        preflight.push_str(&format!(
            "  if (!ku_task_driver_external_storage(driver, arg_{index}, sizeof(*arg_{index}), KU_TASK_FRAME_ALIGNOF({c_ty}))\n\
             || ku_task_frame_ranges_overlap(output, sizeof(*output), arg_{index}, sizeof(*arg_{index}))) return KU_TASK_DRIVER_INVALID_ARGUMENT;\n"
        ));
        for (other, (_, other_ty)) in parameters.iter().enumerate().take(index) {
            if owned(ty) || owned(other_ty) {
                preflight.push_str(&format!(
                    "  if (ku_task_frame_ranges_overlap(arg_{index}, sizeof(*arg_{index}), arg_{other}, sizeof(*arg_{other}))) return KU_TASK_DRIVER_INVALID_ARGUMENT;\n"
                ));
            }
        }
        charge.push_str(&active_strings(ty, &format!("(*arg_{index})"), &|place| {
            format!(
                "  checked = ku_task_adapter_string_charge({place}, &charge);\n\
                 if (checked != KU_TASK_DRIVER_OK) return checked;\n"
            )
        }));
        // Also prevent a move/handle publication from overwriting a buffer that
        // is being moved. This is cheap and bounded by the 64-slot IR limit.
        charge.push_str(&active_strings(ty, &format!("(*arg_{index})"), &|place| {
            let mut checks = format!(
                "  if (ku_task_adapter_string_overlaps({place}, output, sizeof(*output))\n\
                 || ku_task_adapter_string_overlaps_runtime({place}, driver)"
            );
            for other in 0..parameters.len() {
                checks.push_str(&format!(
                    "\n || ku_task_adapter_string_overlaps({place}, arg_{other}, sizeof(*arg_{other}))"
                ));
            }
            checks.push_str(") return KU_TASK_DRIVER_INVALID_ARGUMENT;\n");
            checks
        }));
        if owned(ty) {
            rejected_drops.push_str(&format!(
                "  {}\n",
                super::task::drop_statement(ty, &format!("(*arg_{index})"))?
            ));
            restore.push_str(&format!(
                "  *arg_{index} = {};\n\
                 instance->frame.header.initialized &= ~(UINT64_C(1) << {slot});\n",
                c_move_value(ty, &format!("instance->frame.s_{slot}"))?
            ));
        }
    }
    let payload_aliases = active_strings(&function.result, "instance->payload", &|place| {
        format!(
            "  if (ku_task_adapter_string_overlaps({place}, outcome, sizeof(*outcome)) || ku_task_adapter_string_overlaps({place}, request, sizeof(*request)) || (request->parent && ku_task_adapter_string_overlaps({place},request->parent,sizeof(*request->parent)))) return KU_TASK_CONTROL_INVALID_ARGUMENT;\n"
        )
    });
    let request_aliases = active_strings(&function.result, "instance->payload", &|place| {
        format!("  if (ku_task_adapter_string_overlaps({place},request,sizeof(*request))) return KU_TASK_CONTROL_INVALID_ARGUMENT;\n")
    });
    let payload_charge = active_strings(&function.result, "instance->payload", &|place| {
        format!("  checked=ku_task_adapter_string_charge({place},&payload_charge); if (checked!=KU_TASK_DRIVER_OK) return checked;\n")
    });
    let mut task_mask = 0u64;
    let mut task_count = 0usize;
    let mut transfers = String::new();
    for (index, slot) in function.slots.iter().enumerate() {
        if matches!(slot.ty, TaskSlotType::Task { .. }) {
            task_mask |= 1u64 << index;
            transfers.push_str(&format!("  if (instance->frame.header.initialized & (UINT64_C(1)<<{index})) {{\n    KuTaskValueV1* child=&instance->frame.s_{index};\n    uint32_t moved=ku_task_value_check(child);\n    if (moved!=KU_TASK_DRIVER_OK) return moved;\n    if (child->tag==KU_TASK_VALUE_LIVE) {{\n      moved=ku_task_driver_owner_drop_receipt(&child->ticket,&child->owner,instance->drain_deadline,&instance->receipts[{task_count}]);\n      if (moved!=KU_TASK_DRIVER_OK) return moved;\n      instance->receipt_mask |= UINT64_C(1)<<{task_count};\n    }}\n    *child=(KuTaskValueV1){{0}};\n    instance->frame.header.initialized &= ~(UINT64_C(1)<<{index});\n  }}\n"));
            task_count += 1;
        }
    }
    let source = ADAPTER_FUNCTION
        .replace("@ID@", &id)
        .replace("@RESULT@", &result_type)
        .replace("@SUFFIX@", &c_type_suffix(inner)?)
        .replace("@PARAM_DECLS@", &declarations)
        .replace("@PARAM_ARGS@", &arguments)
        .replace("@PREFLIGHT@", &preflight)
        .replace("@CHARGE@", &charge)
        .replace("@RESTORE@", &restore)
        .replace("@REJECTED_DROPS@", &rejected_drops)
        .replace("@PAYLOAD_CHARGE@", &payload_charge)
        .replace("@FIELD@", host::field(&function.result)?)
        .replace("@KIND@", &host::kind(&function.result)?.to_string())
        .replace("@TASK_MASK@", &task_mask.to_string())
        .replace("@TASK_COUNT@", &task_count.to_string())
        .replace("@RECEIPT_SIZE@", &task_count.max(1).to_string())
        .replace("@TRANSFERS@", &transfers)
        .replace("@PAYLOAD_ALIASES@", &payload_aliases)
        .replace("@REQUEST_ALIASES@", &request_aliases)
        .replace(
            "@EMPTY_RESULT@",
            &empty_value_expr(&function.result, "(*output)")?,
        );
    out.push_str(&source);
    out.check()
}

const ADAPTER_ABI: &str = r#"
/* Internal typed adapter ABI; not source Task syntax or a stable C FFI.
 * Handle/header calls are exclusive, not concurrently copied raw owner calls.
 * A valid deep Owned allocation is unique; integer-range checks cannot prove
 * that an arbitrary C pointer is mapped, writable or genuinely owns malloc.
 * All argument/result/handle headers are complete independent caller-owned
 * objects, disjoint from Task-owned deep allocations and alive through calls.
 * A live handle remains valid until a successful move or owner transfer. The
 * known start/take buffer checks are extra guards, not a general alias oracle:
 * move must not inspect a concurrently running frame to guess its deep ranges.
 * Scope drop transfers ownership to R3; it is not a cleanup/retirement ACK.
 * take is a single nonblocking attempt; hosted Await registers the fixed link.
 */
enum { KU_TASK_ADAPTER_OUT_OF_MEMORY = 48u };
static uint32_t ku_task_adapter_frame_status(uint32_t status) {
  switch (status) {
    case KU_TASK_FRAME_OK: return KU_TASK_CONTROL_OK;
    case KU_TASK_FRAME_ABI_MISMATCH: return KU_TASK_CONTROL_ABI_MISMATCH;
    case KU_TASK_FRAME_INVALID_STORAGE:
    case KU_TASK_FRAME_INVALID_ARGUMENT: return KU_TASK_CONTROL_INVALID_ARGUMENT;
    case KU_TASK_FRAME_LIMIT: return KU_TASK_CONTROL_LIMIT;
    default: return KU_TASK_CONTROL_INVALID_STATE;
  }
}
static size_t ku_task_adapter_string_extent(KuString value) {
  return value.storage == KU_STRING_OWNED && value.ptr
      ? (value.capacity ? value.capacity : 1) : value.len;
}
static uint32_t ku_task_adapter_string_charge(KuString value, size_t* charge) {
  if (value.storage == KU_STRING_STATIC) {
    if (value.capacity || (value.len && !value.ptr)) return KU_TASK_DRIVER_INVALID_ARGUMENT;
  } else if (value.storage == KU_STRING_OWNED) {
    if (value.len > value.capacity || (!value.ptr && (value.len || value.capacity)))
      return KU_TASK_DRIVER_INVALID_ARGUMENT;
    /* Existing concat(empty,empty) owns malloc(1) with len/capacity both zero. */
    size_t bytes = value.ptr ? (value.capacity ? value.capacity : 1) : 0;
    if (bytes > SIZE_MAX - *charge) return KU_TASK_DRIVER_LIMIT;
    *charge += bytes;
  } else return KU_TASK_DRIVER_INVALID_ARGUMENT;
  size_t extent = ku_task_adapter_string_extent(value);
  if (extent && (!value.ptr || extent > UINTPTR_MAX - (uintptr_t)value.ptr))
    return KU_TASK_DRIVER_INVALID_ARGUMENT;
  return KU_TASK_DRIVER_OK;
}
static int ku_task_adapter_string_overlaps(KuString value, const void* pointer, size_t bytes) {
  size_t extent = ku_task_adapter_string_extent(value);
  return extent && ku_task_frame_ranges_overlap(value.ptr, extent, pointer, bytes);
}
static int ku_task_adapter_string_overlaps_runtime(KuString value, KuTaskDriverV1* driver) {
  return ku_task_adapter_string_overlaps(value, driver, sizeof(*driver))
      || ku_task_adapter_string_overlaps(value, driver->slots, driver->capacity * sizeof(*driver->slots))
      || ku_task_adapter_string_overlaps(value, driver->ring, driver->capacity * sizeof(*driver->ring));
}
static void ku_task_adapter_fault(KuTaskDriverV1* driver, int clock_fault) {
  /* Callbacks run outside the queue mutex. The registered task or BUILDING
   * reservation protects driver storage while recording a sticky fault. */
  if (ku_task_driver_lock(driver)) return;
  if (clock_fault) ku_task_driver_enter_clock_fault(driver);
  else { driver->fault = KU_TASK_DRIVER_INTERNAL; ku_task_driver_signal(driver); }
  ku_task_driver_unlock(driver);
}
typedef struct KuTaskAdapterClockV1 {
  KuTaskDriverV1* driver;
  KuTaskControlV1* budget;
} KuTaskAdapterClockV1;
static uint64_t ku_task_adapter_now(void* raw) {
  KuTaskAdapterClockV1* clock = (KuTaskAdapterClockV1*)raw;
  uint64_t now = ku_task_driver_now_ms();
  if (now == UINT64_MAX) {
    ku_task_adapter_fault(clock->driver, 1);
    return UINT64_MAX;
  }
  /* Re-read the live minimum at EVERY cleanup safepoint. Do not mutate the
   * frame deadline from this callback: C comparison operand order is unspecified.
   * Returning MAX forces the existing frame drop-glue path after a tightening. */
  if (clock->budget && now >= ku_task_control_cleanup_deadline(clock->budget))
    return UINT64_MAX;
  return now;
}
static uint32_t ku_task_adapter_rollback(KuTaskDriverTicketV1* ticket, uint32_t failure) {
  uint32_t result = ku_task_driver_rollback(ticket);
  if (result == KU_TASK_DRIVER_OK) return failure;
  /* A broken OS mutex may prevent budget return. Keep the actual reservation
   * visible as a fault; do not forge zero accounting or terminate the process. */
  ku_task_adapter_fault(ticket->driver, 0);
  return KU_TASK_DRIVER_INTERNAL;
}
"#;

const ADAPTER_FUNCTION: &str = r#"
typedef struct KuTaskInstance_@ID@ {
  KuTaskControlV1 control;
  KuTaskFrame_@ID@ frame;
  @RESULT@ payload;
  KuTaskDriverTicketV1 ticket;
  bool frame_initialized, payload_initialized, registered;
  uint32_t exit_class, has_cleanup_deadline;
  uint64_t cleanup_deadline;
  uint32_t drain_started, values_cleaned, drain_failure;
  uint64_t drain_deadline, receipt_mask;
  uint32_t drain_deadline_published;
  uint64_t drain_published_deadline;
  KuTaskDriverCleanupReceiptV1 receipts[@RECEIPT_SIZE@];
  KuTaskDriverWaitTokenV1 wait;
} KuTaskInstance_@ID@;
typedef struct KuTaskHandle_@ID@ {
  KuTaskControlOwnerV1 owner;
  KuTaskDriverTicketV1 ticket;
} KuTaskHandle_@ID@;

static uint32_t ku_task_@ID@_drain(KuTaskInstance_@ID@* instance, uint32_t reason, KuTaskControlV1* budget) {
  KuTaskAdapterHostV1 host={&instance->ticket,&instance->control,&instance->wait};
  if (instance->wait.driver) {
    KuTaskDriverWaitSnapshotV1 snapshot={0};
    uint32_t status=ku_task_driver_wait_read(&instance->wait,&snapshot);
    if (status!=KU_TASK_DRIVER_OK) return status;
    if (snapshot.kind==KU_TASK_DRIVER_WAIT_KIND_RESULT) {
      if (!reason) return KU_TASK_DRIVER_INTERNAL;
      status=ku_task_driver_wait_detach(&instance->wait);
      if (status!=KU_TASK_DRIVER_OK) return status;
    }
  }
  if (!instance->drain_started && ((instance->frame.header.initialized & UINT64_C(@TASK_MASK@)) || instance->receipt_mask)) {
    instance->drain_deadline=ku_task_host_deadline(&host,instance->has_cleanup_deadline ? instance->cleanup_deadline : UINT64_MAX,1);
    instance->drain_started=1;
  }
  if (instance->drain_started) instance->drain_deadline=ku_task_host_deadline(&host,instance->drain_deadline,0);
@TRANSFERS@
  /* Every sibling was durably transferred before any Value cleanup/ACK wait. */
  if (instance->drain_started && (!instance->drain_deadline_published
      || instance->drain_deadline < instance->drain_published_deadline)) {
    /* A later ancestor cancellation can tighten an already-transferred scope.
     * Receipts protect logical identity, and the driver uses its live registry
     * lease: never reconstruct a child owner or renew its original reason. */
    for (size_t i=0;i<@TASK_COUNT@u;i++) if (instance->receipt_mask & (UINT64_C(1)<<i)) {
      uint32_t status=ku_task_driver_cancel_receipt(&instance->receipts[i],instance->drain_deadline);
      if (status==KU_TASK_DRIVER_CLEANUP_ACK) instance->receipt_mask &= ~(UINT64_C(1)<<i);
      else if (status!=KU_TASK_DRIVER_OK) { instance->drain_failure=status; return status; }
    }
    instance->drain_published_deadline=instance->drain_deadline;
    instance->drain_deadline_published=1;
  }
  if (reason && !instance->values_cleaned && instance->frame_initialized) {
    KuTaskAdapterClockV1 bridge={instance->ticket.driver,budget};
    KuTaskFrameClockV1 clock={ku_task_adapter_now,&bridge,&host};
    uint32_t status;
    if (instance->frame.header.status==KU_TASK_FRAME_READY) {
      status=ku_task_frame_@ID@_destroy(&instance->frame,sizeof(instance->frame),KU_TASK_FRAME_ABI_VERSION);
      if (status!=KU_TASK_FRAME_OK) return KU_TASK_DRIVER_INTERNAL;
      instance->frame_initialized=false;
    } else {
      uint32_t frame_reason=reason==KU_TASK_CONTROL_CANCELLED ? KU_TASK_FRAME_CANCELLED : KU_TASK_FRAME_TIMED_OUT;
      uint64_t value_deadline=ku_task_control_cleanup_deadline(budget);
      if (instance->drain_started) value_deadline=ku_task_driver_min(value_deadline,instance->drain_deadline);
      status=ku_task_frame_@ID@_terminate(&instance->frame,sizeof(instance->frame),KU_TASK_FRAME_ABI_VERSION,frame_reason,value_deadline,&clock);
      if (status!=KU_TASK_FRAME_CANCELLED && status!=KU_TASK_FRAME_TIMED_OUT) return KU_TASK_DRIVER_INTERNAL;
    }
    instance->values_cleaned=1;
  }
  if (instance->wait.driver) {
    KuTaskDriverWaitSnapshotV1 snapshot={0};
    uint32_t status=ku_task_driver_wait_read(&instance->wait,&snapshot);
    if (status!=KU_TASK_DRIVER_OK) return status;
    if (snapshot.state==KU_TASK_DRIVER_WAIT_ARMED) {
      if (ku_task_driver_set_intent(&instance->ticket,KU_TASK_DRIVER_WAIT)!=KU_TASK_DRIVER_OK) return KU_TASK_DRIVER_INTERNAL;
      return KU_TASK_DRIVER_PENDING;
    }
    status=ku_task_driver_wait_detach(&instance->wait);
    if (status!=KU_TASK_DRIVER_OK) return status;
    if (snapshot.outcome!=KU_TASK_DRIVER_CLEANUP_ACK) instance->drain_failure=snapshot.outcome;
  }
  if (instance->drain_failure) return instance->drain_failure;
  /* Bounded by declared Task slots; no repeated polling or recursive child drive. */
  for (size_t i=0;i<@TASK_COUNT@u;i++) if (instance->receipt_mask & (UINT64_C(1)<<i)) {
    uint32_t status=ku_task_driver_cleanup_receipt_read(&instance->receipts[i]);
    if (status==KU_TASK_DRIVER_CLEANUP_ACK) { instance->receipt_mask &= ~(UINT64_C(1)<<i); continue; }
    if (status!=KU_TASK_DRIVER_PENDING) { instance->drain_failure=status; return status; }
  }
  if (!instance->receipt_mask) return KU_TASK_DRIVER_OK;
  for (size_t i=0;i<@TASK_COUNT@u;i++) if (instance->receipt_mask & (UINT64_C(1)<<i)) {
    uint32_t status=ku_task_driver_cleanup_wait_arm(&instance->ticket,&instance->receipts[i],instance->drain_deadline,&instance->wait);
    if (status==KU_TASK_DRIVER_CLEANUP_ACK) { instance->receipt_mask &= ~(UINT64_C(1)<<i); continue; }
    if (status==KU_TASK_DRIVER_PENDING) return status;
    instance->drain_failure=status; return status;
  }
  return KU_TASK_DRIVER_OK;
}
static uint32_t ku_task_@ID@_resume(void* raw) {
  KuTaskInstance_@ID@* instance = (KuTaskInstance_@ID@*)raw;
  if (!instance->frame_initialized) return KU_TASK_CONTROL_INVALID_STATE;
  KuTaskAdapterClockV1 bridge = { instance->ticket.driver, NULL };
  KuTaskAdapterHostV1 host={&instance->ticket,&instance->control,&instance->wait};
  KuTaskFrameClockV1 clock = { ku_task_adapter_now, &bridge, &host };
  uint32_t status=KU_TASK_FRAME_READY;
  if (!instance->payload_initialized) {
    if (ku_task_driver_set_intent(&instance->ticket,KU_TASK_DRIVER_YIELD)!=KU_TASK_DRIVER_OK) return KU_TASK_CONTROL_INVALID_STATE;
    status = ku_task_frame_@ID@_resume(&instance->frame, sizeof(instance->frame), KU_TASK_FRAME_ABI_VERSION, &clock);
  }
  if (status == KU_TASK_FRAME_PENDING) {
    return KU_TASK_CONTROL_PENDING;
  } else if (status == KU_TASK_FRAME_READY && !instance->payload_initialized) {
    status = ku_task_frame_@ID@_take_result(&instance->frame, sizeof(instance->frame), KU_TASK_FRAME_ABI_VERSION, &instance->payload);
    if (status == KU_TASK_FRAME_OK) {
      instance->payload_initialized = true;
      instance->exit_class=instance->frame.header.exit_class ? instance->frame.header.exit_class : KU_TASK_EXIT_USER_RESULT;
      instance->has_cleanup_deadline=instance->frame.header.has_exit_deadline;
      instance->cleanup_deadline=instance->frame.header.exit_deadline;
    }
  }
  if (instance->payload_initialized) {
    status=ku_task_@ID@_drain(instance,0,NULL);
    if (status==KU_TASK_DRIVER_PENDING) return KU_TASK_CONTROL_PENDING;
    if (status==KU_TASK_DRIVER_CLEANUP_TIMEOUT) {
      ku_result_drop_@SUFFIX@(&instance->payload);
      instance->payload.error=ku_error_make(ku_string_static((const uint8_t*)"task",4),ku_string_static((const uint8_t*)"shutdown_timeout",16),ku_string_static((const uint8_t*)"owned child cleanup deadline expired",36));
      instance->exit_class=KU_TASK_EXIT_RUNTIME_FAILURE;
      instance->has_cleanup_deadline=1; instance->cleanup_deadline=instance->drain_deadline;
      return KU_TASK_CONTROL_FAILED;
    }
    if (status==KU_TASK_DRIVER_OK) return instance->payload.ok ? KU_TASK_CONTROL_COMPLETED : KU_TASK_CONTROL_FAILED;
  }
  ku_task_adapter_fault(instance->ticket.driver, 0);
  return KU_TASK_CONTROL_INVALID_STATE;
}
static uint32_t ku_task_@ID@_cleanup(void* raw, uint32_t reason, KuTaskControlV1* budget) {
  KuTaskInstance_@ID@* instance = (KuTaskInstance_@ID@*)raw;
  if (instance->payload_initialized) { instance->payload_initialized=false; ku_result_drop_@SUFFIX@(&instance->payload); }
  uint32_t frame_reason = reason == KU_TASK_CONTROL_CANCELLED ? KU_TASK_FRAME_CANCELLED
      : reason == KU_TASK_CONTROL_TIMED_OUT ? KU_TASK_FRAME_TIMED_OUT : KU_TASK_FRAME_INVALID_ARGUMENT;
  if (frame_reason == KU_TASK_FRAME_INVALID_ARGUMENT) return KU_TASK_CONTROL_INVALID_ARGUMENT;
  uint32_t status=ku_task_@ID@_drain(instance,reason,budget);
  if (status==KU_TASK_DRIVER_OK || status==KU_TASK_DRIVER_CLEANUP_TIMEOUT) return KU_TASK_CONTROL_OK;
  if (status==KU_TASK_DRIVER_PENDING) return KU_TASK_CONTROL_PENDING;
  /* READY is only possible when R2 already staged/dropped the frame itself;
   * it must never be reinterpreted here as cancellation cleanup success. */
  ku_task_adapter_fault(instance->ticket.driver, 0);
  return KU_TASK_CONTROL_INVALID_STATE;
}
static void ku_task_@ID@_drop_frame(void* raw) {
  KuTaskInstance_@ID@* instance = (KuTaskInstance_@ID@*)raw;
  if (!instance->frame_initialized) return;
  uint32_t status = ku_task_frame_@ID@_destroy(&instance->frame, sizeof(instance->frame), KU_TASK_FRAME_ABI_VERSION);
  if (status != KU_TASK_FRAME_OK) { ku_task_adapter_fault(instance->ticket.driver, 0); return; }
  instance->frame_initialized = false;
}
static void ku_task_@ID@_drop_payload(void* raw) {
  KuTaskInstance_@ID@* instance = (KuTaskInstance_@ID@*)raw;
  if (instance->payload_initialized) {
    instance->payload_initialized = false;
    ku_result_drop_@SUFFIX@(&instance->payload);
  }
}
static uint32_t ku_task_@ID@_take_payload(void* raw, void* destination) {
  KuTaskInstance_@ID@* instance = (KuTaskInstance_@ID@*)raw;
  KuTaskAdapterTakeRequestV1* request=(KuTaskAdapterTakeRequestV1*)destination;
  if (!ku_task_driver_external_storage(instance->ticket.driver,request,sizeof(*request),KU_TASK_FRAME_ALIGNOF(KuTaskAdapterTakeRequestV1))
      || ku_task_frame_ranges_overlap(instance,sizeof(*instance),request,sizeof(*request))) return KU_TASK_CONTROL_INVALID_ARGUMENT;
  if (!instance->payload_initialized) return KU_TASK_CONTROL_RESULT_TAKEN;
@REQUEST_ALIASES@
  KuTaskAdapterOutcomeV1* outcome=request->outcome;
  if (!ku_task_driver_external_storage(instance->ticket.driver,outcome,sizeof(*outcome),KU_TASK_FRAME_ALIGNOF(KuTaskAdapterOutcomeV1))
      || ku_task_frame_ranges_overlap(instance,sizeof(*instance),outcome,sizeof(*outcome))
      || ku_task_frame_ranges_overlap(request,sizeof(*request),outcome,sizeof(*outcome))) return KU_TASK_CONTROL_INVALID_ARGUMENT;
  if (request->parent && (!ku_task_driver_external_storage(instance->ticket.driver,request->parent,sizeof(*request->parent),KU_TASK_FRAME_ALIGNOF(KuTaskDriverTicketV1))
      || ku_task_frame_ranges_overlap(request->parent,sizeof(*request->parent),outcome,sizeof(*outcome))
      || ku_task_frame_ranges_overlap(request->parent,sizeof(*request->parent),request,sizeof(*request)))) return KU_TASK_CONTROL_INVALID_ARGUMENT;
  @RESULT@* output=&outcome->value.@FIELD@;
@PAYLOAD_ALIASES@
  /* Reject deep alias before reading a typed header from that address: the
   * aliased allocation itself may be smaller than sizeof(*output). */
  if (outcome->result_kind || outcome->exit_class || outcome->has_cleanup_deadline || outcome->cleanup_deadline || !(@EMPTY_RESULT@)) return KU_TASK_CONTROL_INVALID_ARGUMENT;
  size_t payload_charge=0;
  uint32_t checked=KU_TASK_DRIVER_OK;
@PAYLOAD_CHARGE@
  if (request->parent) {
    checked=ku_task_driver_transfer_charge(&instance->ticket,request->parent,payload_charge,sizeof(*instance));
    if (checked!=KU_TASK_DRIVER_OK) return checked;
  }
  *output = ku_result_move_@SUFFIX@(&instance->payload);
  outcome->result_kind=@KIND@u; outcome->exit_class=instance->exit_class;
  outcome->has_cleanup_deadline=instance->has_cleanup_deadline; outcome->cleanup_deadline=instance->cleanup_deadline;
  instance->payload_initialized = false;
  return KU_TASK_CONTROL_OK;
}
static void ku_task_@ID@_dispose(KuTaskControlV1* control, void* raw) {
  KuTaskInstance_@ID@* instance = (KuTaskInstance_@ID@*)raw;
  (void)control;
  KuTaskDriverTicketV1 ticket = instance->ticket;
  bool registered = instance->registered;
  free(instance);
  /* Last access to the instance was BEFORE free. Slot/bytes remain reserved
   * until this separate ticket acknowledgement, including late observer refs. */
  if (registered && ku_task_driver_disposed(&ticket) != KU_TASK_DRIVER_OK)
    ku_task_adapter_fault(ticket.driver, 0);
}
static const KuTaskControlOpsV1 ku_task_@ID@_ops = {
  ku_task_@ID@_resume, ku_task_@ID@_cleanup, ku_task_@ID@_drop_frame,
  ku_task_@ID@_drop_payload, ku_task_@ID@_take_payload, ku_task_@ID@_dispose
};
static uint32_t ku_task_@ID@_check(KuTaskHandle_@ID@* handle) {
  if (!ku_task_frame_storage_valid(handle, sizeof(*handle), sizeof(*handle), KU_TASK_FRAME_ALIGNOF(KuTaskHandle_@ID@)))
    return KU_TASK_DRIVER_INVALID_ARGUMENT;
  uint32_t checked = ku_task_driver_check_ticket(&handle->ticket);
  if (checked != KU_TASK_DRIVER_OK) return checked;
  if (!ku_task_driver_external_storage(handle->ticket.driver, handle, sizeof(*handle), KU_TASK_FRAME_ALIGNOF(KuTaskHandle_@ID@)))
    return KU_TASK_DRIVER_INVALID_ARGUMENT;
  checked = ku_task_control_check_lease(&handle->owner.lease);
  if (checked != KU_TASK_CONTROL_OK) return checked;
  KuTaskControlV1* control = handle->owner.lease.control;
  /* Immutable callback identity validates the concrete layout before its cast.
   * The first instance field is control, so context must have that same address. */
  if (control->context != (void*)control || control->operations.resume != ku_task_@ID@_resume
      || control->operations.take_payload != ku_task_@ID@_take_payload || control->operations.dispose != ku_task_@ID@_dispose)
    return KU_TASK_DRIVER_INVALID_ARGUMENT;
  KuTaskInstance_@ID@* instance = (KuTaskInstance_@ID@*)control;
  if (!ku_task_driver_external_storage(handle->ticket.driver, instance, sizeof(*instance), KU_TASK_FRAME_ALIGNOF(KuTaskInstance_@ID@))
      || ku_task_frame_ranges_overlap(handle, sizeof(*handle), instance, sizeof(*instance))) return KU_TASK_DRIVER_INVALID_ARGUMENT;
  if (!instance->registered || instance->ticket.driver != handle->ticket.driver
      || instance->ticket.slot != handle->ticket.slot || instance->ticket.generation != handle->ticket.generation)
    return KU_TASK_DRIVER_INVALID_STATE;
  return KU_TASK_DRIVER_OK;
}
static uint32_t ku_task_@ID@_move(KuTaskHandle_@ID@* output, KuTaskHandle_@ID@* source) {
  uint32_t checked = ku_task_@ID@_check(source);
  if (checked != KU_TASK_DRIVER_OK) return checked;
  if (!ku_task_driver_external_storage(source->ticket.driver, output, sizeof(*output), KU_TASK_FRAME_ALIGNOF(KuTaskHandle_@ID@))
      || ku_task_frame_ranges_overlap(source, sizeof(*source), output, sizeof(*output))
      || ku_task_frame_ranges_overlap(source->owner.lease.control, sizeof(KuTaskInstance_@ID@), output, sizeof(*output)))
    return KU_TASK_DRIVER_INVALID_ARGUMENT;
  if (output->owner.lease.control || output->ticket.driver || output->ticket.slot || output->ticket.generation)
    return KU_TASK_DRIVER_INVALID_STATE;
  checked = ku_task_control_owner_move(&output->owner, &source->owner);
  if (checked != KU_TASK_CONTROL_OK) return checked;
  output->ticket = source->ticket; source->ticket = (KuTaskDriverTicketV1){0};
  return KU_TASK_DRIVER_OK;
}
static uint32_t ku_task_@ID@_take(KuTaskHandle_@ID@* handle, KuTaskAdapterOutcomeV1* output) {
  uint32_t checked = ku_task_@ID@_check(handle);
  if (checked != KU_TASK_DRIVER_OK) return checked;
  if (!ku_task_driver_external_storage(handle->ticket.driver, output, sizeof(*output), KU_TASK_FRAME_ALIGNOF(KuTaskAdapterOutcomeV1))
      || ku_task_frame_ranges_overlap(handle, sizeof(*handle), output, sizeof(*output))
      || ku_task_frame_ranges_overlap(handle->owner.lease.control, sizeof(KuTaskInstance_@ID@), output, sizeof(*output)))
    return KU_TASK_DRIVER_INVALID_ARGUMENT;
  /* R3 notifies only AFTER R2 has published TAKEN or restored AVAILABLE. Deep
   * payload aliases are checked inside the exclusive R2 take callback. */
  KuTaskAdapterTakeRequestV1 request={output,NULL};
  return ku_task_driver_take_result(&handle->ticket, &handle->owner.lease, &request);
}
static uint32_t ku_task_@ID@_drop(KuTaskHandle_@ID@* handle, uint64_t absolute_deadline) {
  uint32_t checked = ku_task_@ID@_check(handle);
  if (checked != KU_TASK_DRIVER_OK) return checked;
  checked = ku_task_driver_owner_drop(&handle->ticket, &handle->owner, absolute_deadline);
  if (checked == KU_TASK_DRIVER_OK) handle->ticket = (KuTaskDriverTicketV1){0};
  return checked;
}
static uint32_t ku_task_@ID@_discard_private(KuTaskControlOwnerV1* owner) {
  /* Exclusively owned, never published, never resumed, no external leases.
   * Valid generated entry cleanup is finite and cannot return Pending. No
   * retry/ABORT registration, manual ref decrement or force-free bypasses R2. */
  KuTaskDriverV1* driver = ((KuTaskInstance_@ID@*)owner->lease.control)->ticket.driver;
  uint32_t status = ku_task_control_request_cancel(&owner->lease, KU_TASK_CONTROL_CANCELLED, 0);
  if (status != KU_TASK_CONTROL_OK) { ku_task_adapter_fault(driver, 0); return KU_TASK_DRIVER_INTERNAL; }
  status = ku_task_control_poll(&owner->lease);
  if (status != KU_TASK_CONTROL_CANCELLED) { ku_task_adapter_fault(driver, 0); return KU_TASK_DRIVER_INTERNAL; }
  status = ku_task_control_owner_drop(owner, 0);
  if (status != KU_TASK_CONTROL_OK) { ku_task_adapter_fault(driver, 0); return KU_TASK_DRIVER_INTERNAL; }
  return KU_TASK_DRIVER_OK;
}
static uint32_t ku_task_@ID@_try_start(KuTaskDriverV1* driver@PARAM_DECLS@, KuTaskHandle_@ID@* output) {
  uint32_t checked = ku_task_driver_check(driver);
  if (checked != KU_TASK_DRIVER_OK) return checked;
  if (!ku_task_driver_external_storage(driver, output, sizeof(*output), KU_TASK_FRAME_ALIGNOF(KuTaskHandle_@ID@)))
    return KU_TASK_DRIVER_INVALID_ARGUMENT;
@PREFLIGHT@
  size_t charge = sizeof(KuTaskInstance_@ID@);
@CHARGE@
  /* Header/deep-alias rejection precedes typed output reads. A pointer into
   * an input int/string buffer need not actually contain a full Handle. */
  if (output->owner.lease.control || output->ticket.driver || output->ticket.slot || output->ticket.generation)
    return KU_TASK_DRIVER_INVALID_STATE;
  KuTaskDriverTicketV1 ticket = {0};
  checked = ku_task_driver_reserve(driver, charge, &ticket);
  if (checked != KU_TASK_DRIVER_OK) return checked;
  KuTaskInstance_@ID@* instance = (KuTaskInstance_@ID@*)calloc(1, sizeof(*instance));
  if (!instance) return ku_task_adapter_rollback(&ticket, KU_TASK_ADAPTER_OUT_OF_MEMORY);
  instance->ticket = ticket;
  KuTaskControlOwnerV1 owner = {0};
  checked = ku_task_control_init(&instance->control, sizeof(instance->control), KU_TASK_CONTROL_ABI_VERSION, &ku_task_@ID@_ops, instance, &owner);
  if (checked != KU_TASK_CONTROL_OK) {
    free(instance);
    return ku_task_adapter_rollback(&ticket, checked);
  }
  checked = ku_task_frame_@ID@_init(&instance->frame, sizeof(instance->frame), KU_TASK_FRAME_ABI_VERSION@PARAM_ARGS@);
  if (checked != KU_TASK_FRAME_OK) {
    uint32_t failure = ku_task_adapter_frame_status(checked);
    if (ku_task_@ID@_discard_private(&owner) != KU_TASK_DRIVER_OK) return KU_TASK_DRIVER_INTERNAL;
    return ku_task_adapter_rollback(&ticket, failure);
  }
  instance->frame_initialized = true;
  /* Publish immutable metadata BEFORE commit can make this instance runnable. */
  instance->registered = true;
  checked = ku_task_driver_commit(&ticket, &owner, KU_TASK_DRIVER_START, UINT64_MAX);
  if (checked != KU_TASK_DRIVER_OK) {
    /* A failed R3 commit did not publish any registry/execution lease. Restore
     * only Owned entry arguments; Copy caller headers were never consumed. */
    instance->registered = false;
@RESTORE@
    if (ku_task_@ID@_discard_private(&owner) != KU_TASK_DRIVER_OK) return KU_TASK_DRIVER_INTERNAL;
    return ku_task_adapter_rollback(&ticket, checked);
  }
  /* No fallible operation remains. Local owner protects a fast completion. */
  output->owner = owner;
  output->ticket = ticket;
  return KU_TASK_DRIVER_OK;
}
static uint32_t ku_task_@ID@_start_value(KuTaskDriverV1* driver@PARAM_DECLS@, KuTaskValueV1* output) {
  uint32_t checked=ku_task_driver_check(driver);
  if (checked!=KU_TASK_DRIVER_OK) return checked;
  if (!ku_task_driver_external_storage(driver,output,sizeof(*output),KU_TASK_FRAME_ALIGNOF(KuTaskValueV1))) return KU_TASK_DRIVER_INVALID_ARGUMENT;
@PREFLIGHT@
  size_t charge=0;
@CHARGE@
  (void)charge;
  if (output->tag || output->owner.lease.control || output->ticket.driver) return KU_TASK_DRIVER_INVALID_STATE;
  KuTaskHandle_@ID@ handle={0};
  uint32_t status=ku_task_@ID@_try_start(driver@PARAM_ARGS@,&handle);
  if (status!=KU_TASK_DRIVER_OK) {
    if (status!=KU_TASK_DRIVER_LIMIT && status!=KU_TASK_DRIVER_CLOSED && status!=KU_TASK_ADAPTER_OUT_OF_MEMORY) return status;
@REJECTED_DROPS@
    output->tag=KU_TASK_VALUE_INLINE_FAILED; output->result_kind=@KIND@u; output->function_id=@ID@u; output->rejection_code=status;
    return KU_TASK_DRIVER_OK;
  }
  output->tag=KU_TASK_VALUE_LIVE; output->result_kind=@KIND@u; output->function_id=@ID@u;
  output->owner=handle.owner; output->ticket=handle.ticket;
  return KU_TASK_DRIVER_OK;
}
"#;
