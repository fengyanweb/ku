//! Internal R2 control/lifetime kernel. Not a scheduler or a source Task ABI.
//!
//! A typed adapter keeps its result outside the R1 frame. This kernel alone
//! cannot make owner drop self-draining: runtime admission must retain a driver
//! lease and arrange polling until the lifecycle pin is acknowledged.

use crate::backend::c::output::COutput;
use crate::error::KuResult;

pub(super) fn emit_runtime(out: &mut COutput) -> KuResult<()> {
    out.check()?;
    out.push_str(CONTROL_ABI);
    out.check()
}

const CONTROL_ABI: &str = r#"
/* Internal control ABI v1. Every concurrent call holds a distinct live lease;
 * retain is legal only from a protected live lease, never an unprotected raw
 * pointer. Lease/owner tokens are move-only and may not be copied, forged, or
 * concurrently mutated. Control storage and its typed context are stable until
 * dispose, which alone may free their allocation. The caller initializes both
 * control storage and the owner token to zero before init. No atomics below
 * extend these requirements to stale pointers or already freed allocations.
 *
 * A successful init installs one owner reference and one lifecycle pin. Before
 * exposing the owner, the runtime MUST admit/register a driver with its own
 * lease; cancellation/drop must wake that driver, including while cleanup is
 * Pending. The pin is released only after terminal acknowledgement and frame
 * destruction. This kernel supplies neither admission nor wake/registration,
 * therefore it is not by itself a complete source Task/handle-drop runtime.
 *
 * Compiler-owned callbacks are bounded, nonthrowing, and may not longjmp.
 * resume returns Pending or stages ONE separately owned result/error/panic
 * payload and returns Completed/Failed/Panicked. Its terminal return means all
 * ordinary frame cleanup has run; drop_frame destroys remaining frame storage,
 * not that staged payload. cleanup returns Pending or OK, preserves the main
 * reason, and reads cleanup_deadline at safepoints. It must not stage a payload.
 * take_payload returns OK after moving, or InvalidArgument without mutation;
 * drop_payload/drop_frame cannot fail. No callback may release its caller's
 * lease or use the unleased budget pointer after cleanup returns. resume,
 * cleanup, and drop_frame run under the executor gate. Losing private payloads
 * are dropped there too; published take/drop callbacks instead hold the atomic
 * payload ownership claim and can run only after frame destruction. Thus frame
 * callbacks and published payload callbacks never overlap; adapters must keep
 * independently accessed metadata synchronized rather than treat the whole
 * context as protected by executor. Reentrant poll through another live lease
 * returns Pending while executor is held, or may observe the immutable terminal
 * state once released; it never waits. dispose is the last access and cannot
 * reenter this control.
 *
 * request_cancel returns OK when durable; Pending means retry is required and
 * consumes no lease/owner. owner_drop likewise retains its owner on Pending.
 * A completed, unawaited payload belongs to the logical owner, not the internal
 * references: owner_drop discards it immediately even if leases keep control
 * alive. If a take is already moving that payload, drop returns Pending and
 * retains the owner until that move finishes. A dropped result cannot be taken.
 * The first reason is immutable. Absolute deadlines are only tightened, never
 * renewed. A runtime root must create the shared cleanup budget; this kernel
 * does not create a fresh budget per task. No I/O generation, wait registration,
 * scheduler, blocking return delivery, source panic lowering, or M:N is here.
 */
