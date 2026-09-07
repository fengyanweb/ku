//! Generated, internal R4 typed factories over the existing R1/R2/R3 contracts.
//!
//! This does not enable source async or add a public/manual Task API. A caller
//! exclusively owns each handle/header it passes, and every deep Owned buffer
//! remains valid, uniquely owned and disjoint from other live allocations. We
//! validate representable ranges and header aliases, not arbitrary raw pointers.
//! All current frame operations have allocation-free/static-value semantics, so
//! input active capacity plus the single inline instance is a conservative task
//! charge, not a complete language-wide heap or transferred-result budget.

use crate::error::KuResult;
use crate::ir::task::{TaskFunction, TaskProgram, TaskSlotType};
use crate::ir::IrType;

use super::output::COutput;
use super::task::empty_value_expr;
use super::{c_move_value, c_type, c_type_suffix, unsupported};

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
        let TaskSlotType::Value { ty, borrowed } = &slot_type.ty;
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
            restore.push_str(&format!(
                "  *arg_{index} = {};\n\
                 instance->frame.header.initialized &= ~(UINT64_C(1) << {slot});\n",
                c_move_value(ty, &format!("instance->frame.s_{slot}"))?
            ));
        }
    }
    let payload_aliases = active_strings(&function.result, "instance->payload", &|place| {
        format!(
            "  if (ku_task_adapter_string_overlaps({place}, output, sizeof(*output))) return KU_TASK_CONTROL_INVALID_ARGUMENT;\n"
        )
    });
    let source = ADAPTER_FUNCTION
        .replace("@ID@", &id)
        .replace("@RESULT@", &result_type)
        .replace("@SUFFIX@", &c_type_suffix(inner)?)
        .replace("@PARAM_DECLS@", &declarations)
        .replace("@PARAM_ARGS@", &arguments)
        .replace("@PREFLIGHT@", &preflight)
        .replace("@CHARGE@", &charge)
        .replace("@RESTORE@", &restore)
        .replace("@PAYLOAD_ALIASES@", &payload_aliases)
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
 * take is a single nonblocking attempt; await registration remains a later IR.
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
} KuTaskInstance_@ID@;
typedef struct KuTaskHandle_@ID@ {
  KuTaskControlOwnerV1 owner;
  KuTaskDriverTicketV1 ticket;
} KuTaskHandle_@ID@;

static uint32_t ku_task_@ID@_resume(void* raw) {
  KuTaskInstance_@ID@* instance = (KuTaskInstance_@ID@*)raw;
  if (!instance->frame_initialized) return KU_TASK_CONTROL_INVALID_STATE;
  KuTaskAdapterClockV1 bridge = { instance->ticket.driver, NULL };
  KuTaskFrameClockV1 clock = { ku_task_adapter_now, &bridge };
  uint32_t status = ku_task_frame_@ID@_resume(&instance->frame, sizeof(instance->frame), KU_TASK_FRAME_ABI_VERSION, &clock);
  if (status == KU_TASK_FRAME_PENDING) {
    /* Bare verified Suspend has no event registration: it is explicitly YIELD. */
    status = ku_task_driver_set_intent(&instance->ticket, KU_TASK_DRIVER_YIELD);
    if (status == KU_TASK_DRIVER_OK) return KU_TASK_CONTROL_PENDING;
  } else if (status == KU_TASK_FRAME_READY && !instance->payload_initialized) {
    status = ku_task_frame_@ID@_take_result(&instance->frame, sizeof(instance->frame), KU_TASK_FRAME_ABI_VERSION, &instance->payload);
    if (status == KU_TASK_FRAME_OK) {
      instance->payload_initialized = true;
      return instance->payload.ok ? KU_TASK_CONTROL_COMPLETED : KU_TASK_CONTROL_FAILED;
    }
  }
  ku_task_adapter_fault(instance->ticket.driver, 0);
  return KU_TASK_CONTROL_INVALID_STATE;
}
static uint32_t ku_task_@ID@_cleanup(void* raw, uint32_t reason, KuTaskControlV1* budget) {
  KuTaskInstance_@ID@* instance = (KuTaskInstance_@ID@*)raw;
  if (!instance->frame_initialized) return KU_TASK_CONTROL_OK;
  uint32_t frame_reason = reason == KU_TASK_CONTROL_CANCELLED ? KU_TASK_FRAME_CANCELLED
      : reason == KU_TASK_CONTROL_TIMED_OUT ? KU_TASK_FRAME_TIMED_OUT : KU_TASK_FRAME_INVALID_ARGUMENT;
  if (frame_reason == KU_TASK_FRAME_INVALID_ARGUMENT) return KU_TASK_CONTROL_INVALID_ARGUMENT;
  KuTaskAdapterClockV1 bridge = { instance->ticket.driver, budget };
  KuTaskFrameClockV1 clock = { ku_task_adapter_now, &bridge };
  uint64_t deadline = ku_task_control_cleanup_deadline(budget);
  uint32_t status = ku_task_frame_@ID@_terminate(&instance->frame, sizeof(instance->frame), KU_TASK_FRAME_ABI_VERSION, frame_reason, deadline, &clock);
  if (status == KU_TASK_FRAME_CANCELLED || status == KU_TASK_FRAME_TIMED_OUT) return KU_TASK_CONTROL_OK;
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
  @RESULT@* output = (@RESULT@*)destination;
  if (!ku_task_driver_external_storage(instance->ticket.driver, output, sizeof(*output), KU_TASK_FRAME_ALIGNOF(@RESULT@))
      || ku_task_frame_ranges_overlap(instance, sizeof(*instance), output, sizeof(*output))) return KU_TASK_CONTROL_INVALID_ARGUMENT;
  if (!instance->payload_initialized) return KU_TASK_CONTROL_RESULT_TAKEN;
@PAYLOAD_ALIASES@
  /* Reject deep alias before reading a typed header from that address: the
   * aliased allocation itself may be smaller than sizeof(*output). */
  if (!(@EMPTY_RESULT@)) return KU_TASK_CONTROL_INVALID_ARGUMENT;
  *output = ku_result_move_@SUFFIX@(&instance->payload);
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
static uint32_t ku_task_@ID@_take(KuTaskHandle_@ID@* handle, @RESULT@* output) {
  uint32_t checked = ku_task_@ID@_check(handle);
  if (checked != KU_TASK_DRIVER_OK) return checked;
  if (!ku_task_driver_external_storage(handle->ticket.driver, output, sizeof(*output), KU_TASK_FRAME_ALIGNOF(@RESULT@))
      || ku_task_frame_ranges_overlap(handle, sizeof(*handle), output, sizeof(*output))
      || ku_task_frame_ranges_overlap(handle->owner.lease.control, sizeof(KuTaskInstance_@ID@), output, sizeof(*output)))
    return KU_TASK_DRIVER_INVALID_ARGUMENT;
  /* R3 notifies only AFTER R2 has published TAKEN or restored AVAILABLE. Deep
   * payload aliases are checked inside the exclusive R2 take callback. */
  return ku_task_driver_take_result(&handle->ticket, &handle->owner.lease, output);
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
"#;