#define KU_TASK_CONTROL_ABI_VERSION 1u
#define KU_TASK_CONTROL_MAX_REFERENCES ((size_t)65536u)
enum {
  KU_TASK_CONTROL_OK = 0u,
  KU_TASK_CONTROL_PENDING = 1u,
  KU_TASK_CONTROL_COMPLETED = 2u,
  KU_TASK_CONTROL_FAILED = 3u,
  KU_TASK_CONTROL_PANICKED = 4u,
  KU_TASK_CONTROL_CANCELLED = 5u,
  KU_TASK_CONTROL_TIMED_OUT = 6u,
  KU_TASK_CONTROL_INVALID_ARGUMENT = 7u,
  KU_TASK_CONTROL_ABI_MISMATCH = 8u,
  KU_TASK_CONTROL_LIMIT = 9u,
  KU_TASK_CONTROL_INVALID_STATE = 10u,
  KU_TASK_CONTROL_RESULT_TAKEN = 11u,
  KU_TASK_CONTROL_LIVE = 16u,
  KU_TASK_CONTROL_PUBLISHING_CANCEL = 17u,
  KU_TASK_CONTROL_PUBLISHING_TIMEOUT = 18u,
  KU_TASK_CONTROL_REQUESTED_CANCEL = 19u,
  KU_TASK_CONTROL_REQUESTED_TIMEOUT = 20u
};
enum {
  KU_TASK_CONTROL_PAYLOAD_EMPTY = 0u,
  KU_TASK_CONTROL_PAYLOAD_AVAILABLE = 1u,
  KU_TASK_CONTROL_PAYLOAD_TAKING = 2u,
  KU_TASK_CONTROL_PAYLOAD_TAKEN = 3u,
  KU_TASK_CONTROL_PAYLOAD_DROPPED = 4u
};

/* Reuse the closure ABI's naturally aligned atomic representation, not its
 * relaxed load, unbounded retain loop, or process-exiting overflow behavior.
 * State/result publication requires acquire loads and release stores. Strong
 * one-shot CAS means contention returns Pending without spin or busy waiting.
 */
#if defined(_MSC_VER)
typedef volatile __int64 KuTaskControlDeadlineV1;
static void ku_task_control_atomic_init(KuAtomicRefcount* value, size_t initial) {
  *value = (__int64)initial;
}
static size_t ku_task_control_atomic_load(KuAtomicRefcount* value) {
  return (size_t)_InterlockedCompareExchange64(value, 0, 0);
}
static void ku_task_control_atomic_store(KuAtomicRefcount* value, size_t next) {
  (void)_InterlockedExchange64(value, (__int64)next);
}
static bool ku_task_control_atomic_cas(
    KuAtomicRefcount* value, size_t* expected, size_t next) {
  __int64 observed = _InterlockedCompareExchange64(
      value, (__int64)next, (__int64)*expected);
  if ((size_t)observed == *expected) return true;
  *expected = (size_t)observed;
  return false;
}
static size_t ku_task_control_atomic_exchange(KuAtomicRefcount* value, size_t next) {
  return (size_t)_InterlockedExchange64(value, (__int64)next);
}
static size_t ku_task_control_atomic_release_one(KuAtomicRefcount* value) {
  return (size_t)_InterlockedExchangeAdd64(value, -1);
}
static void ku_task_control_deadline_init(KuTaskControlDeadlineV1* value) {
  *value = (__int64)UINT64_MAX;
}
static uint64_t ku_task_control_deadline_load(KuTaskControlDeadlineV1* value) {
  return (uint64_t)_InterlockedCompareExchange64(value, 0, 0);
}
static void ku_task_control_deadline_store(
    KuTaskControlDeadlineV1* value, uint64_t next) {
  (void)_InterlockedExchange64(value, (__int64)next);
}
static bool ku_task_control_deadline_cas(
    KuTaskControlDeadlineV1* value, uint64_t* expected, uint64_t next) {
  __int64 observed = _InterlockedCompareExchange64(
      value, (__int64)next, (__int64)*expected);
  if ((uint64_t)observed == *expected) return true;
  *expected = (uint64_t)observed;
  return false;
}
#else
typedef _Atomic uint64_t KuTaskControlDeadlineV1;
static void ku_task_control_atomic_init(KuAtomicRefcount* value, size_t initial) {
  atomic_init(value, initial);
}
static size_t ku_task_control_atomic_load(KuAtomicRefcount* value) {
  return atomic_load_explicit(value, memory_order_acquire);
}
static void ku_task_control_atomic_store(KuAtomicRefcount* value, size_t next) {
  atomic_store_explicit(value, next, memory_order_release);
}
static bool ku_task_control_atomic_cas(
    KuAtomicRefcount* value, size_t* expected, size_t next) {
  return atomic_compare_exchange_strong_explicit(
      value, expected, next, memory_order_acq_rel, memory_order_acquire);
}
static size_t ku_task_control_atomic_exchange(KuAtomicRefcount* value, size_t next) {
  return atomic_exchange_explicit(value, next, memory_order_acq_rel);
}
static size_t ku_task_control_atomic_release_one(KuAtomicRefcount* value) {
  return atomic_fetch_sub_explicit(value, (size_t)1, memory_order_acq_rel);
}
static void ku_task_control_deadline_init(KuTaskControlDeadlineV1* value) {
  atomic_init(value, UINT64_MAX);
}
static uint64_t ku_task_control_deadline_load(KuTaskControlDeadlineV1* value) {
  return atomic_load_explicit(value, memory_order_acquire);
}
static void ku_task_control_deadline_store(
    KuTaskControlDeadlineV1* value, uint64_t next) {
  atomic_store_explicit(value, next, memory_order_release);
}
static bool ku_task_control_deadline_cas(
    KuTaskControlDeadlineV1* value, uint64_t* expected, uint64_t next) {
  return atomic_compare_exchange_strong_explicit(
      value, expected, next, memory_order_acq_rel, memory_order_acquire);
}
#endif

typedef struct KuTaskControlV1 KuTaskControlV1;
typedef struct KuTaskControlLeaseV1 {
  KuTaskControlV1* control;
} KuTaskControlLeaseV1;
typedef struct KuTaskControlOwnerV1 {
  KuTaskControlLeaseV1 lease;
} KuTaskControlOwnerV1;
typedef struct KuTaskControlOpsV1 {
  uint32_t (*resume)(void* context);
  uint32_t (*cleanup)(void* context, uint32_t reason, KuTaskControlV1* budget);
  void (*drop_frame)(void* context);
  void (*drop_payload)(void* context);
  uint32_t (*take_payload)(void* context, void* output);
  void (*dispose)(KuTaskControlV1* control, void* context);
} KuTaskControlOpsV1;
struct KuTaskControlV1 {
  uint32_t abi_version;
  size_t storage_size;
  KuAtomicRefcount references;
  KuAtomicRefcount phase;
  KuAtomicRefcount executor;
  KuAtomicRefcount lifecycle_pin;
  KuAtomicRefcount payload;
  KuTaskControlDeadlineV1 cleanup_deadline_ms;
  KuTaskControlOpsV1 operations;
  void* context;
  /* Only accessed while owning executor. */
  uint32_t frame_destroyed;
};

static int ku_task_control_is_payload_terminal(size_t phase) {
  return phase == KU_TASK_CONTROL_COMPLETED || phase == KU_TASK_CONTROL_FAILED
      || phase == KU_TASK_CONTROL_PANICKED;
}
static int ku_task_control_is_terminal(size_t phase) {
  return ku_task_control_is_payload_terminal(phase)
      || phase == KU_TASK_CONTROL_CANCELLED || phase == KU_TASK_CONTROL_TIMED_OUT;
}
static int ku_task_control_is_publishing(size_t phase) {
  return phase == KU_TASK_CONTROL_PUBLISHING_CANCEL
      || phase == KU_TASK_CONTROL_PUBLISHING_TIMEOUT;
}
static int ku_task_control_is_requested(size_t phase) {
  return phase == KU_TASK_CONTROL_REQUESTED_CANCEL
      || phase == KU_TASK_CONTROL_REQUESTED_TIMEOUT;
}
static uint32_t ku_task_control_check_lease(const KuTaskControlLeaseV1* lease) {
  if (!ku_task_frame_storage_valid(lease, sizeof(*lease), sizeof(*lease),
                                   KU_TASK_FRAME_ALIGNOF(KuTaskControlLeaseV1))
      || !lease->control) return KU_TASK_CONTROL_INVALID_ARGUMENT;
  KuTaskControlV1* control = lease->control;
  if (!ku_task_frame_storage_valid(control, sizeof(*control), sizeof(*control),
                                   KU_TASK_FRAME_ALIGNOF(KuTaskControlV1)))
    return KU_TASK_CONTROL_INVALID_ARGUMENT;
  if (control->abi_version != KU_TASK_CONTROL_ABI_VERSION)
    return KU_TASK_CONTROL_ABI_MISMATCH;
  if (control->storage_size != sizeof(*control)) return KU_TASK_CONTROL_INVALID_ARGUMENT;
  return KU_TASK_CONTROL_OK;
}
static uint32_t ku_task_control_init(
    KuTaskControlV1* control, size_t bytes, uint32_t abi,
    const KuTaskControlOpsV1* operations, void* context, KuTaskControlOwnerV1* owner) {
  if (abi != KU_TASK_CONTROL_ABI_VERSION) return KU_TASK_CONTROL_ABI_MISMATCH;
  if (!ku_task_frame_storage_valid(control, bytes, sizeof(*control),
                                   KU_TASK_FRAME_ALIGNOF(KuTaskControlV1))
      || !ku_task_frame_storage_valid(operations, sizeof(*operations), sizeof(*operations),
                                      KU_TASK_FRAME_ALIGNOF(KuTaskControlOpsV1))
      || !ku_task_frame_storage_valid(owner, sizeof(*owner), sizeof(*owner),
                                      KU_TASK_FRAME_ALIGNOF(KuTaskControlOwnerV1)))
    return KU_TASK_CONTROL_INVALID_ARGUMENT;
  if (ku_task_frame_ranges_overlap(control, bytes, owner, sizeof(*owner))
      || ku_task_frame_ranges_overlap(control, bytes, operations, sizeof(*operations))
      || ku_task_frame_ranges_overlap(owner, sizeof(*owner), operations, sizeof(*operations)))
    return KU_TASK_CONTROL_INVALID_ARGUMENT;
  if (!ku_task_frame_zero_bytes(control, sizeof(*control)) || owner->lease.control)
    return KU_TASK_CONTROL_INVALID_STATE;
  if (!operations->resume || !operations->cleanup || !operations->drop_frame
      || !operations->drop_payload || !operations->take_payload || !operations->dispose)
    return KU_TASK_CONTROL_INVALID_ARGUMENT;
  control->abi_version = KU_TASK_CONTROL_ABI_VERSION;
  control->storage_size = sizeof(*control);
  control->operations = *operations;
  control->context = context;
  ku_task_control_atomic_init(&control->references, 2);
  ku_task_control_atomic_init(&control->phase, KU_TASK_CONTROL_LIVE);
  ku_task_control_atomic_init(&control->executor, 0);
  ku_task_control_atomic_init(&control->lifecycle_pin, 1);
  ku_task_control_atomic_init(&control->payload, KU_TASK_CONTROL_PAYLOAD_EMPTY);
  ku_task_control_deadline_init(&control->cleanup_deadline_ms);
  owner->lease.control = control;
  return KU_TASK_CONTROL_OK;
}
static uint32_t ku_task_control_lease_retain(
    const KuTaskControlLeaseV1* source, KuTaskControlLeaseV1* output) {
  uint32_t checked = ku_task_control_check_lease(source);
  if (checked != KU_TASK_CONTROL_OK) return checked;
  KuTaskControlV1* control = source->control;
  if (!ku_task_frame_storage_valid(output, sizeof(*output), sizeof(*output),
                                   KU_TASK_FRAME_ALIGNOF(KuTaskControlLeaseV1))
      || ku_task_frame_ranges_overlap(source, sizeof(*source), output, sizeof(*output))
      || ku_task_frame_ranges_overlap(control, sizeof(*control), output, sizeof(*output)))
    return KU_TASK_CONTROL_INVALID_ARGUMENT;
  if (output->control) return KU_TASK_CONTROL_INVALID_STATE;
  size_t count = ku_task_control_atomic_load(&control->references);
  if (!count) return KU_TASK_CONTROL_INVALID_STATE;
  if (count >= KU_TASK_CONTROL_MAX_REFERENCES) return KU_TASK_CONTROL_LIMIT;
  if (!ku_task_control_atomic_cas(&control->references, &count, count + 1))
    return KU_TASK_CONTROL_PENDING;
  output->control = control;
  return KU_TASK_CONTROL_OK;
}
static void ku_task_control_unref(KuTaskControlV1* control) {
  /* Each valid lease has exactly one release; the internal pin prevents a last
   * release before frame destruction. The previous live lease protects this
   * decrement; no retain-from-zero or speculative raw-pointer load is legal. */
  if (ku_task_control_atomic_release_one(&control->references) != 1) return;
  if (ku_task_control_atomic_load(&control->payload) == KU_TASK_CONTROL_PAYLOAD_AVAILABLE) {
    ku_task_control_atomic_store(&control->payload, KU_TASK_CONTROL_PAYLOAD_DROPPED);
    control->operations.drop_payload(control->context);
  }
  void (*dispose)(KuTaskControlV1*, void*) = control->operations.dispose;
  void* context = control->context;
  dispose(control, context);
  /* The allocation may be gone. No control/context access is permitted here. */
}
static uint32_t ku_task_control_lease_release(KuTaskControlLeaseV1* lease) {
  uint32_t checked = ku_task_control_check_lease(lease);
  if (checked != KU_TASK_CONTROL_OK) return checked;
  KuTaskControlV1* control = lease->control;
  lease->control = NULL;
  ku_task_control_unref(control);
  return KU_TASK_CONTROL_OK;
}
static uint32_t ku_task_control_owner_move(
    KuTaskControlOwnerV1* output, KuTaskControlOwnerV1* source) {
  if (!ku_task_frame_storage_valid(source, sizeof(*source), sizeof(*source),
                                   KU_TASK_FRAME_ALIGNOF(KuTaskControlOwnerV1))
      || !ku_task_frame_storage_valid(output, sizeof(*output), sizeof(*output),
                                      KU_TASK_FRAME_ALIGNOF(KuTaskControlOwnerV1))
      || ku_task_frame_ranges_overlap(source, sizeof(*source), output, sizeof(*output)))
    return KU_TASK_CONTROL_INVALID_ARGUMENT;
  uint32_t checked = ku_task_control_check_lease(&source->lease);
  if (checked != KU_TASK_CONTROL_OK) return checked;
  if (ku_task_frame_ranges_overlap(source->lease.control, sizeof(KuTaskControlV1),
                                   output, sizeof(*output)))
    return KU_TASK_CONTROL_INVALID_ARGUMENT;
  if (output->lease.control) return KU_TASK_CONTROL_INVALID_STATE;
  output->lease = source->lease;
  source->lease.control = NULL;
  return KU_TASK_CONTROL_OK;
}
static uint64_t ku_task_control_cleanup_deadline(KuTaskControlV1* control) {
  /* An executor-held lease protects this borrowed internal budget pointer. */
  return ku_task_control_deadline_load(&control->cleanup_deadline_ms);
}
static uint32_t ku_task_control_request_cancel(
    const KuTaskControlLeaseV1* lease, uint32_t reason, uint64_t absolute_deadline_ms) {
  uint32_t checked = ku_task_control_check_lease(lease);
  if (checked != KU_TASK_CONTROL_OK) return checked;
  if (reason != KU_TASK_CONTROL_CANCELLED && reason != KU_TASK_CONTROL_TIMED_OUT)
    return KU_TASK_CONTROL_INVALID_ARGUMENT;
  KuTaskControlV1* control = lease->control;
  size_t phase = ku_task_control_atomic_load(&control->phase);
  if (phase == KU_TASK_CONTROL_LIVE) {
    size_t reserved = reason == KU_TASK_CONTROL_CANCELLED
        ? KU_TASK_CONTROL_PUBLISHING_CANCEL : KU_TASK_CONTROL_PUBLISHING_TIMEOUT;
    if (ku_task_control_atomic_cas(&control->phase, &phase, reserved)) {
      /* Unique cancellation linearization point above. While publishing,
       * other callers return Pending and cannot modify/read this deadline.
       * There is no callback, allocation, failure, or early-return between
       * reservation and publication, so a successful caller never abandons it. */
      ku_task_control_deadline_store(&control->cleanup_deadline_ms, absolute_deadline_ms);
      ku_task_control_atomic_store(&control->phase, reason == KU_TASK_CONTROL_CANCELLED
          ? KU_TASK_CONTROL_REQUESTED_CANCEL : KU_TASK_CONTROL_REQUESTED_TIMEOUT);
      return KU_TASK_CONTROL_OK;
    }
  }
  if (ku_task_control_is_terminal(phase)) return (uint32_t)phase;
  if (ku_task_control_is_publishing(phase)) return KU_TASK_CONTROL_PENDING;
  if (!ku_task_control_is_requested(phase)) return KU_TASK_CONTROL_INVALID_STATE;
  uint64_t deadline = ku_task_control_deadline_load(&control->cleanup_deadline_ms);
  if (deadline <= absolute_deadline_ms) return KU_TASK_CONTROL_OK;
  if (ku_task_control_deadline_cas(
          &control->cleanup_deadline_ms, &deadline, absolute_deadline_ms))
    return KU_TASK_CONTROL_OK;
  /* A concurrent shorter budget already satisfies us. Otherwise the caller
   * retains its lease/owner and must arrange a bounded runtime retry. */
  return deadline <= absolute_deadline_ms ? KU_TASK_CONTROL_OK : KU_TASK_CONTROL_PENDING;
}
static uint32_t ku_task_control_owner_drop(
    KuTaskControlOwnerV1* owner, uint64_t absolute_deadline_ms) {
  if (!ku_task_frame_storage_valid(owner, sizeof(*owner), sizeof(*owner),
                                   KU_TASK_FRAME_ALIGNOF(KuTaskControlOwnerV1)))
    return KU_TASK_CONTROL_INVALID_ARGUMENT;
  uint32_t requested = ku_task_control_request_cancel(
      &owner->lease, KU_TASK_CONTROL_CANCELLED, absolute_deadline_ms);
  if (requested != KU_TASK_CONTROL_OK && !ku_task_control_is_terminal(requested))
    return requested;
  if (ku_task_control_is_payload_terminal(requested)) {
    KuTaskControlV1* control = owner->lease.control;
    size_t payload = KU_TASK_CONTROL_PAYLOAD_AVAILABLE;
    if (ku_task_control_atomic_cas(
            &control->payload, &payload, KU_TASK_CONTROL_PAYLOAD_DROPPED)) {
      /* Logical owner drop and concurrent await share this ownership claim.
       * A surviving driver/timer lease preserves control lifetime, never the
       * right to retain an unawaited payload after its owner scope has ended. */
      control->operations.drop_payload(control->context);
    } else if (payload == KU_TASK_CONTROL_PAYLOAD_TAKING) {
      return KU_TASK_CONTROL_PENDING;
    } else if (payload != KU_TASK_CONTROL_PAYLOAD_TAKEN
               && payload != KU_TASK_CONTROL_PAYLOAD_DROPPED) {
      return KU_TASK_CONTROL_INVALID_STATE;
    }
  }
  return ku_task_control_lease_release(&owner->lease);
}
static uint32_t ku_task_control_status(const KuTaskControlLeaseV1* lease) {
  uint32_t checked = ku_task_control_check_lease(lease);
  if (checked != KU_TASK_CONTROL_OK) return checked;
  size_t phase = ku_task_control_atomic_load(&lease->control->phase);
  if (ku_task_control_is_terminal(phase)) return (uint32_t)phase;
  if (phase == KU_TASK_CONTROL_LIVE || ku_task_control_is_requested(phase)
      || ku_task_control_is_publishing(phase)) return KU_TASK_CONTROL_PENDING;
  return KU_TASK_CONTROL_INVALID_STATE;
}
static uint32_t ku_task_control_take_result(
    const KuTaskControlLeaseV1* lease, void* output) {
  uint32_t checked = ku_task_control_check_lease(lease);
  if (checked != KU_TASK_CONTROL_OK) return checked;
  KuTaskControlV1* control = lease->control;
  size_t phase = ku_task_control_atomic_load(&control->phase);
  if (!ku_task_control_is_payload_terminal(phase))
    return ku_task_control_is_terminal(phase) ? (uint32_t)phase : KU_TASK_CONTROL_PENDING;
  if (!output) return KU_TASK_CONTROL_INVALID_ARGUMENT;
  size_t payload = KU_TASK_CONTROL_PAYLOAD_AVAILABLE;
  if (!ku_task_control_atomic_cas(&control->payload, &payload, KU_TASK_CONTROL_PAYLOAD_TAKING))
    return payload == KU_TASK_CONTROL_PAYLOAD_TAKING
        ? KU_TASK_CONTROL_PENDING : KU_TASK_CONTROL_RESULT_TAKEN;
  uint32_t taken = control->operations.take_payload(control->context, output);
  ku_task_control_atomic_store(&control->payload, taken == KU_TASK_CONTROL_OK
      ? KU_TASK_CONTROL_PAYLOAD_TAKEN : KU_TASK_CONTROL_PAYLOAD_AVAILABLE);
  return taken;
}
static void ku_task_control_drop_frame_once(KuTaskControlV1* control) {
  if (!control->frame_destroyed) {
    /* Mark before callback; reentrant poll fails the executor CAS. */
    control->frame_destroyed = 1;
    control->operations.drop_frame(control->context);
  }
}
static uint32_t ku_task_control_finish_poll(KuTaskControlV1* control, uint32_t terminal) {
  ku_task_control_atomic_store(&control->executor, 0);
  /* A second terminal observer can obtain executor now, so claiming the pin
   * uses an atomic exchange. The current poll's external lease still protects
   * control throughout this release and the returned scalar requires no read. */
  if (ku_task_control_atomic_exchange(&control->lifecycle_pin, 0))
    ku_task_control_unref(control);
  return terminal;
}
static uint32_t ku_task_control_poll(const KuTaskControlLeaseV1* lease) {
  uint32_t checked = ku_task_control_check_lease(lease);
  if (checked != KU_TASK_CONTROL_OK) return checked;
  KuTaskControlV1* control = lease->control;
  size_t expected = 0;
  if (!ku_task_control_atomic_cas(&control->executor, &expected, 1))
    return KU_TASK_CONTROL_PENDING;
  size_t phase = ku_task_control_atomic_load(&control->phase);
  if (ku_task_control_is_terminal(phase))
    return ku_task_control_finish_poll(control, (uint32_t)phase);
  if (phase == KU_TASK_CONTROL_LIVE) {
    uint32_t outcome = control->operations.resume(control->context);
    if (ku_task_control_is_payload_terminal(outcome)) {
      ku_task_control_atomic_store(&control->payload, KU_TASK_CONTROL_PAYLOAD_AVAILABLE);
      ku_task_control_drop_frame_once(control);
      expected = KU_TASK_CONTROL_LIVE;
      /* The fully constructed typed payload and destroyed frame precede this
       * single irreversible completion/error/panic vs cancellation decision.
       * An acquire terminal observation therefore sees a complete payload. */
      if (ku_task_control_atomic_cas(&control->phase, &expected, outcome))
        return ku_task_control_finish_poll(control, outcome);
      ku_task_control_atomic_store(&control->payload, KU_TASK_CONTROL_PAYLOAD_DROPPED);
      control->operations.drop_payload(control->context);
      phase = expected;
    } else if (outcome == KU_TASK_CONTROL_PENDING) {
      phase = ku_task_control_atomic_load(&control->phase);
    } else {
      /* Compiler adapter contract violation: do not invent a successful
       * payload or free its frame. A retained driver may request cancellation
       * and use valid cleanup; the ownership pin intentionally remains. */
      ku_task_control_atomic_store(&control->executor, 0);
      return KU_TASK_CONTROL_INVALID_STATE;
    }
  }
  if (phase == KU_TASK_CONTROL_LIVE || ku_task_control_is_publishing(phase)) {
    ku_task_control_atomic_store(&control->executor, 0);
    return KU_TASK_CONTROL_PENDING;
  }
  if (!ku_task_control_is_requested(phase)) {
    ku_task_control_atomic_store(&control->executor, 0);
    return KU_TASK_CONTROL_INVALID_STATE;
  }
  uint32_t reason = phase == KU_TASK_CONTROL_REQUESTED_CANCEL
      ? KU_TASK_CONTROL_CANCELLED : KU_TASK_CONTROL_TIMED_OUT;
  if (!control->frame_destroyed) {
    uint32_t cleaned = control->operations.cleanup(control->context, reason, control);
    if (cleaned != KU_TASK_CONTROL_OK) {
      ku_task_control_atomic_store(&control->executor, 0);
      return cleaned == KU_TASK_CONTROL_PENDING
          ? KU_TASK_CONTROL_PENDING : KU_TASK_CONTROL_INVALID_STATE;
    }
    ku_task_control_drop_frame_once(control);
  }
  /* Requested cancellation has already won the decision. Cleanup/error/return
   * cannot overwrite its reason; only this executor acknowledges its terminal
   * state after all frame ownership has ended. */
  ku_task_control_atomic_store(&control->phase, reason);
  return ku_task_control_finish_poll(control, reason);
}
"#;
