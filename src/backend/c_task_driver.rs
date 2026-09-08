//! Internal fixed worker-group driver over the R1 frame/R2 control contracts.
//! Caller-owned storage serves the restricted source Task path.
//! Shared-ring parallel execution is bounded; work stealing and event polling remain unimplemented.

use crate::backend::c::output::COutput;
use crate::error::KuResult;

pub(super) fn emit_runtime(out: &mut COutput) -> KuResult<()> {
    out.check()?;
    out.push_str(DRIVER_ABI);
    out.check()
}

const DRIVER_ABI: &str = r#"
/* Internal driver ABI 7, not a public Task API, netpoll, or complete M:N scheduler.
 * Private V1 type spellings are retained, not their old binary layout/version.
 * Caller-owned zero-filled driver/slot/ring storage remains live until destroy
 * succeeds. Every API caller protects that storage. Init, join and destroy are
 * exclusive lifecycle operations; no other caller races them. Partial init
 * failure may leave LIVE or REAPING ownership: status alone never permits free.
 * Created, last-storage-access EXITED, OS joined and handle closed are distinct.
 * Only a completely created healthy group admits new reservations. Fixed worker
 * records allocate no queue/RC edge. State-observer notifications never wake
 * work sleepers merely because another worker is parking.
 * Both conditions use the same mutex and the same absolute clock domain.
 * A reservation protects storage even before a control exists. Tickets are
 * generation-checked bindings, not unprotected pointers to control objects.
 * Controls may be accessed concurrently only through distinct live R2 leases.
 *
 * reserve consumes no input. rollback is legal ONLY with no live control/pin
 * and no registered control. A trusted builder may first fully dispose an
 * unpublished control through R2; otherwise it uses commit(ABORT) with a
 * trusted adapter able to clean its partially initialized frame. START preserves
 * owner, ABORT consumes it; neither exposes a control without both registry and
 * execution leases. A builder supplies a unique unpublished control, never a
 * control registered with another driver; same-driver rebinding is checked.
 * This raw cross-driver uniqueness contract is not a public user capability.
 * The adapter's final dispose calls disposed using a local
 * ticket copy AFTER freeing its task allocation; no task access follows that
 * budget return. Task bytes include control/frame/adapter/owned capacities;
 * fixed_bytes covers supplied driver/slot/ring storage, not OS thread stacks.
 * Growing payloads require a future budget allocator; do not claim total RSS.
 * Hosted Start reserves only its new instance: existing Owned input charge
 * stays on the RUNNING parent during synchronous construction. commit_child
 * moves that charge and publishes the child under one mutex acquisition;
 * every failure precedes both changes. Its immutable request is compiler-owned
 * stack storage, not an authority token or a lease. The same parent/request/
 * instance size must be used for reserve_child and commit_child in one callback.
 * Never publish a hosted reservation with raw commit/ABORT or let an input-owning
 * builder escape the parent callback. On failure restore Owned arguments, fully
 * dispose the unpublished control through R2, then rollback its instance charge.
 * Raw reserve/commit continue to charge all externally supplied input capacity.
 *
 * All R2 access after commit uses these wrappers or the slot's sole executor. Published
 * take/cancel progress is notified AFTER R2's final state store. Callbacks set
 * YIELD or WAIT before returning Pending. WAIT is a trusted internal registered
 * progress source with a ticket; it is not permission for user cleanup await.
 * Wake during RUNNING sets NOTIFIED, otherwise one preallocated queue ticket is
 * used. Cancellation/owner retirement never allocate or require extra capacity.
 * Callbacks, control poll, release/dispose and payload destruction run unlocked.
 * An unexpected non-Pending poll result parks the slot as FAULTED. Ordinary
 * wake/YIELD must not replay a partially executed frame; only cancellation or
 * shutdown may schedule its still-live cleanup. References and charge stay
 * owned until that cleanup actually finishes, and the driver fault is sticky.
 * Lock-held control access is limited to acquire readiness/cancel-state reads
 * and shutdown's bounded atomic-only request_cancel, protected by the registry
 * lease: none of these has a callback,
 * allocation, release or wait and must remain so. Cancellation must not need a
 * fresh reference when a control is already at its reference limit.
 * No callback blocks indefinitely or recursively polls another task.
 *
 * Result waits are fixed slot IDs/generations/epochs, never new control refs.
 * Each parent has one outgoing wait and each child has at most one waiter.
 * Only the executing compiler callback arms a wait; its hidden child owner
 * stays live until take/drop. All token/header calls are exclusive, complete
 * caller-owned objects, disjoint from control and Task-owned deep storage.
 * read/detach may use an externally synchronized token while a parent parks.
 * Successful detach supplies one queue/YIELD progress event when needed.
 * Cancelling a parent aborts only its outgoing RESULT wait: its incoming
 * ancestor still waits for that parent's actual cleanup/terminal publication.
 * ACK waits use the same single link, not another queue or a retained child.
 * The legacy final-drain path holds one finite absolute scope deadline per
 * task generation. The optional scope_begin/scope_end protocol below permits
 * separate successful normal scopes without changing that legacy path.
 * Each active drain holds a finite absolute scope
 * deadline: never recreate a budget at arm/detach or cancel the normal parent
 * merely to wake its scope. Runtime cleanup ACK waits do not permit user
 * cleanup Suspend. A resumed callback reads its existing token and sets WAIT
 * again if still pending. Immediate ACK/error must be handled in that bounded
 * quantum; expired cleanup cannot manufacture progress with repeated YIELD.
 * Cleanup receipts are issued only by successful owner transfer. They are
 * non-owning IDs, not leases, result values, or authenticated raw C objects.
 * Do not fabricate receipts from tickets. Their storage is caller-owned and
 * disjoint from owner/ticket/control/runtime and Task-owned deep allocations.
 * Receipts may be copied/read while the caller separately protects driver
 * storage; they cannot be read after driver destruction. A per-slot ACK
 * watermark survives disposal/reuse/BUILDING rollback for valid old receipts.
 * ACK proves logical frame/pin/owner/payload cleanup, not final reference
 * release, a successful Task result, or physical cleanup of an entire subtree.
 * An impossible ACK state quarantines only that slot: its receipt reports
 * INTERNAL, no further polls are scheduled, and leases/charged bytes remain
 * accounted. This is not recoverable cancellation or permission to force-free.
 *
 * A scope session registers its COMPLETE expected live-child set before any
 * owner transfer. Its receipt array is stable instance-owned storage, not a
 * callback's short-lived stack array. The real registry/executor protects the
 * instance; only its sole RUNNING executor calls begin/end/cleanup_wait_arm.
 * Expected entries begin zero, are written once by owner_drop_receipt, and
 * remain immutable through end/terminal. Do not change the span or an
 * issued receipt, and do not omit an owned child from the initial expected
 * mask. These raw provenance/lifetime/unique-write preconditions do not claim
 * to authenticate arbitrary hostile C pointers or forged old receipts.
 * Timer, notify, shutdown and terminal paths never dereference the borrowed
 * span. End accepts no replacement list/subset: every registered expected
 * entry must be issued and actually ACKed. No new heap/queue/RC edge is used.
 * Pending end is only an observation, not a registered wake or permission to
 * retry in a loop: finish the transfer batch and arm its real pending receipt.
 * Unissued zero entries count as Pending; observing them after D expires may
 * latch CLEANUP_TIMEOUT. All ACKs win over a fired timer alone, but never over
 * an already latched scope failure. End certifies this manifest, not a subtree.
 * An active session may arm only ACK waits on its original expected entries.
 * scope_promote_final alone may OR still-live outer children into that same
 * fixed manifest, before transferring any added owner. It preserves every old
 * expected bit (including issued ACKs and unissued entries), receipt, token,
 * wait and epoch. Newly added entries must be empty. This is a one-way FINAL
 * session, never eligible for normal end or a renewed cleanup budget. LIVE
 * callers require a trusted typed whole-function exit witness; this raw kernel
 * cannot authenticate source return/fail/runtime-failure rather than Continue.
 * Promotion accepts LIVE, requested and PUBLISHING without changing R2 outcome.
 * It neither waits for publication nor reads an unpublished control deadline.
 * Its output is a complete exclusive u64 object, not necessarily zero; failure
 * leaves all objects unchanged. Existing driver/scope failure remains sticky
 * but does not alone prevent safe structural registration of outer owners.
 * Metadata OK is not successful cleanup, child ACK or Task completion.
 * Parent D tightening does not scan this span to cancel children: the future
 * adapter must propagate shorter D through cancel_receipt for every pending
 * issued child. Terminal unregister only forgets metadata; it does not ACK or
 * drain omitted children on behalf of an incorrect caller.
 * Successful end clears only this normal scope's D, never cancellation or
 * shutdown state. Its final acquire LIVE read is the close linearization
 * point; the mutex does NOT serialize external R2 cancellation CAS. A later
 * cancel may overlap successful end. A future source adapter MUST return to
 * ku_task_dispatch and acquire-check phase before executing the next source
 * block. A cancellation already observed by end leaves D/manifest registered.
 *
 * owner_drop durably moves the owner to reserved storage, not completion of its
 * cleanup. Parent scope cleanup must separately await logical cleanup ACK,
 * not final dispose (a late observer can keep control storage alive), without
 * marking normal parent return Cancelled. Pending owner retirement is
 * kept in a slot and waits for publication/take notification, not a retry loop.
 * Terminal owners/late leases still occupy resident count/bytes until dispose.
 * Shutdown cannot steal a user owner. Timeout leaves storage and workers valid;
 * caller releases its owners/registrations and retries shutdown/join/destroy.
 * Join never grants a fresh cleanup budget. Windows waits use the remaining
 * shared absolute D; POSIX joins follow all-worker last-access witnesses and
 * retain pthread_join's final OS-return scheduling boundary, not a hard timeout.
 * Clock failure is sticky: close admission, cancel with shared deadline zero,
 * and keep healthy workers for deterministic cleanup. Idle faulted workers wait only
 * for OS condition notifications, never retry a failed clock periodically.
 */
#define KU_TASK_DRIVER_ABI_VERSION 7u
#define KU_TASK_DRIVER_MAX_SLOTS ((size_t)1024u)
#define KU_TASK_DRIVER_MAX_WORKERS ((size_t)32u)
enum {
  KU_TASK_DRIVER_OK = 0u, KU_TASK_DRIVER_PENDING = 1u,
  KU_TASK_DRIVER_INVALID_ARGUMENT = 7u, KU_TASK_DRIVER_ABI_MISMATCH = 8u,
  KU_TASK_DRIVER_LIMIT = 9u, KU_TASK_DRIVER_INVALID_STATE = 10u,
  KU_TASK_DRIVER_STALE = 32u, KU_TASK_DRIVER_CLOSED = 33u,
  KU_TASK_DRIVER_SHUTDOWN_TIMEOUT = 34u, KU_TASK_DRIVER_INTERNAL = 35u,
  KU_TASK_DRIVER_WAIT_READY = 36u, KU_TASK_DRIVER_WAIT_ABORTED = 37u,
  KU_TASK_DRIVER_WAIT_CYCLE = 38u, KU_TASK_DRIVER_CLEANUP_ACK = 39u,
  KU_TASK_DRIVER_CLEANUP_TIMEOUT = 40u,
  KU_TASK_DRIVER_START = 0u, KU_TASK_DRIVER_ABORT = 1u,
  KU_TASK_DRIVER_YIELD = 1u, KU_TASK_DRIVER_WAIT = 2u
};
enum {
  KU_TASK_DRIVER_FREE = 0u, KU_TASK_DRIVER_BUILDING = 1u,
  KU_TASK_DRIVER_QUEUED = 2u, KU_TASK_DRIVER_RUNNING = 3u,
  KU_TASK_DRIVER_PARKED = 4u, KU_TASK_DRIVER_RETIRING = 5u,
  KU_TASK_DRIVER_TERMINAL_HELD = 6u, KU_TASK_DRIVER_FAULTED = 7u,
  KU_TASK_DRIVER_OWNER_USER = 0u, KU_TASK_DRIVER_OWNER_DEFERRED = 1u,
  KU_TASK_DRIVER_OWNER_WORKER = 2u, KU_TASK_DRIVER_OWNER_RELEASED = 3u
};
enum {
  KU_TASK_DRIVER_STORAGE_ZERO = 0u, KU_TASK_DRIVER_STORAGE_LIVE = 1u,
  KU_TASK_DRIVER_STORAGE_REAPING = 2u,
  KU_TASK_DRIVER_WORKER_ACTIVE = 0u, KU_TASK_DRIVER_WORKER_WAITING = 1u,
  KU_TASK_DRIVER_WORKER_EXITED = 2u,
  KU_TASK_DRIVER_SYNC_MUTEX = 1u, KU_TASK_DRIVER_SYNC_STATE = 2u,
  KU_TASK_DRIVER_SYNC_WORK = 4u, KU_TASK_DRIVER_SYNC_ATTRIBUTES = 8u
};
enum {
  KU_TASK_DRIVER_WAIT_EMPTY = 0u, KU_TASK_DRIVER_WAIT_ARMED = 1u,
  KU_TASK_DRIVER_WAIT_NOTIFIED = 2u,
  KU_TASK_DRIVER_WAIT_KIND_EMPTY = 0u, KU_TASK_DRIVER_WAIT_KIND_RESULT = 1u,
  KU_TASK_DRIVER_WAIT_KIND_CLEANUP_ACK = 2u
};
#if defined(_WIN32)
#ifndef WIN32_LEAN_AND_MEAN
#define WIN32_LEAN_AND_MEAN
#endif
#include <windows.h>
#include <process.h>
typedef SRWLOCK KuTaskDriverMutexV1;
typedef CONDITION_VARIABLE KuTaskDriverConditionV1;
#else
#include <pthread.h>
#include <time.h>
#include <errno.h>
typedef pthread_mutex_t KuTaskDriverMutexV1;
typedef pthread_cond_t KuTaskDriverConditionV1;
#endif
typedef struct KuTaskDriverV1 KuTaskDriverV1;
typedef struct KuTaskDriverTicketV1 {
  KuTaskDriverV1* driver;
  size_t slot;
  uint64_t generation;
} KuTaskDriverTicketV1;
/* No driver/slot layout change: this request exists only on a compiler callback
 * stack. Parent control is an identity, dereferenced only via a live registry. */
typedef struct KuTaskDriverStartChargeV1 {
  KuTaskDriverTicketV1 parent;
  KuTaskControlV1* parent_control;
  size_t minimum_parent_bytes, owned_bytes;
} KuTaskDriverStartChargeV1;
typedef struct KuTaskDriverCleanupReceiptV1 {
  KuTaskDriverV1* driver;
  size_t slot;
  uint64_t generation;
  uint32_t abi_version;
} KuTaskDriverCleanupReceiptV1;
typedef struct KuTaskDriverScopeTokenV1 {
  KuTaskDriverV1* driver;
  size_t parent_slot;
  uint64_t parent_generation, scope_id, epoch;
} KuTaskDriverScopeTokenV1;
typedef struct KuTaskDriverWaitTokenV1 {
  KuTaskDriverV1* driver;
  size_t parent_slot;
  uint64_t parent_generation, epoch;
} KuTaskDriverWaitTokenV1;
typedef struct KuTaskDriverWaitSnapshotV1 {
  uint32_t state, outcome;
  size_t child_slot;
  uint64_t child_generation;
  uint32_t kind;
} KuTaskDriverWaitSnapshotV1;
typedef struct KuTaskDriverSlotV1 {
  uint64_t generation;
  /* Mutex-owned proof for issued receipts, preserved across slot reuse. */
  uint64_t cleanup_acked_generation;
  uint32_t cleanup_fault;
  size_t charged_bytes;
  uint32_t state, notified, intent, owner_location;
  uint32_t deadline_fired, cancel_pending;
  uint64_t cancel_deadline;
  size_t wrapper_active;
  KuTaskControlV1* binding;
  KuTaskControlLeaseV1 driver_lease;
  KuTaskControlLeaseV1 execution_lease;
  KuTaskControlOwnerV1 deferred_owner;
  /* Mutex-owned IDs only: neither edge creates a reference-count cycle. */
  uint64_t wait_epoch;
  uint32_t wait_state, wait_outcome;
  uint32_t wait_kind;
  uint32_t scope_wait_started, scope_deadline_fired, scope_wait_failure;
  uint64_t scope_wait_deadline;
  /* Borrowed stable manifest; read only by the sole RUNNING executor.
   * Epoch is never reset by normal close while this generation is resident. */
  uint32_t scope_session_active, scope_session_final;
  uint64_t scope_session_id, scope_session_epoch;
  const KuTaskDriverCleanupReceiptV1* scope_receipts;
  size_t scope_receipt_capacity;
  uint64_t scope_expected_mask;
  size_t wait_child_slot;
  uint64_t wait_child_generation;
  size_t waiter_parent_slot;
  uint64_t waiter_parent_generation, waiter_epoch;
} KuTaskDriverSlotV1;
typedef struct KuTaskDriverWorkerV1 {
  KuTaskDriverV1* driver;
  size_t index;
  uint32_t state, joined, closed;
#if defined(_WIN32)
  HANDLE thread;
#else
  pthread_t thread;
#endif
} KuTaskDriverWorkerV1;
typedef struct KuTaskDriverSnapshotV1 {
  size_t resident, building, queued, running, parked, retiring, terminal_held;
  size_t reserved_bytes, fixed_bytes, byte_limit;
  uint64_t polls, wakes, waits;
  size_t worker_target, workers_created, workers_waiting, workers_exited, workers_joined;
  uint32_t closing, fault, clock_fault;
} KuTaskDriverSnapshotV1;
struct KuTaskDriverV1 {
  uint32_t abi_version, initialized, closing, fault, clock_fault, sync_resources;
  size_t worker_target, workers_created, workers_waiting, workers_exited, workers_joined;
  size_t storage_size, capacity, head, queued, running, resident, building;
  size_t reserved_bytes, fixed_bytes, byte_limit;
  uint64_t polls, wakes, waits, shutdown_deadline, next_deadline;
  KuTaskDriverSlotV1* slots;
  size_t* ring;
  KuTaskDriverMutexV1 mutex;
  KuTaskDriverConditionV1 condition, work_condition;
#if !defined(_WIN32) && !defined(__APPLE__)
  pthread_condattr_t condition_attributes;
#endif
  KuTaskDriverWorkerV1 workers[KU_TASK_DRIVER_MAX_WORKERS];
};
static uint64_t ku_task_driver_now_ms(void) {
  /* UINT64_MAX is a clock failure/range sentinel, never a valid sampled time.
   * Returning zero on failure would allow expired waits to retry forever. */
#if defined(_WIN32)
  return (uint64_t)GetTickCount64();
#else
  struct timespec value;
  if (clock_gettime(CLOCK_MONOTONIC, &value) != 0 || value.tv_sec < 0
      || value.tv_nsec < 0 || value.tv_nsec >= 1000000000L) return UINT64_MAX;
  uint64_t seconds = (uint64_t)value.tv_sec;
  if (seconds > UINT64_MAX / 1000u) return UINT64_MAX;
  uint64_t whole = seconds * 1000u, fraction = (uint64_t)value.tv_nsec / 1000000u;
  return fraction > UINT64_MAX - whole ? UINT64_MAX : whole + fraction;
#endif
}
static uint64_t ku_task_driver_min(uint64_t a, uint64_t b) { return a < b ? a : b; }
static int ku_task_driver_lock(KuTaskDriverV1* driver) {
#if defined(_WIN32)
  AcquireSRWLockExclusive(&driver->mutex); return 0;
#else
  return pthread_mutex_lock(&driver->mutex);
#endif
}
static int ku_task_driver_unlock(KuTaskDriverV1* driver) {
#if defined(_WIN32)
  ReleaseSRWLockExclusive(&driver->mutex); return 0;
#else
  return pthread_mutex_unlock(&driver->mutex);
#endif
}
static void ku_task_driver_signal(KuTaskDriverV1* driver) {
#if defined(_WIN32)
  WakeAllConditionVariable(&driver->condition);
#else
  if (pthread_cond_broadcast(&driver->condition) != 0) driver->fault = KU_TASK_DRIVER_INTERNAL;
#endif
}
/* Worker work and external state observers deliberately use separate channels.
 * A worker's parking announcement must not wake another idle worker. */
static void ku_task_driver_signal_work(KuTaskDriverV1* driver) {
#if defined(_WIN32)
  WakeAllConditionVariable(&driver->work_condition);
#else
  if (pthread_cond_broadcast(&driver->work_condition) != 0) driver->fault = KU_TASK_DRIVER_INTERNAL;
#endif
}
/* Requires mutex. 0: notification/spurious wake; 1: deadline; -1: OS error;
 * -2: clock failure (distinct from a broken synchronization primitive).
 * The predicate is always rechecked. No fixed-interval polling is used. */
static int ku_task_driver_wait_on(
    KuTaskDriverV1* driver, KuTaskDriverConditionV1* condition, uint64_t deadline) {
  uint64_t now = ku_task_driver_now_ms();
  if (now == UINT64_MAX) return -2;
  if (deadline != UINT64_MAX && now >= deadline) return 1;
#if defined(_WIN32)
  DWORD delay = INFINITE;
  if (deadline != UINT64_MAX) {
    uint64_t remaining = deadline - now;
    delay = remaining >= (uint64_t)INFINITE ? INFINITE - 1u : (DWORD)remaining;
  }
  if (SleepConditionVariableSRW(condition, &driver->mutex, delay, 0)) return 0;
  return GetLastError() == ERROR_TIMEOUT ? 1 : -1;
#else
  if (deadline == UINT64_MAX) return pthread_cond_wait(condition, &driver->mutex) == 0 ? 0 : -1;
  uint64_t remaining = deadline - now;
  /* Use at most a day for the OS conversion, then recheck the absolute budget.
   * This is a platform range bound, not a progress-poll interval. */
  if (remaining > UINT64_C(86400000)) remaining = UINT64_C(86400000);
  struct timespec timeout;
#if defined(__APPLE__)
  timeout.tv_sec = (time_t)(remaining / 1000u);
  timeout.tv_nsec = (long)((remaining % 1000u) * 1000000u);
  int result = pthread_cond_timedwait_relative_np(condition, &driver->mutex, &timeout);
#else
  /* The condition uses CLOCK_MONOTONIC too. Reconstructing from a SECOND clock
   * sample plus the OLD remaining duration would renew the deadline by any
   * preemption between samples. Convert the original absolute budget directly. */
  uint64_t wake_at = now + remaining;
  timeout.tv_sec = (time_t)(wake_at / 1000u);
  if ((uint64_t)timeout.tv_sec != wake_at / 1000u) return -1;
  timeout.tv_nsec = (long)((wake_at % 1000u) * 1000000u);
  int result = pthread_cond_timedwait(condition, &driver->mutex, &timeout);
#endif
  return result == 0 ? 0 : result == ETIMEDOUT ? 1 : -1;
#endif
}
static int ku_task_driver_wait(KuTaskDriverV1* driver, uint64_t deadline) {
  return ku_task_driver_wait_on(driver, &driver->condition, deadline);
}
static int ku_task_driver_wait_work(KuTaskDriverV1* driver, uint64_t deadline) {
  return ku_task_driver_wait_on(driver, &driver->work_condition, deadline);
}
static int ku_task_driver_wait_without_clock(KuTaskDriverV1* driver) {
  KuTaskDriverConditionV1* condition = &driver->work_condition;
#if defined(_WIN32)
  return SleepConditionVariableSRW(condition, &driver->mutex, INFINITE, 0) ? 0 : -1;
#else
  return pthread_cond_wait(condition, &driver->mutex) == 0 ? 0 : -1;
#endif
}
static uint32_t ku_task_driver_check_lifecycle(KuTaskDriverV1* driver) {
  if (!ku_task_frame_storage_valid(driver, sizeof(*driver), sizeof(*driver),
                                   KU_TASK_FRAME_ALIGNOF(KuTaskDriverV1)))
    return KU_TASK_DRIVER_INVALID_ARGUMENT;
  if (driver->abi_version != KU_TASK_DRIVER_ABI_VERSION) return KU_TASK_DRIVER_ABI_MISMATCH;
  if ((driver->initialized != KU_TASK_DRIVER_STORAGE_LIVE
       && driver->initialized != KU_TASK_DRIVER_STORAGE_REAPING)
      || driver->storage_size != sizeof(*driver)) return KU_TASK_DRIVER_INVALID_STATE;
  return KU_TASK_DRIVER_OK;
}
static uint32_t ku_task_driver_check(KuTaskDriverV1* driver) {
  uint32_t checked = ku_task_driver_check_lifecycle(driver);
  if (checked != KU_TASK_DRIVER_OK) return checked;
  if (driver->initialized != KU_TASK_DRIVER_STORAGE_LIVE
      || driver->sync_resources != (KU_TASK_DRIVER_SYNC_MUTEX
          | KU_TASK_DRIVER_SYNC_STATE | KU_TASK_DRIVER_SYNC_WORK))
    return KU_TASK_DRIVER_INVALID_STATE;
  return KU_TASK_DRIVER_OK;
}
static uint32_t ku_task_driver_check_ticket(const KuTaskDriverTicketV1* ticket) {
  if (!ku_task_frame_storage_valid(ticket, sizeof(*ticket), sizeof(*ticket),
                                   KU_TASK_FRAME_ALIGNOF(KuTaskDriverTicketV1)))
    return KU_TASK_DRIVER_INVALID_ARGUMENT;
  return ku_task_driver_check(ticket->driver);
}
static int ku_task_driver_external_storage(
    KuTaskDriverV1* driver, const void* pointer, size_t size, size_t alignment) {
  return ku_task_frame_storage_valid(pointer, size, size, alignment)
      && !ku_task_frame_ranges_overlap(pointer, size, driver, sizeof(*driver))
      && !ku_task_frame_ranges_overlap(pointer, size, driver->slots,
                                       driver->capacity * sizeof(*driver->slots))
      && !ku_task_frame_ranges_overlap(pointer, size, driver->ring,
                                       driver->capacity * sizeof(*driver->ring));
}
static uint32_t ku_task_driver_check_cleanup_receipt(const KuTaskDriverCleanupReceiptV1* receipt) {
  if (!ku_task_frame_storage_valid(receipt, sizeof(*receipt), sizeof(*receipt),
                                   KU_TASK_FRAME_ALIGNOF(KuTaskDriverCleanupReceiptV1)))
    return KU_TASK_DRIVER_INVALID_ARGUMENT;
  if (receipt->abi_version != KU_TASK_DRIVER_ABI_VERSION) return KU_TASK_DRIVER_ABI_MISMATCH;
  uint32_t checked = ku_task_driver_check(receipt->driver);
  if (checked != KU_TASK_DRIVER_OK) return checked;
  KuTaskDriverV1* driver = receipt->driver;
  if (!ku_task_driver_external_storage(driver, receipt, sizeof(*receipt),
                                       KU_TASK_FRAME_ALIGNOF(KuTaskDriverCleanupReceiptV1))
      || !receipt->generation || receipt->slot >= driver->capacity)
    return KU_TASK_DRIVER_INVALID_ARGUMENT;
  return KU_TASK_DRIVER_OK;
}
/* Mutex-held, non-owning receipt proof. Never dereference a retired binding. */
static uint32_t ku_task_driver_cleanup_receipt_status_locked(
    KuTaskDriverV1* driver, size_t index, uint64_t generation) {
  if (index >= driver->capacity || !generation) return KU_TASK_DRIVER_INVALID_ARGUMENT;
  KuTaskDriverSlotV1* slot = &driver->slots[index];
  if (slot->cleanup_acked_generation > slot->generation) return KU_TASK_DRIVER_INTERNAL;
  if (generation > slot->generation) return KU_TASK_DRIVER_INVALID_ARGUMENT;
  if (slot->cleanup_acked_generation >= generation) return KU_TASK_DRIVER_CLEANUP_ACK;
  if (slot->generation == generation && slot->cleanup_fault) return slot->cleanup_fault;
  if (slot->generation == generation && slot->state != KU_TASK_DRIVER_FREE
      && slot->state != KU_TASK_DRIVER_BUILDING && slot->state != KU_TASK_DRIVER_RETIRING)
    return KU_TASK_DRIVER_PENDING;
  return KU_TASK_DRIVER_INTERNAL;
}
static void ku_task_driver_arm_deadline(KuTaskDriverV1* driver, KuTaskDriverSlotV1* slot) {
  if (slot->cleanup_fault) return;
  uint64_t previous = driver->next_deadline;
  if (!slot->deadline_fired)
    driver->next_deadline = ku_task_driver_min(driver->next_deadline, slot->cancel_deadline);
  if (slot->scope_wait_started && !slot->scope_deadline_fired && !slot->scope_wait_failure)
    driver->next_deadline = ku_task_driver_min(driver->next_deadline, slot->scope_wait_deadline);
  if (driver->next_deadline < previous) ku_task_driver_signal_work(driver);
}
static void ku_task_driver_enter_clock_fault(KuTaskDriverV1* driver);
/* Requires mutex and a protected driver lifetime. */
static KuTaskDriverSlotV1* ku_task_driver_find(const KuTaskDriverTicketV1* ticket) {
  KuTaskDriverV1* driver = ticket->driver;
  if (ticket->slot >= driver->capacity) return NULL;
  KuTaskDriverSlotV1* slot = &driver->slots[ticket->slot];
  return slot->state != KU_TASK_DRIVER_FREE && ticket->generation
      && slot->generation == ticket->generation ? slot : NULL;
}
static void ku_task_driver_enqueue(KuTaskDriverV1* driver, size_t index) {
  KuTaskDriverSlotV1* slot = &driver->slots[index];
  if (slot->cleanup_fault) return; /* Quarantined, never a retryable Pending. */
  if (slot->state == KU_TASK_DRIVER_RUNNING) { slot->notified = 1; return; }
  if (slot->state == KU_TASK_DRIVER_QUEUED || slot->state == KU_TASK_DRIVER_BUILDING
      || slot->state == KU_TASK_DRIVER_RETIRING || slot->state == KU_TASK_DRIVER_FREE) return;
  if (slot->state == KU_TASK_DRIVER_FAULTED && !slot->cancel_pending && !driver->closing) {
    /* A stale/spurious notification is not permission to rerun an invalid
     * continuation. The real registry, not the ticket, protects this read.
     * A cancellation already published during the failed poll still wakes
     * cleanup, including the owner-transfer and wrapper-end paths. */
    KuTaskControlV1* control = slot->driver_lease.control;
    if (!control || slot->binding != control) { driver->fault = KU_TASK_DRIVER_INTERNAL; return; }
    size_t phase = ku_task_control_atomic_load(&control->phase);
    if (!ku_task_control_is_requested(phase) && !ku_task_control_is_terminal(phase)) return;
  }
  /* At most one entry per resident slot: capacity is reserved at admission. */
  if (driver->queued >= driver->capacity) { driver->fault = KU_TASK_DRIVER_INTERNAL; return; }
  driver->ring[(driver->head + driver->queued) % driver->capacity] = index;
  driver->queued++;
  slot->state = KU_TASK_DRIVER_QUEUED;
  ku_task_driver_signal_work(driver);
  ku_task_driver_signal(driver);
}
/* All helpers below require the driver mutex. Registry leases protect any
 * atomic control read; an ID by itself never authorizes dereferencing control. */
static uint32_t ku_task_driver_wait_ready_locked(KuTaskDriverSlotV1* child) {
  if (!child->driver_lease.control || child->state == KU_TASK_DRIVER_BUILDING
      || child->state == KU_TASK_DRIVER_RETIRING || child->state == KU_TASK_DRIVER_FREE)
    return KU_TASK_DRIVER_INTERNAL;
  KuTaskControlV1* control = child->driver_lease.control;
  size_t phase = ku_task_control_atomic_load(&control->phase);
  if (phase == KU_TASK_CONTROL_LIVE || ku_task_control_is_requested(phase)
      || ku_task_control_is_publishing(phase) || phase == KU_TASK_CONTROL_COMMITTING)
    return KU_TASK_DRIVER_PENDING;
  if (phase == KU_TASK_CONTROL_CANCELLED || phase == KU_TASK_CONTROL_TIMED_OUT)
    return KU_TASK_DRIVER_WAIT_READY;
  if (!ku_task_control_is_payload_terminal(phase)) return KU_TASK_DRIVER_INTERNAL;
  size_t payload = ku_task_control_atomic_load(&control->payload);
  if (payload == KU_TASK_CONTROL_PAYLOAD_TAKING) return KU_TASK_DRIVER_PENDING;
  if (payload == KU_TASK_CONTROL_PAYLOAD_AVAILABLE || payload == KU_TASK_CONTROL_PAYLOAD_TAKEN
      || payload == KU_TASK_CONTROL_PAYLOAD_DROPPED) return KU_TASK_DRIVER_WAIT_READY;
  return KU_TASK_DRIVER_INTERNAL;
}
static uint32_t ku_task_driver_wait_unlink_locked(KuTaskDriverV1* driver, size_t index) {
  KuTaskDriverSlotV1* parent = &driver->slots[index];
  if (parent->wait_state != KU_TASK_DRIVER_WAIT_ARMED) return KU_TASK_DRIVER_OK;
  KuTaskDriverSlotV1* child = parent->wait_child_slot < driver->capacity
      ? &driver->slots[parent->wait_child_slot] : NULL;
  if (!child || child->state == KU_TASK_DRIVER_FREE
      || child->generation != parent->wait_child_generation
      || child->waiter_parent_slot != index || child->waiter_parent_generation != parent->generation
      || child->waiter_epoch != parent->wait_epoch) {
    driver->fault = KU_TASK_DRIVER_INTERNAL; return KU_TASK_DRIVER_INTERNAL;
  }
  child->waiter_parent_slot = 0; child->waiter_parent_generation = 0; child->waiter_epoch = 0;
  return KU_TASK_DRIVER_OK;
}
static uint32_t ku_task_driver_wait_clear_locked(KuTaskDriverV1* driver, size_t index) {
  KuTaskDriverSlotV1* parent = &driver->slots[index];
  uint32_t result = ku_task_driver_wait_unlink_locked(driver, index);
  parent->wait_state = KU_TASK_DRIVER_WAIT_EMPTY; parent->wait_outcome = 0;
  parent->wait_kind = KU_TASK_DRIVER_WAIT_KIND_EMPTY;
  parent->wait_child_slot = 0; parent->wait_child_generation = 0;
  /* Never reset wait_epoch while this task generation remains resident. */
  return result;
}
static void ku_task_driver_wait_abort_locked(KuTaskDriverV1* driver, size_t index) {
  KuTaskDriverSlotV1* parent = &driver->slots[index];
  if (parent->wait_kind != KU_TASK_DRIVER_WAIT_KIND_RESULT) return;
  if (parent->wait_state == KU_TASK_DRIVER_WAIT_EMPTY
      || (parent->wait_state == KU_TASK_DRIVER_WAIT_NOTIFIED
          && (parent->wait_outcome == KU_TASK_DRIVER_WAIT_ABORTED
              || parent->wait_outcome == KU_TASK_DRIVER_INTERNAL))) return;
  uint32_t unlinked = ku_task_driver_wait_unlink_locked(driver, index);
  parent->wait_state = KU_TASK_DRIVER_WAIT_NOTIFIED;
  parent->wait_outcome = unlinked == KU_TASK_DRIVER_OK ? KU_TASK_DRIVER_WAIT_ABORTED : KU_TASK_DRIVER_INTERNAL;
  ku_task_driver_enqueue(driver, index);
}
static void ku_task_driver_wait_cancel_check_locked(KuTaskDriverV1* driver, size_t index) {
  KuTaskDriverSlotV1* slot = &driver->slots[index];
  if (slot->wait_state == KU_TASK_DRIVER_WAIT_EMPTY
      || slot->wait_kind != KU_TASK_DRIVER_WAIT_KIND_RESULT || !slot->driver_lease.control) return;
  size_t phase = ku_task_control_atomic_load(&slot->driver_lease.control->phase);
  if (ku_task_control_is_publishing(phase) || ku_task_control_is_requested(phase)
      || phase == KU_TASK_CONTROL_CANCELLED || phase == KU_TASK_CONTROL_TIMED_OUT)
    ku_task_driver_wait_abort_locked(driver, index);
}
static void ku_task_driver_wait_complete_locked(KuTaskDriverV1* driver, size_t index, uint32_t ready) {
  KuTaskDriverSlotV1* child = &driver->slots[index];
  if (!child->waiter_epoch) return;
  if (ready == KU_TASK_DRIVER_INTERNAL) driver->fault = KU_TASK_DRIVER_INTERNAL;
  KuTaskDriverSlotV1* parent = child->waiter_parent_slot < driver->capacity
      ? &driver->slots[child->waiter_parent_slot] : NULL;
  if (!parent || parent->state == KU_TASK_DRIVER_FREE || parent->state == KU_TASK_DRIVER_RETIRING
      || parent->generation != child->waiter_parent_generation
      || parent->wait_epoch != child->waiter_epoch || parent->wait_state != KU_TASK_DRIVER_WAIT_ARMED
      || parent->wait_child_slot != index || parent->wait_child_generation != child->generation) {
    driver->fault = KU_TASK_DRIVER_INTERNAL;
  } else {
    if (parent->wait_kind == KU_TASK_DRIVER_WAIT_KIND_CLEANUP_ACK) {
      if (!parent->scope_wait_failure
          && (ready == KU_TASK_DRIVER_INTERNAL || ready == KU_TASK_DRIVER_CLEANUP_TIMEOUT))
        parent->scope_wait_failure = ready;
      if (parent->scope_wait_failure) ready = parent->scope_wait_failure;
    }
    parent->wait_state = KU_TASK_DRIVER_WAIT_NOTIFIED; parent->wait_outcome = ready;
    ku_task_driver_enqueue(driver, child->waiter_parent_slot);
  }
  child->waiter_parent_slot = 0; child->waiter_parent_generation = 0; child->waiter_epoch = 0;
}
static void ku_task_driver_wait_publish_locked(KuTaskDriverV1* driver, size_t index) {
  KuTaskDriverSlotV1* child = &driver->slots[index];
  if (!child->waiter_epoch) return;
  KuTaskDriverSlotV1* parent = child->waiter_parent_slot < driver->capacity
      ? &driver->slots[child->waiter_parent_slot] : NULL;
  if (!parent || parent->generation != child->waiter_parent_generation
      || parent->wait_epoch != child->waiter_epoch || parent->wait_state != KU_TASK_DRIVER_WAIT_ARMED
      || parent->wait_child_slot != index || parent->wait_child_generation != child->generation) {
    ku_task_driver_wait_complete_locked(driver, index, KU_TASK_DRIVER_INTERNAL); return;
  }
  uint32_t ready = parent->wait_kind == KU_TASK_DRIVER_WAIT_KIND_RESULT
      ? ku_task_driver_wait_ready_locked(child)
      : parent->wait_kind == KU_TASK_DRIVER_WAIT_KIND_CLEANUP_ACK
      ? ku_task_driver_cleanup_receipt_status_locked(driver, index, child->generation)
      : KU_TASK_DRIVER_INTERNAL;
  if (ready != KU_TASK_DRIVER_PENDING) ku_task_driver_wait_complete_locked(driver, index, ready);
}
/* Expiring a budget is not failure without a still-pending child. An ACK that
 * was already published wins even if the parent was scheduled after its D. */
static void ku_task_driver_scope_expire_locked(KuTaskDriverV1* driver, size_t index, uint64_t now) {
  KuTaskDriverSlotV1* parent = &driver->slots[index];
  if (!parent->scope_wait_started || parent->scope_deadline_fired || parent->scope_wait_failure
      || parent->cleanup_fault || now < parent->scope_wait_deadline) return;
  parent->scope_deadline_fired = 1;
  if (parent->wait_state != KU_TASK_DRIVER_WAIT_ARMED
      || parent->wait_kind != KU_TASK_DRIVER_WAIT_KIND_CLEANUP_ACK) return;
  KuTaskDriverSlotV1* child = parent->wait_child_slot < driver->capacity
      ? &driver->slots[parent->wait_child_slot] : NULL;
  if (!child || child->generation != parent->wait_child_generation
      || child->waiter_parent_slot != index || child->waiter_parent_generation != parent->generation
      || child->waiter_epoch != parent->wait_epoch) {
    /* A broken pair is not Pending. Keep this parent's durable error latch,
     * and never clear another parent's unmatched incoming registration. */
    driver->fault = KU_TASK_DRIVER_INTERNAL; parent->scope_wait_failure = KU_TASK_DRIVER_INTERNAL;
    parent->wait_state = KU_TASK_DRIVER_WAIT_NOTIFIED; parent->wait_outcome = KU_TASK_DRIVER_INTERNAL;
    ku_task_driver_enqueue(driver, index); return;
  }
  uint32_t ready = ku_task_driver_cleanup_receipt_status_locked(
      driver, parent->wait_child_slot, parent->wait_child_generation);
  if (ready == KU_TASK_DRIVER_PENDING) ready = KU_TASK_DRIVER_CLEANUP_TIMEOUT;
  else if (ready != KU_TASK_DRIVER_CLEANUP_ACK && ready != KU_TASK_DRIVER_INTERNAL)
    ready = KU_TASK_DRIVER_INTERNAL;
  ku_task_driver_wait_complete_locked(driver, parent->wait_child_slot, ready);
}
/* Existing published cancellation/shutdown budgets may only tighten scope D.
 * PUBLISHING is deliberately not a license to read an unpublished deadline. */
static void ku_task_driver_scope_refresh_locked(KuTaskDriverV1* driver, size_t index) {
  KuTaskDriverSlotV1* slot = &driver->slots[index];
  if (!slot->scope_wait_started || slot->scope_wait_failure || slot->cleanup_fault) return;
  uint64_t deadline = ku_task_driver_min(slot->scope_wait_deadline, slot->cancel_deadline);
  if (driver->closing) deadline = ku_task_driver_min(deadline, driver->shutdown_deadline);
  if (slot->driver_lease.control) {
    KuTaskControlV1* control = slot->driver_lease.control;
    size_t phase = ku_task_control_atomic_load(&control->phase);
    if (ku_task_control_is_requested(phase) || phase == KU_TASK_CONTROL_CANCELLED
        || phase == KU_TASK_CONTROL_TIMED_OUT)
      deadline = ku_task_driver_min(deadline, ku_task_control_cleanup_deadline(control));
  }
  slot->scope_wait_deadline = deadline;
  ku_task_driver_arm_deadline(driver, slot);
  uint64_t now = driver->clock_fault ? 0 : ku_task_driver_now_ms();
  if (now == UINT64_MAX) { ku_task_driver_enter_clock_fault(driver); return; }
  ku_task_driver_scope_expire_locked(driver, index, now);
}
/* Metadata only: safe on terminal paths even after frame storage was dropped.
 * Do not read or zero a borrowed receipt/token here. The allocation remains
 * protected by the registry until this registration has been forgotten. */
static void ku_task_driver_scope_unregister_locked(KuTaskDriverSlotV1* parent) {
  parent->scope_session_active = 0; parent->scope_session_final = 0;
  parent->scope_session_id = 0;
  parent->scope_receipts = NULL; parent->scope_receipt_capacity = 0;
  parent->scope_expected_mask = 0;
}
/* Address-only membership preflight, before any typed receipt field read. */
static int ku_task_driver_scope_contains_locked(
    const KuTaskDriverSlotV1* parent, const KuTaskDriverCleanupReceiptV1* receipt) {
  if (!parent->scope_session_active) return 1;
  if (!parent->scope_receipts || !parent->scope_receipt_capacity
      || parent->scope_receipt_capacity > 64u) return 0;
  uintptr_t base = (uintptr_t)parent->scope_receipts, address = (uintptr_t)receipt;
  if (address < base) return 0;
  uintptr_t offset = address - base;
  size_t bytes = parent->scope_receipt_capacity * sizeof(*receipt);
  if (offset >= bytes || offset % sizeof(*receipt)) return 0;
  size_t index = (size_t)(offset / sizeof(*receipt));
  return (parent->scope_expected_mask & ((uint64_t)1u << index)) != 0;
}
static int ku_task_driver_scope_receipt_empty(const KuTaskDriverCleanupReceiptV1* receipt) {
  return !receipt->driver && !receipt->slot && !receipt->generation && !receipt->abi_version;
}
/* Whole-manifest field validation, after callers validate its stable span.
 * Exact duplicate IDs are not two children; at most 64*63/2 comparisons.
 * This is not an ACK/status read: a quarantined old child must not prevent
 * registering still-owned outer children. Old generations may remain valid
 * after disposal/reuse, but a generation not yet issued cannot be valid. */
static uint32_t ku_task_driver_scope_manifest_locked(
    const KuTaskDriverTicketV1* parent_ticket, const KuTaskDriverSlotV1* parent) {
  KuTaskDriverV1* driver = parent_ticket->driver;
  size_t capacity = parent->scope_receipt_capacity;
  const KuTaskDriverCleanupReceiptV1* receipts = parent->scope_receipts;
  if (capacity < 64u && (parent->scope_expected_mask >> capacity))
    return KU_TASK_DRIVER_INVALID_ARGUMENT;
  for (size_t i = 0; i < capacity; i++) {
    if (!(parent->scope_expected_mask & ((uint64_t)1u << i))) continue;
    const KuTaskDriverCleanupReceiptV1* receipt = &receipts[i];
    if (ku_task_driver_scope_receipt_empty(receipt)) continue;
    if (receipt->driver != driver || receipt->abi_version != KU_TASK_DRIVER_ABI_VERSION
        || !receipt->generation || receipt->slot >= driver->capacity
        || (receipt->slot == parent_ticket->slot && receipt->generation == parent_ticket->generation))
      return KU_TASK_DRIVER_INVALID_ARGUMENT;
    if (receipt->generation > driver->slots[receipt->slot].generation)
      return KU_TASK_DRIVER_INVALID_ARGUMENT;
    for (size_t j = 0; j < i; j++) {
      if ((parent->scope_expected_mask & ((uint64_t)1u << j))
          && receipts[j].driver == receipt->driver && receipts[j].slot == receipt->slot
          && receipts[j].generation == receipt->generation)
        return KU_TASK_DRIVER_INVALID_ARGUMENT;
    }
  }
  return KU_TASK_DRIVER_OK;
}
static uint32_t ku_task_driver_scope_begin(
    const KuTaskDriverTicketV1* parent_ticket, uint64_t scope_id,
    const KuTaskDriverCleanupReceiptV1* receipts, size_t capacity,
    uint64_t expected_mask, uint64_t absolute_deadline, KuTaskDriverScopeTokenV1* output) {
  uint32_t checked = ku_task_driver_check_ticket(parent_ticket);
  if (checked != KU_TASK_DRIVER_OK) return checked;
  if (!capacity || capacity > 64u || absolute_deadline == UINT64_MAX
      || (capacity < 64u && (expected_mask >> capacity))) return KU_TASK_DRIVER_INVALID_ARGUMENT;
  KuTaskDriverV1* driver = parent_ticket->driver;
  size_t bytes = capacity * sizeof(*receipts);
  if (!ku_task_driver_external_storage(driver, parent_ticket, sizeof(*parent_ticket),
                                       KU_TASK_FRAME_ALIGNOF(KuTaskDriverTicketV1))
      || !ku_task_driver_external_storage(driver, receipts, bytes,
                                          KU_TASK_FRAME_ALIGNOF(KuTaskDriverCleanupReceiptV1))
      || !ku_task_driver_external_storage(driver, output, sizeof(*output),
                                          KU_TASK_FRAME_ALIGNOF(KuTaskDriverScopeTokenV1))
      || ku_task_frame_ranges_overlap(receipts, bytes, parent_ticket, sizeof(*parent_ticket))
      || ku_task_frame_ranges_overlap(receipts, bytes, output, sizeof(*output))
      || ku_task_frame_ranges_overlap(output, sizeof(*output), parent_ticket, sizeof(*parent_ticket)))
    return KU_TASK_DRIVER_INVALID_ARGUMENT;
  if (ku_task_driver_lock(driver)) return KU_TASK_DRIVER_INTERNAL;
  KuTaskDriverSlotV1* parent = ku_task_driver_find(parent_ticket);
  uint32_t result = !parent ? KU_TASK_DRIVER_STALE
      : parent->state != KU_TASK_DRIVER_RUNNING || !parent->driver_lease.control
          || parent->binding != parent->driver_lease.control ? KU_TASK_DRIVER_INVALID_STATE
      : parent->cleanup_fault ? parent->cleanup_fault : KU_TASK_DRIVER_OK;
  if (result == KU_TASK_DRIVER_OK
      && (ku_task_frame_ranges_overlap(receipts, bytes, parent->driver_lease.control, sizeof(KuTaskControlV1))
          || ku_task_frame_ranges_overlap(output, sizeof(*output), parent->driver_lease.control, sizeof(KuTaskControlV1))
          || ku_task_frame_ranges_overlap(parent_ticket, sizeof(*parent_ticket),
                                          parent->driver_lease.control, sizeof(KuTaskControlV1))))
    result = KU_TASK_DRIVER_INVALID_ARGUMENT;
  /* Known shorter-header aliases are rejected before any typed output read. */
  if (result == KU_TASK_DRIVER_OK
      && (output->driver || output->parent_slot || output->parent_generation || output->scope_id || output->epoch
          || parent->scope_session_active || parent->scope_session_final
          || parent->scope_receipts || parent->scope_receipt_capacity
          || parent->scope_expected_mask || parent->scope_wait_started
          || parent->wait_state != KU_TASK_DRIVER_WAIT_EMPTY))
    result = KU_TASK_DRIVER_INVALID_STATE;
  if (result == KU_TASK_DRIVER_OK && parent->scope_session_epoch == UINT64_MAX)
    result = KU_TASK_DRIVER_LIMIT;
  if (result == KU_TASK_DRIVER_OK) {
    for (size_t i = 0; i < capacity; i++) {
      if ((expected_mask & ((uint64_t)1u << i)) && !ku_task_driver_scope_receipt_empty(&receipts[i])) {
        result = KU_TASK_DRIVER_INVALID_STATE; break;
      }
    }
  }
  if (result == KU_TASK_DRIVER_OK && driver->fault) result = driver->fault;
  if (result == KU_TASK_DRIVER_OK
      && (driver->closing || parent->cancel_pending
          || ku_task_control_atomic_load(&parent->driver_lease.control->phase) != KU_TASK_CONTROL_LIVE))
    result = KU_TASK_DRIVER_WAIT_ABORTED;
  if (result == KU_TASK_DRIVER_OK) {
    /* No clocks/callbacks or fallible ownership changes after registration.
     * Deadline notification may record an OS fault but cannot unpublish this
     * accepted session. A later overlapping cancel also leaves it registered. */
    parent->scope_session_epoch++;
    parent->scope_session_active = 1; parent->scope_session_final = 0;
    parent->scope_session_id = scope_id;
    parent->scope_receipts = receipts; parent->scope_receipt_capacity = capacity;
    parent->scope_expected_mask = expected_mask;
    parent->scope_wait_started = 1; parent->scope_deadline_fired = 0;
    parent->scope_wait_failure = 0; parent->scope_wait_deadline = absolute_deadline;
    *output = (KuTaskDriverScopeTokenV1){
      driver, parent_ticket->slot, parent_ticket->generation, scope_id, parent->scope_session_epoch
    };
    ku_task_driver_arm_deadline(driver, parent);
  }
  ku_task_driver_unlock(driver);
  return result;
}
/* Trusted whole-function exit metadata, never normal Continue. No clock read,
 * control CAS, owner transfer, ACK consumption, callback or new Task progress
 * event. Tightening the cached timer only notifies work sleepers to recheck D.
 * Registry protects atomic control access; sole executor protects the span.
 * Every rejection precedes any output or runtime mutation. */
static uint32_t ku_task_driver_scope_promote_final(
    const KuTaskDriverTicketV1* parent_ticket, const KuTaskDriverScopeTokenV1* token,
    uint64_t additional_expected_mask, uint64_t requested_deadline, uint64_t* effective_deadline) {
  uint32_t checked = ku_task_driver_check_ticket(parent_ticket);
  if (checked != KU_TASK_DRIVER_OK) return checked;
  if (requested_deadline == UINT64_MAX) return KU_TASK_DRIVER_INVALID_ARGUMENT;
  KuTaskDriverV1* driver = parent_ticket->driver;
  if (!ku_task_driver_external_storage(driver, parent_ticket, sizeof(*parent_ticket),
                                       KU_TASK_FRAME_ALIGNOF(KuTaskDriverTicketV1))
      || !ku_task_driver_external_storage(driver, token, sizeof(*token),
                                          KU_TASK_FRAME_ALIGNOF(KuTaskDriverScopeTokenV1))
      || !ku_task_driver_external_storage(driver, effective_deadline, sizeof(*effective_deadline),
                                          KU_TASK_FRAME_ALIGNOF(uint64_t))
      || ku_task_frame_ranges_overlap(token, sizeof(*token), parent_ticket, sizeof(*parent_ticket))
      || ku_task_frame_ranges_overlap(effective_deadline, sizeof(*effective_deadline), token, sizeof(*token))
      || ku_task_frame_ranges_overlap(effective_deadline, sizeof(*effective_deadline),
                                      parent_ticket, sizeof(*parent_ticket)))
    return KU_TASK_DRIVER_INVALID_ARGUMENT;
  if (ku_task_driver_lock(driver)) return KU_TASK_DRIVER_INTERNAL;
  KuTaskDriverSlotV1* parent = ku_task_driver_find(parent_ticket);
  uint32_t result = !parent ? KU_TASK_DRIVER_STALE
      : parent->state != KU_TASK_DRIVER_RUNNING || !parent->driver_lease.control
          || parent->binding != parent->driver_lease.control ? KU_TASK_DRIVER_INVALID_STATE
      : parent->cleanup_fault ? parent->cleanup_fault : KU_TASK_DRIVER_OK;
  size_t capacity = parent ? parent->scope_receipt_capacity : 0;
  const KuTaskDriverCleanupReceiptV1* receipts = parent ? parent->scope_receipts : NULL;
  size_t bytes = capacity <= 64u ? capacity * sizeof(*receipts) : 0;
  if (result == KU_TASK_DRIVER_OK
      && (!parent->scope_session_active || !parent->scope_wait_started
          || !capacity || capacity > 64u || !receipts)) result = KU_TASK_DRIVER_STALE;
  if (result == KU_TASK_DRIVER_OK
      && (parent->scope_session_active != 1u || parent->scope_session_final > 1u
          || parent->scope_wait_started != 1u || parent->scope_wait_deadline == UINT64_MAX))
    result = KU_TASK_DRIVER_INVALID_STATE;
  if (result == KU_TASK_DRIVER_OK
      && ((capacity < 64u && (additional_expected_mask >> capacity))
          || !ku_task_driver_external_storage(driver, receipts, bytes,
                                              KU_TASK_FRAME_ALIGNOF(KuTaskDriverCleanupReceiptV1))
          || ku_task_frame_ranges_overlap(token, sizeof(*token), receipts, bytes)
          || ku_task_frame_ranges_overlap(parent_ticket, sizeof(*parent_ticket), receipts, bytes)
          || ku_task_frame_ranges_overlap(effective_deadline, sizeof(*effective_deadline), receipts, bytes)
          || ku_task_frame_ranges_overlap(token, sizeof(*token), parent->driver_lease.control, sizeof(KuTaskControlV1))
          || ku_task_frame_ranges_overlap(receipts, bytes, parent->driver_lease.control, sizeof(KuTaskControlV1))
          || ku_task_frame_ranges_overlap(effective_deadline, sizeof(*effective_deadline),
                                          parent->driver_lease.control, sizeof(KuTaskControlV1))
          || ku_task_frame_ranges_overlap(parent_ticket, sizeof(*parent_ticket),
                                          parent->driver_lease.control, sizeof(KuTaskControlV1))))
    result = KU_TASK_DRIVER_INVALID_ARGUMENT;
  if (result == KU_TASK_DRIVER_OK
      && (token->driver != driver || token->parent_slot != parent_ticket->slot
          || token->parent_generation != parent_ticket->generation || !token->epoch
          || token->epoch != parent->scope_session_epoch || token->scope_id != parent->scope_session_id))
    result = KU_TASK_DRIVER_STALE;
  if (result == KU_TASK_DRIVER_OK)
    result = ku_task_driver_scope_manifest_locked(parent_ticket, parent);
  if (result == KU_TASK_DRIVER_OK) {
    uint64_t added = additional_expected_mask & ~parent->scope_expected_mask;
    for (size_t i = 0; i < capacity; i++) {
      if ((added & ((uint64_t)1u << i)) && !ku_task_driver_scope_receipt_empty(&receipts[i])) {
        result = KU_TASK_DRIVER_INVALID_STATE; break;
      }
    }
  }
  if (result == KU_TASK_DRIVER_OK) {
    KuTaskControlV1* control = parent->driver_lease.control;
    size_t phase = ku_task_control_atomic_load(&control->phase);
    if (phase != KU_TASK_CONTROL_LIVE && !ku_task_control_is_requested(phase)
        && !ku_task_control_is_publishing(phase)) result = KU_TASK_DRIVER_INVALID_STATE;
    else {
      uint64_t deadline = ku_task_driver_min(parent->scope_wait_deadline, requested_deadline);
      deadline = ku_task_driver_min(deadline, parent->cancel_deadline);
      if (driver->closing) deadline = ku_task_driver_min(deadline, driver->shutdown_deadline);
      if (ku_task_control_is_requested(phase))
        deadline = ku_task_driver_min(deadline, ku_task_control_cleanup_deadline(control));
      /* No fallible work follows. A concurrent publication/shorter request
       * must be observed by a later wrapper/adapter deadline-min update.
       * Preserve fired/failure, old wait links/epochs and cancellation winner.
       * Neither sticky driver fault nor scope failure hides outer ownership. */
      parent->scope_expected_mask |= additional_expected_mask;
      parent->scope_session_final = 1;
      parent->scope_wait_deadline = deadline;
      *effective_deadline = deadline;
      ku_task_driver_arm_deadline(driver, parent);
    }
  }
  ku_task_driver_unlock(driver);
  return result;
}
static uint32_t ku_task_driver_scope_end(
    const KuTaskDriverTicketV1* parent_ticket, KuTaskDriverScopeTokenV1* token) {
  uint32_t checked = ku_task_driver_check_ticket(parent_ticket);
  if (checked != KU_TASK_DRIVER_OK) return checked;
  KuTaskDriverV1* driver = parent_ticket->driver;
  if (!ku_task_driver_external_storage(driver, parent_ticket, sizeof(*parent_ticket),
                                       KU_TASK_FRAME_ALIGNOF(KuTaskDriverTicketV1))
      || !ku_task_driver_external_storage(driver, token, sizeof(*token),
                                          KU_TASK_FRAME_ALIGNOF(KuTaskDriverScopeTokenV1))
      || ku_task_frame_ranges_overlap(token, sizeof(*token), parent_ticket, sizeof(*parent_ticket)))
    return KU_TASK_DRIVER_INVALID_ARGUMENT;
  if (ku_task_driver_lock(driver)) return KU_TASK_DRIVER_INTERNAL;
  KuTaskDriverSlotV1* parent = ku_task_driver_find(parent_ticket);
  uint32_t result = !parent ? KU_TASK_DRIVER_STALE
      : parent->state != KU_TASK_DRIVER_RUNNING || !parent->driver_lease.control
          || parent->binding != parent->driver_lease.control ? KU_TASK_DRIVER_INVALID_STATE
      : parent->cleanup_fault ? parent->cleanup_fault : KU_TASK_DRIVER_OK;
  size_t capacity = parent ? parent->scope_receipt_capacity : 0;
  const KuTaskDriverCleanupReceiptV1* receipts = parent ? parent->scope_receipts : NULL;
  size_t bytes = capacity <= 64u ? capacity * sizeof(*receipts) : 0;
  if (result == KU_TASK_DRIVER_OK
      && (!parent->scope_session_active || !parent->scope_wait_started
          || !capacity || capacity > 64u || !receipts)) result = KU_TASK_DRIVER_STALE;
  if (result == KU_TASK_DRIVER_OK
      && (!ku_task_driver_external_storage(driver, receipts, bytes,
                                           KU_TASK_FRAME_ALIGNOF(KuTaskDriverCleanupReceiptV1))
          || ku_task_frame_ranges_overlap(token, sizeof(*token), receipts, bytes)
          || ku_task_frame_ranges_overlap(parent_ticket, sizeof(*parent_ticket), receipts, bytes)
          || ku_task_frame_ranges_overlap(token, sizeof(*token), parent->driver_lease.control, sizeof(KuTaskControlV1))
          || ku_task_frame_ranges_overlap(receipts, bytes, parent->driver_lease.control, sizeof(KuTaskControlV1))
          || ku_task_frame_ranges_overlap(parent_ticket, sizeof(*parent_ticket),
                                          parent->driver_lease.control, sizeof(KuTaskControlV1))))
    result = KU_TASK_DRIVER_INVALID_ARGUMENT;
  if (result == KU_TASK_DRIVER_OK
      && (token->driver != driver || token->parent_slot != parent_ticket->slot
          || token->parent_generation != parent_ticket->generation || !token->epoch
          || token->epoch != parent->scope_session_epoch || token->scope_id != parent->scope_session_id))
    result = KU_TASK_DRIVER_STALE;
  if (result == KU_TASK_DRIVER_OK
      && (parent->scope_session_active != 1u || parent->scope_session_final
          || parent->wait_state != KU_TASK_DRIVER_WAIT_EMPTY))
    result = KU_TASK_DRIVER_INVALID_STATE;
  if (result == KU_TASK_DRIVER_OK)
    result = ku_task_driver_scope_manifest_locked(parent_ticket, parent);
  int pending = 0;
  if (result == KU_TASK_DRIVER_OK) {
    for (size_t i = 0; i < capacity; i++) {
      if (!(parent->scope_expected_mask & ((uint64_t)1u << i))) continue;
      const KuTaskDriverCleanupReceiptV1* receipt = &receipts[i];
      if (ku_task_driver_scope_receipt_empty(receipt)) { pending = 1; continue; }
      uint32_t ready = ku_task_driver_cleanup_receipt_status_locked(driver, receipt->slot, receipt->generation);
      if (ready == KU_TASK_DRIVER_PENDING) pending = 1;
      else if (ready != KU_TASK_DRIVER_CLEANUP_ACK) { result = ready; break; }
    }
  }
  if (result == KU_TASK_DRIVER_OK && parent->scope_wait_failure) result = parent->scope_wait_failure;
  if (result == KU_TASK_DRIVER_OK && driver->fault) result = driver->fault;
  if (result == KU_TASK_DRIVER_OK
      && (driver->closing || parent->cancel_pending
          || ku_task_control_atomic_load(&parent->driver_lease.control->phase) != KU_TASK_CONTROL_LIVE))
    result = KU_TASK_DRIVER_WAIT_ABORTED;
  if (result == KU_TASK_DRIVER_OK && pending) {
    /* All-ACK success needs no time sample. A fired timer without a still
     * pending child is not itself failure; a previously latched failure is. */
    ku_task_driver_scope_refresh_locked(driver, parent_ticket->slot);
    if (driver->fault) result = driver->fault;
    else if (parent->scope_wait_failure) result = parent->scope_wait_failure;
    else if (driver->closing || parent->cancel_pending
        || ku_task_control_atomic_load(&parent->driver_lease.control->phase) != KU_TASK_CONTROL_LIVE)
      result = KU_TASK_DRIVER_WAIT_ABORTED;
    else if (parent->scope_deadline_fired) {
      parent->scope_wait_failure = KU_TASK_DRIVER_CLEANUP_TIMEOUT;
      result = parent->scope_wait_failure;
    } else result = KU_TASK_DRIVER_PENDING;
  }
  if (result == KU_TASK_DRIVER_OK) {
    /* Scope close linearizes at this final acquire read, NOT at mutex lock.
     * Tests can bracket this exact load without replacing the atomic result. */
    size_t close_phase = ku_task_control_atomic_load(&parent->driver_lease.control->phase);
    if (close_phase != KU_TASK_CONTROL_LIVE) result = KU_TASK_DRIVER_WAIT_ABORTED;
    else {
      /* Infallible metadata only after LIVE; do not add clock/callback/drop.
       * Keep wait/session epochs and all external cancellation state intact.
       * next_deadline may retain a conservative stale minimum: its existing
       * one-shot deadline scan removes it, without a close-time full scan. */
      ku_task_driver_scope_unregister_locked(parent);
      parent->scope_wait_started = 0; parent->scope_deadline_fired = 0;
      parent->scope_wait_failure = 0; parent->scope_wait_deadline = UINT64_MAX;
      *token = (KuTaskDriverScopeTokenV1){0};
    }
  }
  if (result == KU_TASK_DRIVER_INTERNAL) driver->fault = result;
  ku_task_driver_unlock(driver);
  return result;
}
static uint32_t ku_task_driver_wait_cycle_locked(
    KuTaskDriverV1* driver, size_t parent_index, size_t child_index) {
  size_t index = child_index;
  for (size_t step = 0; step < driver->capacity; step++) {
    if (index == parent_index) return KU_TASK_DRIVER_WAIT_CYCLE;
    KuTaskDriverSlotV1* cursor = &driver->slots[index];
    /* NOTIFIED is a durable event latch, no longer a blocking graph edge. */
    if (cursor->wait_state != KU_TASK_DRIVER_WAIT_ARMED) return KU_TASK_DRIVER_OK;
    if (cursor->wait_kind != KU_TASK_DRIVER_WAIT_KIND_RESULT
        && cursor->wait_kind != KU_TASK_DRIVER_WAIT_KIND_CLEANUP_ACK) return KU_TASK_DRIVER_INTERNAL;
    if (cursor->wait_child_slot >= driver->capacity) return KU_TASK_DRIVER_INTERNAL;
    KuTaskDriverSlotV1* next = &driver->slots[cursor->wait_child_slot];
    if (next->state == KU_TASK_DRIVER_FREE || next->state == KU_TASK_DRIVER_BUILDING
        || next->state == KU_TASK_DRIVER_RETIRING
        || next->generation != cursor->wait_child_generation
        || next->waiter_parent_slot != index || next->waiter_parent_generation != cursor->generation
        || next->waiter_epoch != cursor->wait_epoch) return KU_TASK_DRIVER_INTERNAL;
    index = cursor->wait_child_slot;
  }
  return KU_TASK_DRIVER_WAIT_CYCLE;
}
static uint32_t ku_task_driver_wait_arm(
    const KuTaskDriverTicketV1* parent_ticket, const KuTaskDriverTicketV1* child_ticket,
    const KuTaskControlLeaseV1* child_owner_lease, KuTaskDriverWaitTokenV1* output) {
  uint32_t checked = ku_task_driver_check_ticket(parent_ticket);
  if (checked != KU_TASK_DRIVER_OK) return checked;
  checked = ku_task_driver_check_ticket(child_ticket);
  if (checked != KU_TASK_DRIVER_OK) return checked;
  if (parent_ticket->driver != child_ticket->driver) return KU_TASK_DRIVER_INVALID_ARGUMENT;
  checked = ku_task_control_check_lease(child_owner_lease);
  if (checked != KU_TASK_CONTROL_OK) return checked;
  KuTaskDriverV1* driver = parent_ticket->driver;
  if (!ku_task_driver_external_storage(driver, output, sizeof(*output), KU_TASK_FRAME_ALIGNOF(KuTaskDriverWaitTokenV1))
      || ku_task_frame_ranges_overlap(output, sizeof(*output), parent_ticket, sizeof(*parent_ticket))
      || ku_task_frame_ranges_overlap(output, sizeof(*output), child_ticket, sizeof(*child_ticket))
      || ku_task_frame_ranges_overlap(output, sizeof(*output), child_owner_lease, sizeof(*child_owner_lease))
      || ku_task_frame_ranges_overlap(output, sizeof(*output), child_owner_lease->control, sizeof(KuTaskControlV1)))
    return KU_TASK_DRIVER_INVALID_ARGUMENT;
  if (ku_task_driver_lock(driver)) return KU_TASK_DRIVER_INTERNAL;
  KuTaskDriverSlotV1* parent = ku_task_driver_find(parent_ticket);
  KuTaskDriverSlotV1* child = ku_task_driver_find(child_ticket);
  uint32_t result = !parent || !child ? KU_TASK_DRIVER_STALE
      : parent->state != KU_TASK_DRIVER_RUNNING || !parent->driver_lease.control || parent->scope_session_active
      || child->state == KU_TASK_DRIVER_BUILDING || child->state == KU_TASK_DRIVER_RETIRING
      || child->owner_location != KU_TASK_DRIVER_OWNER_USER ? KU_TASK_DRIVER_INVALID_STATE
      : child->binding != child_owner_lease->control ? KU_TASK_DRIVER_INVALID_ARGUMENT : KU_TASK_DRIVER_OK;
  if (result == KU_TASK_DRIVER_OK
      && ku_task_frame_ranges_overlap(output, sizeof(*output), parent->driver_lease.control, sizeof(KuTaskControlV1)))
    result = KU_TASK_DRIVER_INVALID_ARGUMENT;
  /* Reject every known short-header alias before reading a typed token. */
  if (result == KU_TASK_DRIVER_OK
      && (output->driver || output->parent_slot || output->parent_generation || output->epoch))
    result = KU_TASK_DRIVER_INVALID_STATE;
  if (result == KU_TASK_DRIVER_OK
      && (parent->wait_state != KU_TASK_DRIVER_WAIT_EMPTY || child->waiter_epoch))
    result = KU_TASK_DRIVER_INVALID_STATE;
  if (result == KU_TASK_DRIVER_OK
      && ku_task_control_atomic_load(&parent->driver_lease.control->phase) != KU_TASK_CONTROL_LIVE)
    result = KU_TASK_DRIVER_WAIT_ABORTED;
  if (result == KU_TASK_DRIVER_OK && parent->wait_epoch == UINT64_MAX) result = KU_TASK_DRIVER_LIMIT;
  if (result == KU_TASK_DRIVER_OK)
    result = ku_task_driver_wait_cycle_locked(driver, parent_ticket->slot, child_ticket->slot);
  if (result == KU_TASK_DRIVER_OK) {
    result = ku_task_driver_wait_ready_locked(child);
    if (result == KU_TASK_DRIVER_WAIT_READY) {
      /* A caller may retry take on its next quantum; no empty WAIT is parked. */
      parent->intent = KU_TASK_DRIVER_YIELD;
    } else if (result == KU_TASK_DRIVER_PENDING) {
      parent->wait_epoch++;
      parent->wait_state = KU_TASK_DRIVER_WAIT_ARMED; parent->wait_outcome = KU_TASK_DRIVER_PENDING;
      parent->wait_kind = KU_TASK_DRIVER_WAIT_KIND_RESULT;
      parent->wait_child_slot = child_ticket->slot; parent->wait_child_generation = child_ticket->generation;
      child->waiter_parent_slot = parent_ticket->slot; child->waiter_parent_generation = parent_ticket->generation;
      child->waiter_epoch = parent->wait_epoch;
      *output = (KuTaskDriverWaitTokenV1){ driver, parent_ticket->slot, parent_ticket->generation, parent->wait_epoch };
      parent->intent = KU_TASK_DRIVER_WAIT;
      /* R2 atomics publish outside this mutex. Recheck both cancellation and
       * readiness; later publications must visit their post-store wrapper hook. */
      ku_task_driver_wait_cancel_check_locked(driver, parent_ticket->slot);
      ku_task_driver_wait_publish_locked(driver, child_ticket->slot);
    }
  }
  if (result == KU_TASK_DRIVER_INTERNAL) driver->fault = KU_TASK_DRIVER_INTERNAL;
  ku_task_driver_unlock(driver);
  return result;
}
static uint32_t ku_task_driver_cleanup_wait_arm(
    const KuTaskDriverTicketV1* parent_ticket, const KuTaskDriverCleanupReceiptV1* receipt,
    uint64_t absolute_scope_deadline, KuTaskDriverWaitTokenV1* output) {
  uint32_t checked = ku_task_driver_check_ticket(parent_ticket);
  if (checked != KU_TASK_DRIVER_OK) return checked;
  KuTaskDriverV1* driver = parent_ticket->driver;
  if (absolute_scope_deadline == UINT64_MAX
      || !ku_task_driver_external_storage(driver, receipt, sizeof(*receipt),
                                          KU_TASK_FRAME_ALIGNOF(KuTaskDriverCleanupReceiptV1))
      || !ku_task_driver_external_storage(driver, output, sizeof(*output), KU_TASK_FRAME_ALIGNOF(KuTaskDriverWaitTokenV1))
      || ku_task_frame_ranges_overlap(output, sizeof(*output), parent_ticket, sizeof(*parent_ticket))
      || ku_task_frame_ranges_overlap(output, sizeof(*output), receipt, sizeof(*receipt)))
    return KU_TASK_DRIVER_INVALID_ARGUMENT;
  if (ku_task_driver_lock(driver)) return KU_TASK_DRIVER_INTERNAL;
  KuTaskDriverSlotV1* parent = ku_task_driver_find(parent_ticket);
  uint32_t result = !parent ? KU_TASK_DRIVER_STALE
      : parent->state != KU_TASK_DRIVER_RUNNING || !parent->driver_lease.control
      ? KU_TASK_DRIVER_INVALID_STATE : parent->cleanup_fault ? parent->cleanup_fault : KU_TASK_DRIVER_OK;
  /* A known active manifest can reject a foreign/short object by address,
   * before reading it as a receipt. The legacy path keeps its complete raw
   * receipt-storage contract and the same value validation below. */
  if (result == KU_TASK_DRIVER_OK && parent->scope_session_active
      && (!ku_task_driver_scope_contains_locked(parent, receipt)
          || ku_task_frame_ranges_overlap(output, sizeof(*output), parent->scope_receipts,
                  parent->scope_receipt_capacity * sizeof(*parent->scope_receipts))))
    result = KU_TASK_DRIVER_INVALID_ARGUMENT;
  if (result == KU_TASK_DRIVER_OK) {
    result = ku_task_driver_check_cleanup_receipt(receipt);
    if (result == KU_TASK_DRIVER_OK && receipt->driver != driver)
      result = KU_TASK_DRIVER_INVALID_ARGUMENT;
  }
  KuTaskDriverSlotV1* child = result == KU_TASK_DRIVER_OK ? &driver->slots[receipt->slot] : NULL;
  if (result == KU_TASK_DRIVER_OK
      && (ku_task_frame_ranges_overlap(output, sizeof(*output), parent->driver_lease.control, sizeof(KuTaskControlV1))
          || (child->generation == receipt->generation && child->binding
              && ku_task_frame_ranges_overlap(output, sizeof(*output), child->binding, sizeof(KuTaskControlV1)))))
    result = KU_TASK_DRIVER_INVALID_ARGUMENT;
  if (result == KU_TASK_DRIVER_OK
      && (output->driver || output->parent_slot || output->parent_generation || output->epoch
          || parent->wait_state != KU_TASK_DRIVER_WAIT_EMPTY)) result = KU_TASK_DRIVER_INVALID_STATE;
  if (result == KU_TASK_DRIVER_OK) {
    size_t phase = ku_task_control_atomic_load(&parent->driver_lease.control->phase);
    if (phase != KU_TASK_CONTROL_LIVE && !ku_task_control_is_requested(phase)
        && !ku_task_control_is_publishing(phase)) result = KU_TASK_DRIVER_INVALID_STATE;
  }
  if (result == KU_TASK_DRIVER_OK && parent->scope_wait_failure) result = parent->scope_wait_failure;
  uint32_t ready = KU_TASK_DRIVER_PENDING;
  if (result == KU_TASK_DRIVER_OK) {
    ready = ku_task_driver_cleanup_receipt_status_locked(driver, receipt->slot, receipt->generation);
    if (ready != KU_TASK_DRIVER_PENDING && ready != KU_TASK_DRIVER_CLEANUP_ACK
        && ready != KU_TASK_DRIVER_INTERNAL) result = ready;
    if (ready == KU_TASK_DRIVER_PENDING) {
      if (!child->driver_lease.control || child->waiter_epoch) result = KU_TASK_DRIVER_INVALID_STATE;
      else if (parent->wait_epoch == UINT64_MAX) result = KU_TASK_DRIVER_LIMIT;
      else result = ku_task_driver_wait_cycle_locked(driver, parent_ticket->slot, receipt->slot);
    }
  }
  if (result == KU_TASK_DRIVER_OK) {
    if (!parent->scope_wait_started) {
      parent->scope_wait_started = 1; parent->scope_wait_deadline = absolute_scope_deadline;
    } else parent->scope_wait_deadline = ku_task_driver_min(parent->scope_wait_deadline, absolute_scope_deadline);
    /* Even an immediate ACK binds D before the next sibling can be observed. */
    ku_task_driver_scope_refresh_locked(driver, parent_ticket->slot);
    if (ready == KU_TASK_DRIVER_INTERNAL) {
      parent->scope_wait_failure = KU_TASK_DRIVER_INTERNAL; result = KU_TASK_DRIVER_INTERNAL;
    } else if (ready == KU_TASK_DRIVER_CLEANUP_ACK) result = ready;
    else if (parent->scope_deadline_fired) {
      parent->scope_wait_failure = KU_TASK_DRIVER_CLEANUP_TIMEOUT; result = parent->scope_wait_failure;
    } else {
      parent->wait_epoch++;
      parent->wait_state = KU_TASK_DRIVER_WAIT_ARMED; parent->wait_outcome = KU_TASK_DRIVER_PENDING;
      parent->wait_kind = KU_TASK_DRIVER_WAIT_KIND_CLEANUP_ACK;
      parent->wait_child_slot = receipt->slot; parent->wait_child_generation = receipt->generation;
      child->waiter_parent_slot = parent_ticket->slot; child->waiter_parent_generation = parent->generation;
      child->waiter_epoch = parent->wait_epoch;
      *output = (KuTaskDriverWaitTokenV1){ driver, parent_ticket->slot, parent->generation, parent->wait_epoch };
      parent->intent = KU_TASK_DRIVER_WAIT;
      ku_task_driver_scope_refresh_locked(driver, parent_ticket->slot);
      ku_task_driver_wait_publish_locked(driver, receipt->slot);
      result = KU_TASK_DRIVER_PENDING;
    }
  }
  if (result == KU_TASK_DRIVER_INTERNAL) driver->fault = KU_TASK_DRIVER_INTERNAL;
  ku_task_driver_unlock(driver);
  return result;
}
static uint32_t ku_task_driver_wait_check_token(const KuTaskDriverWaitTokenV1* token) {
  if (!ku_task_frame_storage_valid(token, sizeof(*token), sizeof(*token), KU_TASK_FRAME_ALIGNOF(KuTaskDriverWaitTokenV1)))
    return KU_TASK_DRIVER_INVALID_ARGUMENT;
  return ku_task_driver_check(token->driver);
}
static KuTaskDriverSlotV1* ku_task_driver_wait_find_locked(const KuTaskDriverWaitTokenV1* token) {
  KuTaskDriverV1* driver = token->driver;
  if (token->parent_slot >= driver->capacity || !token->parent_generation || !token->epoch) return NULL;
  KuTaskDriverSlotV1* parent = &driver->slots[token->parent_slot];
  return parent->state != KU_TASK_DRIVER_FREE && parent->state != KU_TASK_DRIVER_RETIRING
      && parent->generation == token->parent_generation && parent->wait_epoch == token->epoch
      && parent->wait_state != KU_TASK_DRIVER_WAIT_EMPTY ? parent : NULL;
}
static uint32_t ku_task_driver_wait_read(
    const KuTaskDriverWaitTokenV1* token, KuTaskDriverWaitSnapshotV1* output) {
  uint32_t checked = ku_task_driver_wait_check_token(token);
  if (checked != KU_TASK_DRIVER_OK) return checked;
  KuTaskDriverV1* driver = token->driver;
  if (!ku_task_driver_external_storage(driver, output, sizeof(*output), KU_TASK_FRAME_ALIGNOF(KuTaskDriverWaitSnapshotV1))
      || ku_task_frame_ranges_overlap(token, sizeof(*token), output, sizeof(*output))) return KU_TASK_DRIVER_INVALID_ARGUMENT;
  if (ku_task_driver_lock(driver)) return KU_TASK_DRIVER_INTERNAL;
  KuTaskDriverSlotV1* parent = ku_task_driver_wait_find_locked(token);
  uint32_t result = parent ? KU_TASK_DRIVER_OK : KU_TASK_DRIVER_STALE;
  if (parent && ku_task_frame_ranges_overlap(output, sizeof(*output), parent->driver_lease.control, sizeof(KuTaskControlV1)))
    result = KU_TASK_DRIVER_INVALID_ARGUMENT;
  if (result == KU_TASK_DRIVER_OK && parent->wait_child_slot < driver->capacity) {
    KuTaskDriverSlotV1* child = &driver->slots[parent->wait_child_slot];
    if (child->generation == parent->wait_child_generation && child->binding
        && ku_task_frame_ranges_overlap(output, sizeof(*output), child->binding, sizeof(KuTaskControlV1)))
      result = KU_TASK_DRIVER_INVALID_ARGUMENT;
  }
  if (result == KU_TASK_DRIVER_OK)
    *output = (KuTaskDriverWaitSnapshotV1){ parent->wait_state, parent->wait_outcome, parent->wait_child_slot, parent->wait_child_generation, parent->wait_kind };
  ku_task_driver_unlock(driver);
  return result;
}
static uint32_t ku_task_driver_wait_detach(KuTaskDriverWaitTokenV1* token) {
  uint32_t checked = ku_task_driver_wait_check_token(token);
  if (checked != KU_TASK_DRIVER_OK) return checked;
  KuTaskDriverV1* driver = token->driver;
  if (!ku_task_driver_external_storage(driver, token, sizeof(*token), KU_TASK_FRAME_ALIGNOF(KuTaskDriverWaitTokenV1)))
    return KU_TASK_DRIVER_INVALID_ARGUMENT;
  if (ku_task_driver_lock(driver)) return KU_TASK_DRIVER_INTERNAL;
  KuTaskDriverSlotV1* parent = ku_task_driver_wait_find_locked(token);
  uint32_t result = parent ? KU_TASK_DRIVER_OK : KU_TASK_DRIVER_STALE;
  if (parent && ku_task_frame_ranges_overlap(token, sizeof(*token), parent->driver_lease.control, sizeof(KuTaskControlV1)))
    result = KU_TASK_DRIVER_INVALID_ARGUMENT;
  if (result == KU_TASK_DRIVER_OK && parent->wait_child_slot < driver->capacity) {
    KuTaskDriverSlotV1* child = &driver->slots[parent->wait_child_slot];
    if (child->generation == parent->wait_child_generation && child->binding
        && ku_task_frame_ranges_overlap(token, sizeof(*token), child->binding, sizeof(KuTaskControlV1)))
      result = KU_TASK_DRIVER_INVALID_ARGUMENT;
  }
  if (result == KU_TASK_DRIVER_OK) {
    size_t index = token->parent_slot;
    result = ku_task_driver_wait_clear_locked(driver, index);
    if (result == KU_TASK_DRIVER_OK) *token = (KuTaskDriverWaitTokenV1){0};
    /* Even an inconsistent pair is no longer a usable progress source. Keep
     * INTERNAL visible and do not mutate somebody else's unmatched link or
     * pretend to consume this token, but still unblock the affected parent. */
    if (parent->state == KU_TASK_DRIVER_RUNNING) {
      if (parent->intent == KU_TASK_DRIVER_WAIT) parent->intent = KU_TASK_DRIVER_YIELD;
    } else ku_task_driver_enqueue(driver, index);
  }
  ku_task_driver_unlock(driver);
  return result;
}
/* Requires mutex. Only the first clock failure scans; no periodic retry or
 * normal-code recovery follows. Existing owners/BUILDING reservations remain
 * protected and can later transfer to deterministic worker cleanup. */
static void ku_task_driver_enter_clock_fault(KuTaskDriverV1* driver) {
  if (driver->clock_fault) {
    ku_task_driver_signal_work(driver); ku_task_driver_signal(driver); return;
  }
  driver->clock_fault = 1; driver->fault = KU_TASK_DRIVER_INTERNAL;
  driver->closing = 1; driver->shutdown_deadline = 0; driver->next_deadline = UINT64_MAX;
  for (size_t i = 0; i < driver->capacity; i++) {
    KuTaskDriverSlotV1* slot = &driver->slots[i];
    if (slot->state == KU_TASK_DRIVER_FREE || slot->state == KU_TASK_DRIVER_RETIRING) continue;
    slot->cancel_deadline = 0; slot->deadline_fired = 1; slot->cancel_pending = 1;
    if (slot->driver_lease.control) {
      uint32_t result = ku_task_control_request_cancel(&slot->driver_lease, KU_TASK_CONTROL_CANCELLED, 0);
      if (result != KU_TASK_CONTROL_OK && result != KU_TASK_CONTROL_PENDING
          && !ku_task_control_is_terminal(result)) driver->fault = KU_TASK_DRIVER_INTERNAL;
    }
    ku_task_driver_wait_abort_locked(driver, i);
    if (slot->scope_wait_started) {
      slot->scope_wait_deadline = 0;
      ku_task_driver_scope_expire_locked(driver, i, 0);
    }
    ku_task_driver_enqueue(driver, i);
  }
  ku_task_driver_signal_work(driver);
  ku_task_driver_signal(driver);
}
static uint32_t ku_task_driver_wake(const KuTaskDriverTicketV1* ticket) {
  uint32_t checked = ku_task_driver_check_ticket(ticket);
  if (checked != KU_TASK_DRIVER_OK) return checked;
  KuTaskDriverV1* driver = ticket->driver;
  if (ku_task_driver_lock(driver)) return KU_TASK_DRIVER_INTERNAL;
  KuTaskDriverSlotV1* slot = ku_task_driver_find(ticket);
  uint32_t result = !slot ? KU_TASK_DRIVER_STALE
      : slot->cleanup_fault ? slot->cleanup_fault : KU_TASK_DRIVER_OK;
  if (result == KU_TASK_DRIVER_OK) {
    if (driver->wakes != UINT64_MAX) driver->wakes++;
    ku_task_driver_enqueue(driver, ticket->slot);
    ku_task_driver_signal(driver); /* Also covers a duplicate queued wake. */
  }
  ku_task_driver_unlock(driver);
  return result;
}
static uint32_t ku_task_driver_set_intent(const KuTaskDriverTicketV1* ticket, uint32_t intent) {
  uint32_t checked = ku_task_driver_check_ticket(ticket);
  if (checked != KU_TASK_DRIVER_OK) return checked;
  if (intent != KU_TASK_DRIVER_YIELD && intent != KU_TASK_DRIVER_WAIT) return KU_TASK_DRIVER_INVALID_ARGUMENT;
  KuTaskDriverV1* driver = ticket->driver;
  if (ku_task_driver_lock(driver)) return KU_TASK_DRIVER_INTERNAL;
  KuTaskDriverSlotV1* slot = ku_task_driver_find(ticket);
  uint32_t result = !slot ? KU_TASK_DRIVER_STALE : slot->state != KU_TASK_DRIVER_RUNNING
      ? KU_TASK_DRIVER_INVALID_STATE : KU_TASK_DRIVER_OK;
  if (result == KU_TASK_DRIVER_OK) slot->intent = intent;
  ku_task_driver_unlock(driver);
  return result;
}
static uint32_t ku_task_driver_wrapper_begin(
    const KuTaskDriverTicketV1* ticket, const KuTaskControlLeaseV1* lease) {
  uint32_t checked = ku_task_driver_check_ticket(ticket);
  if (checked != KU_TASK_DRIVER_OK) return checked;
  checked = ku_task_control_check_lease(lease);
  if (checked != KU_TASK_CONTROL_OK) return checked;
  KuTaskDriverV1* driver = ticket->driver;
  if (ku_task_driver_lock(driver)) return KU_TASK_DRIVER_INTERNAL;
  KuTaskDriverSlotV1* slot = ku_task_driver_find(ticket);
  uint32_t result = !slot ? KU_TASK_DRIVER_STALE : slot->binding != lease->control
      ? KU_TASK_DRIVER_INVALID_ARGUMENT : slot->state == KU_TASK_DRIVER_BUILDING
      || slot->state == KU_TASK_DRIVER_RETIRING ? KU_TASK_DRIVER_INVALID_STATE
      : slot->cleanup_fault ? slot->cleanup_fault : KU_TASK_DRIVER_OK;
  if (result == KU_TASK_DRIVER_OK) {
    if (slot->wrapper_active >= KU_TASK_CONTROL_MAX_REFERENCES) result = KU_TASK_DRIVER_LIMIT;
    else slot->wrapper_active++;
  }
  ku_task_driver_unlock(driver);
  return result;
}
static void ku_task_driver_wrapper_end(const KuTaskDriverTicketV1* ticket) {
  KuTaskDriverV1* driver = ticket->driver;
  if (ku_task_driver_lock(driver)) return;
  KuTaskDriverSlotV1* slot = ku_task_driver_find(ticket);
  if (!slot || !slot->wrapper_active) driver->fault = KU_TASK_DRIVER_INTERNAL;
  else {
    slot->wrapper_active--;
    ku_task_driver_scope_refresh_locked(driver, ticket->slot);
    ku_task_driver_wait_cancel_check_locked(driver, ticket->slot);
    ku_task_driver_wait_publish_locked(driver, ticket->slot);
    ku_task_driver_enqueue(driver, ticket->slot);
  }
  ku_task_driver_signal(driver);
  ku_task_driver_unlock(driver);
}
static uint32_t ku_task_driver_request_cancel(
    const KuTaskDriverTicketV1* ticket, const KuTaskControlLeaseV1* lease,
    uint32_t reason, uint64_t deadline) {
  uint32_t checked = ku_task_driver_wrapper_begin(ticket, lease);
  if (checked != KU_TASK_DRIVER_OK) return checked;
  uint32_t result = ku_task_control_request_cancel(lease, reason, deadline);
  KuTaskDriverV1* driver = ticket->driver;
  if (!ku_task_driver_lock(driver)) {
    KuTaskDriverSlotV1* slot = ku_task_driver_find(ticket);
    if (slot && (result == KU_TASK_CONTROL_OK || result == KU_TASK_CONTROL_PENDING)) {
      slot->cancel_deadline = ku_task_driver_min(slot->cancel_deadline, deadline);
      ku_task_driver_arm_deadline(driver, slot);
      if (result == KU_TASK_CONTROL_PENDING) slot->cancel_pending = 1;
      ku_task_driver_scope_refresh_locked(driver, ticket->slot);
    }
    ku_task_driver_unlock(driver);
  }
  ku_task_driver_wrapper_end(ticket);
  return result;
}
static uint32_t ku_task_driver_take_result(
    const KuTaskDriverTicketV1* ticket, const KuTaskControlLeaseV1* lease, void* output) {
  uint32_t checked = ku_task_driver_wrapper_begin(ticket, lease);
  if (checked != KU_TASK_DRIVER_OK) return checked;
  uint32_t result = ku_task_control_take_result(lease, output);
  ku_task_driver_wrapper_end(ticket); /* AFTER final TAKEN/AVAILABLE publication. */
  return result;
}
/* Only the current generated callback may use this bound cancellation entry.
 * Its execution/registry leases already protect control: no invented lease or
 * extra retain is needed at the reference limit. R2 request_cancel is bounded
 * atomics only; no frame, payload, dispose or user callback runs under mutex. */
static uint32_t ku_task_driver_cancel_bound(
    const KuTaskDriverTicketV1* ticket, uint32_t reason, uint64_t deadline) {
  uint32_t checked = ku_task_driver_check_ticket(ticket);
  if (checked != KU_TASK_DRIVER_OK) return checked;
  if (reason != KU_TASK_CONTROL_CANCELLED && reason != KU_TASK_CONTROL_TIMED_OUT)
    return KU_TASK_DRIVER_INVALID_ARGUMENT;
  KuTaskDriverV1* driver = ticket->driver;
  if (ku_task_driver_lock(driver)) return KU_TASK_DRIVER_INTERNAL;
  KuTaskDriverSlotV1* slot = ku_task_driver_find(ticket);
  uint32_t result = !slot ? KU_TASK_DRIVER_STALE
      : slot->cleanup_fault ? slot->cleanup_fault
      : slot->state != KU_TASK_DRIVER_RUNNING || !slot->driver_lease.control
        || slot->binding != slot->driver_lease.control ? KU_TASK_DRIVER_INVALID_STATE
      : ku_task_control_request_cancel(&slot->driver_lease, reason, deadline);
  if (result == KU_TASK_CONTROL_OK || result == KU_TASK_CONTROL_PENDING) {
    slot->cancel_deadline = ku_task_driver_min(slot->cancel_deadline, deadline);
    slot->cancel_pending = 1;
    ku_task_driver_arm_deadline(driver, slot);
    ku_task_driver_scope_refresh_locked(driver, ticket->slot);
    ku_task_driver_wait_cancel_check_locked(driver, ticket->slot);
    ku_task_driver_enqueue(driver, ticket->slot);
  }
  ku_task_driver_signal(driver);
  ku_task_driver_unlock(driver);
  return result;
}
/* Generated typed take glue calls this once, after all output/alias checks and
 * before its infallible move. `bytes` is the actual active Owned payload charge;
 * `minimum_from_bytes` is that generated child's sizeof(instance), not user
 * input. IDs and the TAKING wrapper are checked, not treated as raw-C authority.
 * This preserves total reserved bytes while the allocation changes owner. */
static uint32_t ku_task_driver_transfer_charge(
    const KuTaskDriverTicketV1* from, const KuTaskDriverTicketV1* to,
    size_t bytes, size_t minimum_from_bytes) {
  uint32_t checked = ku_task_driver_check_ticket(from);
  if (checked != KU_TASK_DRIVER_OK) return checked;
  checked = ku_task_driver_check_ticket(to);
  if (checked != KU_TASK_DRIVER_OK) return checked;
  if (from->driver != to->driver || from->slot == to->slot || !minimum_from_bytes)
    return KU_TASK_DRIVER_INVALID_ARGUMENT;
  KuTaskDriverV1* driver = from->driver;
  if (ku_task_driver_lock(driver)) return KU_TASK_DRIVER_INTERNAL;
  KuTaskDriverSlotV1* child = ku_task_driver_find(from);
  KuTaskDriverSlotV1* parent = ku_task_driver_find(to);
  uint32_t result = !child || !parent ? KU_TASK_DRIVER_STALE
      : child->cleanup_fault ? child->cleanup_fault
      : parent->cleanup_fault ? parent->cleanup_fault
      : parent->state != KU_TASK_DRIVER_RUNNING || !parent->driver_lease.control
        || parent->binding != parent->driver_lease.control || !child->driver_lease.control
        || child->binding != child->driver_lease.control || !child->wrapper_active
        || ku_task_control_atomic_load(&child->binding->payload) != KU_TASK_CONTROL_PAYLOAD_TAKING
      ? KU_TASK_DRIVER_INVALID_STATE : KU_TASK_DRIVER_OK;
  if (result == KU_TASK_DRIVER_OK
      && (child->charged_bytes < minimum_from_bytes
          || bytes > child->charged_bytes - minimum_from_bytes
          || parent->charged_bytes > SIZE_MAX - bytes
          || child->charged_bytes > driver->reserved_bytes
          || parent->charged_bytes > driver->reserved_bytes - child->charged_bytes))
    result = KU_TASK_DRIVER_LIMIT;
  if (result == KU_TASK_DRIVER_OK) {
    child->charged_bytes -= bytes;
    parent->charged_bytes += bytes;
  }
  ku_task_driver_unlock(driver);
  return result;
}
static uint32_t ku_task_driver_owner_drop_impl(
    const KuTaskDriverTicketV1* ticket, KuTaskControlOwnerV1* owner, uint64_t deadline,
    KuTaskDriverCleanupReceiptV1* receipt) {
  uint32_t checked = ku_task_driver_check_ticket(ticket);
  if (checked != KU_TASK_DRIVER_OK) return checked;
  if (!ku_task_frame_storage_valid(owner, sizeof(*owner), sizeof(*owner),
                                   KU_TASK_FRAME_ALIGNOF(KuTaskControlOwnerV1)))
    return KU_TASK_DRIVER_INVALID_ARGUMENT;
  checked = ku_task_control_check_lease(&owner->lease);
  if (checked != KU_TASK_CONTROL_OK) return checked;
  KuTaskDriverV1* driver = ticket->driver;
  if (!ku_task_driver_external_storage(driver, owner, sizeof(*owner),
                                       KU_TASK_FRAME_ALIGNOF(KuTaskControlOwnerV1)))
    return KU_TASK_DRIVER_INVALID_ARGUMENT;
  if (receipt) {
    /* Reject known complete-header aliases BEFORE reading the larger receipt
     * as an empty object. Deep allocation independence is the raw contract. */
    if (!ku_task_driver_external_storage(driver, receipt, sizeof(*receipt),
                                         KU_TASK_FRAME_ALIGNOF(KuTaskDriverCleanupReceiptV1))
        || ku_task_frame_ranges_overlap(receipt, sizeof(*receipt), ticket, sizeof(*ticket))
        || ku_task_frame_ranges_overlap(receipt, sizeof(*receipt), owner, sizeof(*owner))
        || ku_task_frame_ranges_overlap(receipt, sizeof(*receipt), owner->lease.control,
                                         sizeof(KuTaskControlV1)))
      return KU_TASK_DRIVER_INVALID_ARGUMENT;
    if (receipt->driver || receipt->slot || receipt->generation || receipt->abi_version)
      return KU_TASK_DRIVER_INVALID_ARGUMENT;
  }
  if (ku_task_driver_lock(driver)) return KU_TASK_DRIVER_INTERNAL;
  KuTaskDriverSlotV1* slot = ku_task_driver_find(ticket);
  uint32_t result = !slot ? KU_TASK_DRIVER_STALE : slot->binding != owner->lease.control
      ? KU_TASK_DRIVER_INVALID_ARGUMENT : slot->owner_location != KU_TASK_DRIVER_OWNER_USER
      ? KU_TASK_DRIVER_INVALID_STATE : KU_TASK_DRIVER_OK;
  if (result == KU_TASK_DRIVER_OK) {
    if (slot->wrapper_active >= KU_TASK_CONTROL_MAX_REFERENCES) result = KU_TASK_DRIVER_LIMIT;
    else slot->wrapper_active++;
  }
  ku_task_driver_unlock(driver);
  if (result != KU_TASK_DRIVER_OK) return result;
  /* Cancel an already running task now, not only when this worker next dequeues
   * its deferred owner. This owner remains the protected lease until transfer.
   * An in-flight publisher may return Pending; the slot durably retains the
   * retry obligation and that publisher's wrapper supplies its final notify. */
  uint32_t requested = ku_task_control_request_cancel(&owner->lease, KU_TASK_CONTROL_CANCELLED, deadline);
  if (ku_task_driver_lock(driver)) return KU_TASK_DRIVER_INTERNAL;
  slot = ku_task_driver_find(ticket);
  if (!slot) { ku_task_driver_unlock(driver); return KU_TASK_DRIVER_INTERNAL; }
  slot->wrapper_active--;
  if (requested != KU_TASK_CONTROL_OK && requested != KU_TASK_CONTROL_PENDING
      && !ku_task_control_is_terminal(requested)) result = requested;
  if (result == KU_TASK_DRIVER_OK) {
    /* Token transfer only: no destructor or control callback while locked. */
    result = ku_task_control_owner_move(&slot->deferred_owner, owner);
    if (result == KU_TASK_CONTROL_OK) {
      slot->owner_location = KU_TASK_DRIVER_OWNER_DEFERRED;
      /* Owner consumption and receipt issuance are one locked transaction.
       * The worker cannot ACK/dispose/reuse this slot before the proof exists. */
      if (receipt) *receipt = (KuTaskDriverCleanupReceiptV1){
        driver, ticket->slot, ticket->generation, KU_TASK_DRIVER_ABI_VERSION
      };
      slot->cancel_deadline = ku_task_driver_min(slot->cancel_deadline, deadline);
      ku_task_driver_arm_deadline(driver, slot);
      slot->cancel_pending = 1;
      ku_task_driver_scope_refresh_locked(driver, ticket->slot);
      ku_task_driver_wait_abort_locked(driver, ticket->slot);
      ku_task_driver_enqueue(driver, ticket->slot);
    }
  }
  ku_task_driver_unlock(driver);
  return result;
}
static uint32_t ku_task_driver_owner_drop(
    const KuTaskDriverTicketV1* ticket, KuTaskControlOwnerV1* owner, uint64_t deadline) {
  return ku_task_driver_owner_drop_impl(ticket, owner, deadline, NULL);
}
static uint32_t ku_task_driver_owner_drop_receipt(
    const KuTaskDriverTicketV1* ticket, KuTaskControlOwnerV1* owner, uint64_t deadline,
    KuTaskDriverCleanupReceiptV1* receipt) {
  if (!receipt) return KU_TASK_DRIVER_INVALID_ARGUMENT;
  return ku_task_driver_owner_drop_impl(ticket, owner, deadline, receipt);
}
static uint32_t ku_task_driver_cleanup_receipt_read(
    const KuTaskDriverCleanupReceiptV1* receipt) {
  uint32_t checked = ku_task_driver_check_cleanup_receipt(receipt);
  if (checked != KU_TASK_DRIVER_OK) return checked;
  KuTaskDriverV1* driver = receipt->driver;
  if (ku_task_driver_lock(driver)) return KU_TASK_DRIVER_INTERNAL;
  /* Never dereference binding/control: an old issued receipt remains useful
   * after physical disposal and after a new control occupies this slot.
   * A valid receipt cannot come from BUILDING rollback or a copied ticket. */
  uint32_t result = ku_task_driver_cleanup_receipt_status_locked(driver, receipt->slot, receipt->generation);
  if (result == KU_TASK_DRIVER_INTERNAL) driver->fault = result;
  ku_task_driver_unlock(driver);
  return result;
}
/* A scope can be cancelled after every child owner has already been handed
 * off. Its non-owning receipts may only tighten those outstanding cleanups;
 * they must never acquire a fresh budget, retain, or target a reused slot.
 * The registry lease is real and protected by this mutex. R2 cancellation
 * performs bounded atomics only, with no user/destructor callback here. */
static uint32_t ku_task_driver_cancel_receipt(
    const KuTaskDriverCleanupReceiptV1* receipt, uint64_t deadline) {
  uint32_t checked = ku_task_driver_check_cleanup_receipt(receipt);
  if (checked != KU_TASK_DRIVER_OK) return checked;
  if (deadline == UINT64_MAX) return KU_TASK_DRIVER_INVALID_ARGUMENT;
  KuTaskDriverV1* driver = receipt->driver;
  if (ku_task_driver_lock(driver)) return KU_TASK_DRIVER_INTERNAL;
  uint32_t result = ku_task_driver_cleanup_receipt_status_locked(driver, receipt->slot, receipt->generation);
  if (result == KU_TASK_DRIVER_PENDING) {
    KuTaskDriverSlotV1* slot = &driver->slots[receipt->slot];
    if (slot->owner_location == KU_TASK_DRIVER_OWNER_USER || !slot->driver_lease.control
        || slot->binding != slot->driver_lease.control) result = KU_TASK_DRIVER_INTERNAL;
    else {
      uint32_t requested = ku_task_control_request_cancel(&slot->driver_lease, KU_TASK_CONTROL_CANCELLED, deadline);
      if (requested == KU_TASK_CONTROL_OK || requested == KU_TASK_CONTROL_PENDING
          || ku_task_control_is_terminal(requested)) {
        slot->cancel_deadline = ku_task_driver_min(slot->cancel_deadline, deadline);
        slot->cancel_pending = 1;
        ku_task_driver_arm_deadline(driver, slot);
        ku_task_driver_scope_refresh_locked(driver, receipt->slot);
        ku_task_driver_wait_cancel_check_locked(driver, receipt->slot);
        ku_task_driver_enqueue(driver, receipt->slot);
        result = KU_TASK_DRIVER_OK;
        ku_task_driver_signal(driver);
      } else result = requested;
    }
  }
  if (result == KU_TASK_DRIVER_INTERNAL) driver->fault = result;
  ku_task_driver_unlock(driver);
  return result;
}
static int ku_task_driver_start_source_storage(
    KuTaskDriverV1* driver, const KuTaskDriverStartChargeV1* source) {
  return ku_task_driver_external_storage(driver, source, sizeof(*source),
                                         KU_TASK_FRAME_ALIGNOF(KuTaskDriverStartChargeV1));
}
/* Requires the queue mutex. Only the real running registry protects control;
 * a copied ticket/control pointer never supplies an extra reference or executor.
 * Cancellation may already be requested: it cannot destroy a running frame.
 * No callback, allocation, retain, or wait occurs in this validation. */
static uint32_t ku_task_driver_start_parent_locked(
    KuTaskDriverV1* driver, const KuTaskDriverStartChargeV1* source,
    KuTaskDriverSlotV1** output) {
  if (source->parent.driver != driver || !source->parent.generation
      || source->parent.slot >= driver->capacity || !source->parent_control
      || !source->minimum_parent_bytes) return KU_TASK_DRIVER_INVALID_ARGUMENT;
  KuTaskDriverSlotV1* parent = ku_task_driver_find(&source->parent);
  if (!parent) return KU_TASK_DRIVER_STALE;
  if (parent->cleanup_fault) return parent->cleanup_fault;
  if (parent->state != KU_TASK_DRIVER_RUNNING || !parent->driver_lease.control
      || parent->binding != parent->driver_lease.control
      || parent->binding != source->parent_control) return KU_TASK_DRIVER_INVALID_STATE;
  if (parent->charged_bytes < source->minimum_parent_bytes
      || source->owned_bytes > parent->charged_bytes - source->minimum_parent_bytes
      || parent->charged_bytes > driver->reserved_bytes) {
    driver->fault = KU_TASK_DRIVER_INTERNAL; return KU_TASK_DRIVER_INTERNAL;
  }
  *output = parent;
  return KU_TASK_DRIVER_OK;
}
static uint32_t ku_task_driver_reserve_impl(
    KuTaskDriverV1* driver, size_t task_bytes, KuTaskDriverTicketV1* output,
    const KuTaskDriverStartChargeV1* source) {
  uint32_t checked = ku_task_driver_check(driver);
  if (checked != KU_TASK_DRIVER_OK) return checked;
  if (!ku_task_driver_external_storage(driver, output, sizeof(*output),
                                       KU_TASK_FRAME_ALIGNOF(KuTaskDriverTicketV1)))
    return KU_TASK_DRIVER_INVALID_ARGUMENT;
  /* Reject the known header alias before reading either header's fields. */
  if (source && (!ku_task_driver_start_source_storage(driver, source)
      || ku_task_frame_ranges_overlap(source, sizeof(*source), output, sizeof(*output))))
    return KU_TASK_DRIVER_INVALID_ARGUMENT;
  if (source && (!ku_task_frame_storage_valid(source->parent_control,
          sizeof(KuTaskControlV1), sizeof(KuTaskControlV1), KU_TASK_FRAME_ALIGNOF(KuTaskControlV1))
      || ku_task_frame_ranges_overlap(source, sizeof(*source),
                                      source->parent_control, sizeof(KuTaskControlV1))
      || ku_task_frame_ranges_overlap(source->parent_control,
                                      sizeof(KuTaskControlV1), output, sizeof(*output))))
    return KU_TASK_DRIVER_INVALID_ARGUMENT;
  if (output->driver)
    return KU_TASK_DRIVER_INVALID_ARGUMENT;
  if (!task_bytes) return KU_TASK_DRIVER_INVALID_ARGUMENT;
  if (ku_task_driver_lock(driver)) return KU_TASK_DRIVER_INTERNAL;
  KuTaskDriverSlotV1* parent = NULL;
  uint32_t result = source ? ku_task_driver_start_parent_locked(driver, source, &parent)
      : KU_TASK_DRIVER_OK;
  (void)parent;
  if (result == KU_TASK_DRIVER_OK && source && source->owned_bytes > SIZE_MAX - task_bytes)
    result = KU_TASK_DRIVER_LIMIT;
  if (result == KU_TASK_DRIVER_OK) {
    if (driver->closing || driver->workers_created != driver->worker_target
        || !driver->workers_created || driver->workers_exited) result = KU_TASK_DRIVER_CLOSED;
    else if (driver->fault) result = driver->fault;
    else if (driver->fixed_bytes > driver->byte_limit
        || driver->reserved_bytes > driver->byte_limit - driver->fixed_bytes) {
      driver->fault = KU_TASK_DRIVER_INTERNAL; result = KU_TASK_DRIVER_INTERNAL;
    } else if (driver->resident >= driver->capacity
        || task_bytes > driver->byte_limit - driver->fixed_bytes - driver->reserved_bytes)
      result = KU_TASK_DRIVER_LIMIT;
  }
  if (result == KU_TASK_DRIVER_OK) {
    result = KU_TASK_DRIVER_LIMIT;
    for (size_t i = 0; i < driver->capacity; i++) {
      KuTaskDriverSlotV1* slot = &driver->slots[i];
      if (slot->state != KU_TASK_DRIVER_FREE || slot->generation == UINT64_MAX) continue;
      if (slot->cleanup_acked_generation > slot->generation) {
        driver->fault = KU_TASK_DRIVER_INTERNAL; result = driver->fault; break;
      }
      uint64_t generation = slot->generation + 1;
      uint64_t cleanup_acked_generation = slot->cleanup_acked_generation;
      memset(slot, 0, sizeof(*slot));
      slot->generation = generation;
      slot->cleanup_acked_generation = cleanup_acked_generation;
      slot->state = KU_TASK_DRIVER_BUILDING;
      slot->charged_bytes = task_bytes;
      slot->cancel_deadline = UINT64_MAX;
      slot->scope_wait_deadline = UINT64_MAX;
      driver->resident++; driver->building++; driver->reserved_bytes += task_bytes;
      *output = (KuTaskDriverTicketV1){ driver, i, generation };
      result = KU_TASK_DRIVER_OK; break;
    }
  }
  ku_task_driver_unlock(driver);
  return result;
}
static uint32_t ku_task_driver_reserve(
    KuTaskDriverV1* driver, size_t task_bytes, KuTaskDriverTicketV1* output) {
  return ku_task_driver_reserve_impl(driver, task_bytes, output, NULL);
}
static uint32_t ku_task_driver_reserve_child(
    KuTaskDriverV1* driver, const KuTaskDriverStartChargeV1* source,
    size_t child_instance_bytes, KuTaskDriverTicketV1* output) {
  if (!source) return KU_TASK_DRIVER_INVALID_ARGUMENT;
  return ku_task_driver_reserve_impl(driver, child_instance_bytes, output, source);
}
static uint32_t ku_task_driver_return_slot(KuTaskDriverV1* driver, KuTaskDriverSlotV1* slot) {
  /* Validate before changing ANY accounting. Rollback has never published a
   * control; physical disposal must already have logical ACK for this exact
   * generation. Neither operation may carry a wait link into another task. */
  int building = slot->state == KU_TASK_DRIVER_BUILDING;
  if (slot->cleanup_fault || slot->scope_session_active || slot->scope_session_final || slot->scope_receipts
      || slot->scope_receipt_capacity || slot->scope_expected_mask
      || slot->wait_state != KU_TASK_DRIVER_WAIT_EMPTY || slot->waiter_epoch
      || slot->cleanup_acked_generation > slot->generation || !slot->generation
      || !driver->resident || !slot->charged_bytes || slot->charged_bytes > driver->reserved_bytes
      || slot->binding || slot->driver_lease.control || slot->execution_lease.control
      || slot->deferred_owner.lease.control || slot->wrapper_active
      || (building && (!driver->building || slot->cleanup_acked_generation == slot->generation))
      || (!building && (slot->state != KU_TASK_DRIVER_RETIRING
          || slot->owner_location != KU_TASK_DRIVER_OWNER_RELEASED
          || slot->cleanup_acked_generation != slot->generation))) {
    driver->fault = KU_TASK_DRIVER_INTERNAL; return KU_TASK_DRIVER_INTERNAL;
  }
  if (building) driver->building--;
  driver->reserved_bytes -= slot->charged_bytes;
  driver->resident--;
  uint64_t generation = slot->generation;
  uint64_t cleanup_acked_generation = slot->cleanup_acked_generation;
  memset(slot, 0, sizeof(*slot));
  slot->generation = generation;
  slot->cleanup_acked_generation = cleanup_acked_generation;
  if (driver->closing && !driver->resident) ku_task_driver_signal_work(driver);
  ku_task_driver_signal(driver);
  return KU_TASK_DRIVER_OK;
}
static uint32_t ku_task_driver_rollback(KuTaskDriverTicketV1* ticket) {
  uint32_t checked = ku_task_driver_check_ticket(ticket);
  if (checked != KU_TASK_DRIVER_OK) return checked;
  KuTaskDriverV1* driver = ticket->driver;
  if (ku_task_driver_lock(driver)) return KU_TASK_DRIVER_INTERNAL;
  KuTaskDriverSlotV1* slot = ku_task_driver_find(ticket);
  uint32_t result = !slot ? KU_TASK_DRIVER_STALE : slot->state != KU_TASK_DRIVER_BUILDING
      ? KU_TASK_DRIVER_INVALID_STATE : KU_TASK_DRIVER_OK;
  if (result == KU_TASK_DRIVER_OK) {
    result = ku_task_driver_return_slot(driver, slot);
    if (result == KU_TASK_DRIVER_OK) *ticket = (KuTaskDriverTicketV1){0};
  }
  ku_task_driver_unlock(driver);
  return result;
}
static uint32_t ku_task_driver_disposed(KuTaskDriverTicketV1* ticket) {
  uint32_t checked = ku_task_driver_check_ticket(ticket);
  if (checked != KU_TASK_DRIVER_OK) return checked;
  KuTaskDriverV1* driver = ticket->driver;
  if (ku_task_driver_lock(driver)) return KU_TASK_DRIVER_INTERNAL;
  KuTaskDriverSlotV1* slot = ku_task_driver_find(ticket);
  uint32_t result = !slot ? KU_TASK_DRIVER_STALE : slot->state != KU_TASK_DRIVER_RETIRING
      || slot->driver_lease.control || slot->execution_lease.control || slot->wrapper_active
      ? KU_TASK_DRIVER_INVALID_STATE : KU_TASK_DRIVER_OK;
  if (result == KU_TASK_DRIVER_OK) {
    result = ku_task_driver_return_slot(driver, slot);
    if (result == KU_TASK_DRIVER_OK) *ticket = (KuTaskDriverTicketV1){0};
  }
  ku_task_driver_unlock(driver);
  return result;
}
static uint32_t ku_task_driver_commit_impl(
    const KuTaskDriverTicketV1* ticket, KuTaskControlOwnerV1* owner,
    uint32_t mode, uint64_t deadline,
    const KuTaskDriverStartChargeV1* source, size_t child_instance_bytes) {
  uint32_t checked = ku_task_driver_check_ticket(ticket);
  if (checked != KU_TASK_DRIVER_OK) return checked;
  if (mode != KU_TASK_DRIVER_START && mode != KU_TASK_DRIVER_ABORT) return KU_TASK_DRIVER_INVALID_ARGUMENT;
  if (source && (mode != KU_TASK_DRIVER_START || !child_instance_bytes))
    return KU_TASK_DRIVER_INVALID_ARGUMENT;
  if (!ku_task_frame_storage_valid(owner, sizeof(*owner), sizeof(*owner),
                                   KU_TASK_FRAME_ALIGNOF(KuTaskControlOwnerV1))) return KU_TASK_DRIVER_INVALID_ARGUMENT;
  KuTaskDriverV1* driver = ticket->driver;
  if (source && (!ku_task_driver_start_source_storage(driver, source)
      || ku_task_frame_ranges_overlap(source, sizeof(*source), ticket, sizeof(*ticket))
      || ku_task_frame_ranges_overlap(source, sizeof(*source), owner, sizeof(*owner))))
    return KU_TASK_DRIVER_INVALID_ARGUMENT;
  if (source && (!ku_task_frame_storage_valid(source->parent_control,
          sizeof(KuTaskControlV1), sizeof(KuTaskControlV1), KU_TASK_FRAME_ALIGNOF(KuTaskControlV1))
      || ku_task_frame_ranges_overlap(source, sizeof(*source),
                                      source->parent_control, sizeof(KuTaskControlV1))
      || ku_task_frame_ranges_overlap(source->parent_control,
                                      sizeof(KuTaskControlV1), ticket, sizeof(*ticket))
      || ku_task_frame_ranges_overlap(source->parent_control,
                                      sizeof(KuTaskControlV1), owner, sizeof(*owner))))
    return KU_TASK_DRIVER_INVALID_ARGUMENT;
  checked = ku_task_control_check_lease(&owner->lease);
  if (checked != KU_TASK_CONTROL_OK) return checked;
  if (source && ku_task_frame_ranges_overlap(source, sizeof(*source),
                                             owner->lease.control, sizeof(KuTaskControlV1)))
    return KU_TASK_DRIVER_INVALID_ARGUMENT;
  KuTaskControlLeaseV1 registry = {0}, execution = {0};
  checked = ku_task_control_lease_retain(&owner->lease, &registry);
  if (checked != KU_TASK_CONTROL_OK) return checked;
  checked = ku_task_control_lease_retain(&owner->lease, &execution);
  if (checked != KU_TASK_CONTROL_OK) { ku_task_control_lease_release(&registry); return checked; }
  if (!ku_task_driver_external_storage(driver, owner, sizeof(*owner),
                                       KU_TASK_FRAME_ALIGNOF(KuTaskControlOwnerV1))
      || !ku_task_driver_external_storage(driver, owner->lease.control, sizeof(KuTaskControlV1),
                                          KU_TASK_FRAME_ALIGNOF(KuTaskControlV1))) {
    ku_task_control_lease_release(&execution); ku_task_control_lease_release(&registry);
    return KU_TASK_DRIVER_INVALID_ARGUMENT;
  }
  if (ku_task_driver_lock(driver)) {
    ku_task_control_lease_release(&execution); ku_task_control_lease_release(&registry);
    return KU_TASK_DRIVER_INTERNAL;
  }
  KuTaskDriverSlotV1* slot = ku_task_driver_find(ticket);
  KuTaskDriverSlotV1* parent = NULL;
  uint32_t result = !slot ? KU_TASK_DRIVER_STALE : slot->state != KU_TASK_DRIVER_BUILDING
      ? KU_TASK_DRIVER_INVALID_STATE : KU_TASK_DRIVER_OK;
  /* These checks precede ABORT's owner move and hosted charge transfer. A
   * corrupt queue must never produce a published-but-reported-failed commit. */
  if (result == KU_TASK_DRIVER_OK
      && (!driver->capacity || driver->capacity > KU_TASK_DRIVER_MAX_SLOTS
          || !driver->resident || driver->resident > driver->capacity
          || !driver->building || driver->building > driver->resident
          || driver->running > driver->resident || driver->queued >= driver->resident
          || driver->queued >= driver->capacity || driver->head >= driver->capacity
          || !driver->workers_created || driver->workers_created > driver->worker_target
          || driver->worker_target > KU_TASK_DRIVER_MAX_WORKERS
          || driver->workers_exited >= driver->workers_created || slot->cleanup_fault
          || slot->binding || slot->driver_lease.control || slot->execution_lease.control
          || slot->deferred_owner.lease.control || slot->wrapper_active
          || slot->wait_state != KU_TASK_DRIVER_WAIT_EMPTY || slot->waiter_epoch
          || slot->cleanup_acked_generation >= slot->generation
          || !slot->charged_bytes || slot->charged_bytes > driver->reserved_bytes)) {
    driver->fault = KU_TASK_DRIVER_INTERNAL; result = KU_TASK_DRIVER_INTERNAL;
  }
  if (result == KU_TASK_DRIVER_OK) {
    for (size_t i = 0; i < driver->capacity; i++) {
      if (driver->slots[i].binding == registry.control) { result = KU_TASK_DRIVER_INVALID_STATE; break; }
    }
  }
  if (result == KU_TASK_DRIVER_OK && source) {
    if (source->parent.driver != driver || source->parent.slot == ticket->slot)
      result = KU_TASK_DRIVER_INVALID_ARGUMENT;
    else if (slot->charged_bytes != child_instance_bytes)
      result = KU_TASK_DRIVER_INVALID_STATE;
    else result = ku_task_driver_start_parent_locked(driver, source, &parent);
    if (result == KU_TASK_DRIVER_OK
        && (source->owned_bytes > SIZE_MAX - slot->charged_bytes
            || parent->charged_bytes > driver->reserved_bytes - slot->charged_bytes)) {
      driver->fault = KU_TASK_DRIVER_INTERNAL; result = KU_TASK_DRIVER_INTERNAL;
    }
  }
  if (result == KU_TASK_DRIVER_OK && mode == KU_TASK_DRIVER_ABORT)
    result = ku_task_control_owner_move(&slot->deferred_owner, owner);
  if (result == KU_TASK_DRIVER_OK) {
    /* No later operation may report an unpublished failure. Existing OS wake
     * errors record fault but retain published ownership, never trigger caller
     * rollback. The parent callback protects inputs during BUILDING; charge
     * changes owner atomically with publication. Closing keeps its existing
     * cleanup-only admission disposition. */
    if (source) {
      parent->charged_bytes -= source->owned_bytes;
      slot->charged_bytes += source->owned_bytes;
    }
    slot->driver_lease = registry; registry.control = NULL;
    slot->execution_lease = execution; execution.control = NULL;
    slot->binding = slot->driver_lease.control;
    if (mode == KU_TASK_DRIVER_ABORT) {
      slot->owner_location = KU_TASK_DRIVER_OWNER_DEFERRED;
      slot->cancel_pending = 1; slot->cancel_deadline = deadline;
    }
    if (driver->closing) {
      slot->cancel_pending = 1;
      slot->cancel_deadline = ku_task_driver_min(slot->cancel_deadline, driver->shutdown_deadline);
    }
    ku_task_driver_arm_deadline(driver, slot);
    driver->building--;
    slot->state = KU_TASK_DRIVER_PARKED;
    ku_task_driver_enqueue(driver, ticket->slot);
  }
  ku_task_driver_unlock(driver);
  if (execution.control) ku_task_control_lease_release(&execution);
  if (registry.control) ku_task_control_lease_release(&registry);
  return result;
}
static uint32_t ku_task_driver_commit(
    const KuTaskDriverTicketV1* ticket, KuTaskControlOwnerV1* owner,
    uint32_t mode, uint64_t deadline) {
  return ku_task_driver_commit_impl(ticket, owner, mode, deadline, NULL, 0);
}
static uint32_t ku_task_driver_commit_child(
    const KuTaskDriverTicketV1* ticket, KuTaskControlOwnerV1* owner,
    const KuTaskDriverStartChargeV1* source, size_t child_instance_bytes) {
  if (!source) return KU_TASK_DRIVER_INVALID_ARGUMENT;
  return ku_task_driver_commit_impl(ticket, owner, KU_TASK_DRIVER_START, UINT64_MAX,
                                     source, child_instance_bytes);
}
static void ku_task_driver_snapshot_locked(KuTaskDriverV1* driver, KuTaskDriverSnapshotV1* output) {
  memset(output, 0, sizeof(*output));
  output->resident = driver->resident; output->building = driver->building;
  output->queued = driver->queued; output->running = driver->running;
  output->reserved_bytes = driver->reserved_bytes; output->fixed_bytes = driver->fixed_bytes;
  output->byte_limit = driver->byte_limit;
  output->polls = driver->polls; output->wakes = driver->wakes; output->waits = driver->waits;
  output->worker_target = driver->worker_target; output->workers_created = driver->workers_created;
  output->workers_waiting = driver->workers_waiting; output->workers_exited = driver->workers_exited;
  output->workers_joined = driver->workers_joined;
  output->closing = driver->closing; output->fault = driver->fault;
  output->clock_fault = driver->clock_fault;
  for (size_t i = 0; i < driver->capacity; i++) {
    switch (driver->slots[i].state) {
      case KU_TASK_DRIVER_PARKED:
      case KU_TASK_DRIVER_FAULTED: output->parked++; break;
      case KU_TASK_DRIVER_RETIRING: output->retiring++; break;
      case KU_TASK_DRIVER_TERMINAL_HELD: output->terminal_held++; break;
      default: break;
    }
  }
}
static uint32_t ku_task_driver_snapshot(KuTaskDriverV1* driver, KuTaskDriverSnapshotV1* output) {
  uint32_t checked = ku_task_driver_check(driver);
  if (checked != KU_TASK_DRIVER_OK) return checked;
  if (!ku_task_driver_external_storage(driver, output, sizeof(*output),
                                       KU_TASK_FRAME_ALIGNOF(KuTaskDriverSnapshotV1))) return KU_TASK_DRIVER_INVALID_ARGUMENT;
  if (ku_task_driver_lock(driver)) return KU_TASK_DRIVER_INTERNAL;
  ku_task_driver_snapshot_locked(driver, output);
  ku_task_driver_unlock(driver); return KU_TASK_DRIVER_OK;
}
/* The cached minimum is tightened in O(1) when a budget is armed. This bounded
 * scan runs only once that minimum expires (even with a continuously busy ready
 * queue), not once per runnable quantum or on a periodic polling interval. A
 * stale minimum from a retired slot causes one harmless recomputation. */
static uint64_t ku_task_driver_deadlines_locked(KuTaskDriverV1* driver, uint64_t now) {
  uint64_t next = UINT64_MAX;
  for (size_t i = 0; i < driver->capacity; i++) {
    KuTaskDriverSlotV1* slot = &driver->slots[i];
    if (slot->state == KU_TASK_DRIVER_FREE || slot->state == KU_TASK_DRIVER_BUILDING
        || slot->state == KU_TASK_DRIVER_RETIRING || slot->cleanup_fault) continue;
    if (!slot->deadline_fired && slot->cancel_deadline != UINT64_MAX) {
      if (slot->cancel_deadline <= now) {
        slot->deadline_fired = 1;
        ku_task_driver_enqueue(driver, i);
      } else next = ku_task_driver_min(next, slot->cancel_deadline);
    }
    if (slot->scope_wait_started && !slot->scope_deadline_fired && !slot->scope_wait_failure) {
      if (slot->scope_wait_deadline <= now) ku_task_driver_scope_expire_locked(driver, i, now);
      else next = ku_task_driver_min(next, slot->scope_wait_deadline);
    }
  }
  driver->next_deadline = next;
  return next;
}
/* Called only by this slot's executor, locked after its R2 poll returned terminal.
 * Its registry/execution leases protect these acquire reads. A terminal enum
 * alone is NOT cleanup ACK while the owner or a payload callback survives. */
static uint32_t ku_task_driver_cleanup_ack_locked(
    KuTaskDriverV1* driver, KuTaskDriverSlotV1* slot, uint32_t outcome) {
  if (slot->cleanup_fault) return slot->cleanup_fault;
  KuTaskControlV1* control = slot->driver_lease.control;
  if (!control || slot->binding != control || !slot->generation
      || slot->cleanup_acked_generation > slot->generation
      || slot->owner_location != KU_TASK_DRIVER_OWNER_RELEASED
      || slot->deferred_owner.lease.control || slot->wrapper_active
      || !ku_task_control_is_terminal(outcome)
      || ku_task_control_atomic_load(&control->phase) != outcome
      || ku_task_control_atomic_load(&control->lifecycle_pin) || !control->frame_destroyed) {
    slot->cleanup_fault = KU_TASK_DRIVER_INTERNAL;
    driver->fault = KU_TASK_DRIVER_INTERNAL; return KU_TASK_DRIVER_INTERNAL;
  }
  size_t payload = ku_task_control_atomic_load(&control->payload);
  int valid_payload = ku_task_control_is_payload_terminal(outcome)
      ? payload == KU_TASK_CONTROL_PAYLOAD_TAKEN || payload == KU_TASK_CONTROL_PAYLOAD_DROPPED
      : payload == KU_TASK_CONTROL_PAYLOAD_EMPTY || payload == KU_TASK_CONTROL_PAYLOAD_DROPPED;
  if (!valid_payload) {
    slot->cleanup_fault = KU_TASK_DRIVER_INTERNAL;
    driver->fault = KU_TASK_DRIVER_INTERNAL; return KU_TASK_DRIVER_INTERNAL;
  }
  /* drop_frame precedes terminal publication in R2; finish_poll releases the
   * lifecycle pin before returning. No callback or byte return occurs here. */
  slot->cleanup_acked_generation = slot->generation;
  return KU_TASK_DRIVER_CLEANUP_ACK;
}
static void ku_task_driver_worker(KuTaskDriverWorkerV1* worker) {
  KuTaskDriverV1* driver = worker->driver;
  if (ku_task_driver_lock(driver)) return;
  for (;;) {
    uint64_t sampled_now = driver->clock_fault ? 0 : ku_task_driver_now_ms();
    if (sampled_now == UINT64_MAX) ku_task_driver_enter_clock_fault(driver);
    uint64_t next_deadline = driver->next_deadline;
    if (!driver->clock_fault && next_deadline != UINT64_MAX && sampled_now >= next_deadline)
      next_deadline = ku_task_driver_deadlines_locked(driver, sampled_now);
    if (driver->closing && !driver->resident && !driver->running && !driver->queued) break;
    if (!driver->queued) {
      worker->state = KU_TASK_DRIVER_WORKER_WAITING; driver->workers_waiting++;
      if (driver->waits != UINT64_MAX) driver->waits++;
      ku_task_driver_signal(driver); /* Observers only: never a work wake. */
      int waited = driver->clock_fault ? ku_task_driver_wait_without_clock(driver)
          : ku_task_driver_wait_work(driver, next_deadline);
      worker->state = KU_TASK_DRIVER_WORKER_ACTIVE; driver->workers_waiting--;
      if (waited == -2) { ku_task_driver_enter_clock_fault(driver); continue; }
      if (waited < 0) {
        driver->fault = KU_TASK_DRIVER_INTERNAL;
        driver->closing = 1; driver->shutdown_deadline = 0;
        ku_task_driver_signal_work(driver); ku_task_driver_signal(driver);
        break;
      }
      continue;
    }
    size_t index = driver->ring[driver->head];
    driver->head = (driver->head + 1) % driver->capacity;
    driver->queued--; driver->running++;
    KuTaskDriverSlotV1* slot = &driver->slots[index];
    slot->state = KU_TASK_DRIVER_RUNNING; slot->notified = 0; slot->intent = 0;
    KuTaskControlLeaseV1 execution = slot->execution_lease;
    slot->execution_lease.control = NULL;
    KuTaskControlOwnerV1 owner = {0};
    if (slot->owner_location == KU_TASK_DRIVER_OWNER_DEFERRED) {
      owner = slot->deferred_owner; slot->deferred_owner.lease.control = NULL;
      slot->owner_location = KU_TASK_DRIVER_OWNER_WORKER;
    }
    uint64_t deadline = slot->cancel_deadline;
    int cancel = slot->cancel_pending || driver->closing;
    slot->cancel_pending = 0;
    if (driver->closing) deadline = ku_task_driver_min(deadline, driver->shutdown_deadline);
    slot->cancel_deadline = deadline;
    if (cancel) ku_task_driver_wait_abort_locked(driver, index);
    else ku_task_driver_wait_cancel_check_locked(driver, index);
    ku_task_driver_unlock(driver);

    uint32_t request = KU_TASK_CONTROL_OK, dropped = KU_TASK_CONTROL_OK;
    if (cancel) request = ku_task_control_request_cancel(&execution, KU_TASK_CONTROL_CANCELLED, deadline);
    if (owner.lease.control) dropped = ku_task_control_owner_drop(&owner, deadline);
    uint32_t outcome = ku_task_control_poll(&execution);

    if (ku_task_driver_lock(driver)) return;
    slot = &driver->slots[index];
    if (driver->polls != UINT64_MAX) driver->polls++;
    driver->running--;
    if (slot->owner_location == KU_TASK_DRIVER_OWNER_WORKER) {
      if (owner.lease.control) {
        slot->deferred_owner = owner; owner.lease.control = NULL;
        slot->owner_location = KU_TASK_DRIVER_OWNER_DEFERRED;
      } else slot->owner_location = KU_TASK_DRIVER_OWNER_RELEASED;
    }
    if (request == KU_TASK_CONTROL_PENDING) slot->cancel_pending = 1;
    if (dropped != KU_TASK_CONTROL_OK && dropped != KU_TASK_CONTROL_PENDING) driver->fault = KU_TASK_DRIVER_INTERNAL;
    int terminal = ku_task_control_is_terminal(outcome);
    ku_task_driver_scope_refresh_locked(driver, index);
    ku_task_driver_wait_cancel_check_locked(driver, index);
    ku_task_driver_wait_publish_locked(driver, index);
    if (terminal) {
      (void)ku_task_driver_wait_clear_locked(driver, index);
      ku_task_driver_scope_unregister_locked(slot);
    }
    KuTaskControlLeaseV1 registry = {0};
    int retire = terminal && slot->owner_location == KU_TASK_DRIVER_OWNER_RELEASED && !slot->wrapper_active;
    if (retire) {
      uint32_t ack = ku_task_driver_cleanup_ack_locked(driver, slot, outcome);
      /* Publish BOTH durable ACK and quarantine failure before binding retires.
       * A faulted child's enqueue is disabled; this wakes its parent instead. */
      ku_task_driver_wait_publish_locked(driver, index);
      if (ack != KU_TASK_DRIVER_CLEANUP_ACK) retire = 0;
    }
    if (retire) {
      registry = slot->driver_lease; slot->driver_lease.control = NULL;
      /* The disposer frees its allocation BEFORE returning this slot's budget.
       * Another builder may legitimately receive the same malloc address in
       * that interval. Retire the executable binding now, while keeping its
       * generation/count/bytes until final disposed acknowledgement. */
      slot->binding = NULL;
      slot->state = KU_TASK_DRIVER_RETIRING;
    } else {
      slot->execution_lease = execution; execution.control = NULL;
      int poll_fault = !terminal && outcome != KU_TASK_CONTROL_PENDING;
      slot->state = terminal ? KU_TASK_DRIVER_TERMINAL_HELD
          : poll_fault ? KU_TASK_DRIVER_FAULTED : KU_TASK_DRIVER_PARKED;
      if (poll_fault) {
        driver->fault = KU_TASK_DRIVER_INTERNAL;
        /* Preserve a concurrent cancellation's notification, not normal
         * execution. enqueue's FAULTED guard also covers later raw wakes. */
        if (slot->notified) ku_task_driver_enqueue(driver, index);
      } else if (slot->notified || (!terminal && slot->intent == KU_TASK_DRIVER_YIELD && !slot->deadline_fired))
        ku_task_driver_enqueue(driver, index);
      else if (!terminal && outcome == KU_TASK_CONTROL_PENDING && !slot->intent
               && !slot->wrapper_active && dropped != KU_TASK_CONTROL_PENDING && request != KU_TASK_CONTROL_PENDING) {
        /* An adapter returned Pending without a registered progress source.
         * Retain its storage and report fault; do not hide it by busy polling. */
        driver->fault = KU_TASK_DRIVER_INTERNAL;
      }
    }
    ku_task_driver_signal(driver);
    ku_task_driver_unlock(driver);
    if (execution.control) ku_task_control_lease_release(&execution);
    if (registry.control) ku_task_control_lease_release(&registry);
    if (ku_task_driver_lock(driver)) return;
  }
  worker->state = KU_TASK_DRIVER_WORKER_EXITED; driver->workers_exited++;
  ku_task_driver_signal(driver);
  ku_task_driver_unlock(driver);
  /* No further driver or task storage access after this point. */
}
#if defined(_WIN32)
static unsigned __stdcall ku_task_driver_thread(void* raw) {
  ku_task_driver_worker((KuTaskDriverWorkerV1*)raw); return 0;
}
#else
static void* ku_task_driver_thread(void* raw) {
  ku_task_driver_worker((KuTaskDriverWorkerV1*)raw); return NULL;
}
#endif
/* On post-resource failure this object remains LIVE or REAPING. The exclusive
 * caller must use the original startup D for shutdown/join/destroy, and may
 * free or zero storage only after destroy succeeds. No Task is admitted early. */
static uint32_t ku_task_driver_init(
    KuTaskDriverV1* driver, size_t bytes, uint32_t abi,
    KuTaskDriverSlotV1* slots, size_t capacity, size_t* ring, size_t ring_capacity,
    size_t byte_limit, size_t worker_count, uint64_t startup_deadline) {
  if (abi != KU_TASK_DRIVER_ABI_VERSION) return KU_TASK_DRIVER_ABI_MISMATCH;
  if (!capacity || capacity > KU_TASK_DRIVER_MAX_SLOTS || ring_capacity != capacity
      || !worker_count || worker_count > KU_TASK_DRIVER_MAX_WORKERS)
    return KU_TASK_DRIVER_LIMIT;
  if (startup_deadline == UINT64_MAX) return KU_TASK_DRIVER_INVALID_ARGUMENT;
  if (capacity > (SIZE_MAX - sizeof(*driver)) / (sizeof(*slots) + sizeof(*ring)))
    return KU_TASK_DRIVER_LIMIT;
  size_t slot_bytes = capacity * sizeof(*slots), ring_bytes = capacity * sizeof(*ring);
  size_t fixed = sizeof(*driver) + slot_bytes + ring_bytes;
  if (byte_limit < fixed) return KU_TASK_DRIVER_LIMIT;
  if (!ku_task_frame_storage_valid(driver, bytes, sizeof(*driver), KU_TASK_FRAME_ALIGNOF(KuTaskDriverV1))
      || !ku_task_frame_storage_valid(slots, slot_bytes, slot_bytes, KU_TASK_FRAME_ALIGNOF(KuTaskDriverSlotV1))
      || !ku_task_frame_storage_valid(ring, ring_bytes, ring_bytes, KU_TASK_FRAME_ALIGNOF(size_t))
      || ku_task_frame_ranges_overlap(driver, bytes, slots, slot_bytes)
      || ku_task_frame_ranges_overlap(driver, bytes, ring, ring_bytes)
      || ku_task_frame_ranges_overlap(slots, slot_bytes, ring, ring_bytes)) return KU_TASK_DRIVER_INVALID_ARGUMENT;
  if (!ku_task_frame_zero_bytes(driver, sizeof(*driver)) || !ku_task_frame_zero_bytes(slots, slot_bytes)
      || !ku_task_frame_zero_bytes(ring, ring_bytes)) return KU_TASK_DRIVER_INVALID_STATE;
  uint64_t now = ku_task_driver_now_ms();
  if (now == UINT64_MAX) return KU_TASK_DRIVER_INTERNAL;
  if (now >= startup_deadline) return KU_TASK_DRIVER_SHUTDOWN_TIMEOUT;

  /* Record each native resource before the next fallible acquisition. REAPING
   * blocks ordinary APIs if a later initializer fails. No worker exists yet. */
  driver->abi_version = KU_TASK_DRIVER_ABI_VERSION;
  driver->initialized = KU_TASK_DRIVER_STORAGE_REAPING;
  driver->storage_size = sizeof(*driver); driver->capacity = capacity;
  driver->slots = slots; driver->ring = ring; driver->fixed_bytes = fixed;
  driver->byte_limit = byte_limit; driver->worker_target = worker_count;
  driver->shutdown_deadline = startup_deadline; driver->next_deadline = UINT64_MAX;
  driver->closing = 1; driver->fault = KU_TASK_DRIVER_INTERNAL;
#if defined(_WIN32)
  InitializeSRWLock(&driver->mutex);
  driver->sync_resources |= KU_TASK_DRIVER_SYNC_MUTEX;
  InitializeConditionVariable(&driver->condition);
  driver->sync_resources |= KU_TASK_DRIVER_SYNC_STATE;
  InitializeConditionVariable(&driver->work_condition);
  driver->sync_resources |= KU_TASK_DRIVER_SYNC_WORK;
#else
  if (pthread_mutex_init(&driver->mutex, NULL)) return KU_TASK_DRIVER_INTERNAL;
  driver->sync_resources |= KU_TASK_DRIVER_SYNC_MUTEX;
#if defined(__APPLE__)
  if (pthread_cond_init(&driver->condition, NULL)) return KU_TASK_DRIVER_INTERNAL;
  driver->sync_resources |= KU_TASK_DRIVER_SYNC_STATE;
  if (pthread_cond_init(&driver->work_condition, NULL)) return KU_TASK_DRIVER_INTERNAL;
  driver->sync_resources |= KU_TASK_DRIVER_SYNC_WORK;
#else
  if (pthread_condattr_init(&driver->condition_attributes)) return KU_TASK_DRIVER_INTERNAL;
  driver->sync_resources |= KU_TASK_DRIVER_SYNC_ATTRIBUTES;
  if (pthread_condattr_setclock(&driver->condition_attributes, CLOCK_MONOTONIC))
    return KU_TASK_DRIVER_INTERNAL;
  if (pthread_cond_init(&driver->condition, &driver->condition_attributes))
    return KU_TASK_DRIVER_INTERNAL;
  driver->sync_resources |= KU_TASK_DRIVER_SYNC_STATE;
  if (pthread_cond_init(&driver->work_condition, &driver->condition_attributes))
    return KU_TASK_DRIVER_INTERNAL;
  driver->sync_resources |= KU_TASK_DRIVER_SYNC_WORK;
  if (pthread_condattr_destroy(&driver->condition_attributes)) return KU_TASK_DRIVER_INTERNAL;
  driver->sync_resources &= ~KU_TASK_DRIVER_SYNC_ATTRIBUTES;
#endif
#endif
  if (ku_task_driver_lock(driver)) return KU_TASK_DRIVER_INTERNAL;
  driver->initialized = KU_TASK_DRIVER_STORAGE_LIVE;
  driver->closing = 0; driver->fault = 0; driver->shutdown_deadline = UINT64_MAX;
  uint32_t result = KU_TASK_DRIVER_OK;
  for (size_t index = 0; index < worker_count; index++) {
    now = ku_task_driver_now_ms();
    if (now == UINT64_MAX) { result = KU_TASK_DRIVER_INTERNAL; break; }
    if (now >= startup_deadline) { result = KU_TASK_DRIVER_SHUTDOWN_TIMEOUT; break; }
    KuTaskDriverWorkerV1* worker = &driver->workers[index];
    worker->driver = driver; worker->index = index;
    worker->state = KU_TASK_DRIVER_WORKER_ACTIVE;
#if defined(_WIN32)
    worker->thread = (HANDLE)_beginthreadex(NULL, 0, ku_task_driver_thread, worker, 0, NULL);
    if (!worker->thread) { result = KU_TASK_DRIVER_INTERNAL; break; }
#else
    if (pthread_create(&worker->thread, NULL, ku_task_driver_thread, worker)) {
      result = KU_TASK_DRIVER_INTERNAL; break;
    }
#endif
    /* New entrypoints block on this mutex until every actual handle has been
     * registered. Never expose an unregistered live thread to teardown. */
    driver->workers_created++;
  }
  if (result == KU_TASK_DRIVER_OK) {
    now = ku_task_driver_now_ms();
    if (now == UINT64_MAX) result = KU_TASK_DRIVER_INTERNAL;
    else if (now >= startup_deadline) result = KU_TASK_DRIVER_SHUTDOWN_TIMEOUT;
  }
  if (result != KU_TASK_DRIVER_OK) {
    driver->closing = 1; driver->fault = KU_TASK_DRIVER_INTERNAL;
    driver->shutdown_deadline = startup_deadline;
    if (now == UINT64_MAX) ku_task_driver_enter_clock_fault(driver);
    else ku_task_driver_signal_work(driver);
  }
  ku_task_driver_signal(driver);
  if (result == KU_TASK_DRIVER_OK && driver->fault) {
    result = KU_TASK_DRIVER_INTERNAL; driver->closing = 1;
    driver->shutdown_deadline = startup_deadline;
    ku_task_driver_signal_work(driver);
  }
  if (ku_task_driver_unlock(driver)) return KU_TASK_DRIVER_INTERNAL;
  return result;
}
/* External root owns this live lease until after take. Readiness is checked
 * under the publication mutex, then condition-wait atomically releases it.
 * No callback, recursive child poll, periodic timeout or retain is involved.
 * UINT64_MAX means wait for actual completion; a finite caller deadline is
 * absolute and is never restarted by spurious or unrelated notifications. */
static uint32_t ku_task_driver_wait_result(
    const KuTaskDriverTicketV1* ticket, const KuTaskControlLeaseV1* lease,
    uint64_t deadline) {
  uint32_t checked = ku_task_driver_check_ticket(ticket);
  if (checked != KU_TASK_DRIVER_OK) return checked;
  checked = ku_task_control_check_lease(lease);
  if (checked != KU_TASK_CONTROL_OK) return checked;
  KuTaskDriverV1* driver = ticket->driver;
  if (ku_task_driver_lock(driver)) return KU_TASK_DRIVER_INTERNAL;
  uint32_t result = KU_TASK_DRIVER_PENDING;
  for (;;) {
    KuTaskDriverSlotV1* slot = ku_task_driver_find(ticket);
    if (!slot) { result = KU_TASK_DRIVER_STALE; break; }
    if (slot->binding != lease->control || !slot->driver_lease.control
        || slot->state == KU_TASK_DRIVER_BUILDING || slot->state == KU_TASK_DRIVER_RETIRING) {
      result = KU_TASK_DRIVER_INVALID_STATE; break;
    }
    if (slot->cleanup_fault) { result = slot->cleanup_fault; break; }
    result = ku_task_driver_wait_ready_locked(slot);
    if (result != KU_TASK_DRIVER_PENDING) break;
    if (driver->fault || driver->clock_fault) { result = KU_TASK_DRIVER_INTERNAL; break; }
    uint64_t now = ku_task_driver_now_ms();
    if (now == UINT64_MAX) {
      ku_task_driver_enter_clock_fault(driver); result = KU_TASK_DRIVER_INTERNAL; break;
    }
    if (deadline != UINT64_MAX && now >= deadline) {
      result = KU_TASK_DRIVER_SHUTDOWN_TIMEOUT; break;
    }
    int waited = ku_task_driver_wait(driver, deadline);
    if (waited == -2) ku_task_driver_enter_clock_fault(driver);
    if (waited < 0) { result = KU_TASK_DRIVER_INTERNAL; break; }
    /* Recheck readiness before time: published completion is not undone just
     * because the external root was scheduled after its waiting deadline. */
  }
  ku_task_driver_unlock(driver);
  return result;
}
static uint32_t ku_task_driver_wait_idle(KuTaskDriverV1* driver, uint64_t deadline) {
  uint32_t checked = ku_task_driver_check(driver);
  if (checked != KU_TASK_DRIVER_OK) return checked;
  if (ku_task_driver_lock(driver)) return KU_TASK_DRIVER_INTERNAL;
  uint32_t result = KU_TASK_DRIVER_OK;
  if (driver->clock_fault) { ku_task_driver_unlock(driver); return KU_TASK_DRIVER_INTERNAL; }
  while (driver->queued || driver->running
         || driver->workers_waiting + driver->workers_exited != driver->workers_created
         || (driver->closing && !driver->resident
             && driver->workers_exited != driver->workers_created)) {
    int waited = ku_task_driver_wait(driver, deadline);
    if (waited == -2) { ku_task_driver_enter_clock_fault(driver); result = KU_TASK_DRIVER_INTERNAL; break; }
    if (waited < 0) { result = KU_TASK_DRIVER_INTERNAL; break; }
    uint64_t now = ku_task_driver_now_ms();
    if (now == UINT64_MAX) { ku_task_driver_enter_clock_fault(driver); result = KU_TASK_DRIVER_INTERNAL; break; }
    if (now >= deadline) { result = KU_TASK_DRIVER_SHUTDOWN_TIMEOUT; break; }
  }
  if (driver->fault) result = driver->fault;
  ku_task_driver_unlock(driver); return result;
}
static uint32_t ku_task_driver_shutdown(KuTaskDriverV1* driver, uint64_t deadline) {
  uint32_t checked = ku_task_driver_check(driver);
  if (checked != KU_TASK_DRIVER_OK) return checked;
  if (ku_task_driver_lock(driver)) return KU_TASK_DRIVER_INTERNAL;
  uint64_t now = ku_task_driver_now_ms();
  if (now == UINT64_MAX) {
    ku_task_driver_enter_clock_fault(driver);
    ku_task_driver_unlock(driver); return KU_TASK_DRIVER_INTERNAL;
  }
  if (driver->clock_fault) { ku_task_driver_unlock(driver); return KU_TASK_DRIVER_INTERNAL; }
  if (!driver->closing) {
    uint64_t root = now > UINT64_MAX - 1001u ? UINT64_MAX - 1u : now + 1000u;
    driver->shutdown_deadline = ku_task_driver_min(root, deadline);
    driver->closing = 1;
  } else driver->shutdown_deadline = ku_task_driver_min(driver->shutdown_deadline, deadline);
  uint64_t cleanup_deadline = driver->shutdown_deadline;
  for (size_t i = 0; i < driver->capacity; i++) {
    KuTaskDriverSlotV1* slot = &driver->slots[i];
    if (slot->state == KU_TASK_DRIVER_FREE || slot->state == KU_TASK_DRIVER_RETIRING) continue;
    slot->cancel_pending = 1;
    slot->cancel_deadline = ku_task_driver_min(slot->cancel_deadline, cleanup_deadline);
    ku_task_driver_arm_deadline(driver, slot);
    if (slot->driver_lease.control) {
      /* This R2 operation is exclusively bounded atomics: no callback or
       * allocation can reenter this mutex. The existing protected registry
       * lease makes cancellation independent of spare reference capacity. */
      uint32_t requested = ku_task_control_request_cancel(
          &slot->driver_lease, KU_TASK_CONTROL_CANCELLED, cleanup_deadline);
      if (requested != KU_TASK_CONTROL_OK && requested != KU_TASK_CONTROL_PENDING
          && !ku_task_control_is_terminal(requested)) driver->fault = KU_TASK_DRIVER_INTERNAL;
    }
    ku_task_driver_wait_abort_locked(driver, i);
    if (slot->scope_wait_started) {
      slot->scope_wait_deadline = ku_task_driver_min(slot->scope_wait_deadline, cleanup_deadline);
      ku_task_driver_scope_expire_locked(driver, i, now);
    }
    ku_task_driver_enqueue(driver, i);
  }
  ku_task_driver_signal_work(driver);
  ku_task_driver_signal(driver);
  uint32_t result = KU_TASK_DRIVER_OK;
  while (driver->workers_exited != driver->workers_created || driver->resident
         || driver->building || driver->queued || driver->running) {
    /* Another caller may only tighten the shared D while this observer waits. */
    cleanup_deadline = ku_task_driver_min(cleanup_deadline, driver->shutdown_deadline);
    if (driver->clock_fault) { result = KU_TASK_DRIVER_INTERNAL; break; }
    int waited = ku_task_driver_wait(driver, cleanup_deadline);
    if (waited == -2) { ku_task_driver_enter_clock_fault(driver); result = KU_TASK_DRIVER_INTERNAL; break; }
    if (waited < 0) { result = KU_TASK_DRIVER_INTERNAL; break; }
    uint64_t sampled_now = ku_task_driver_now_ms();
    if (sampled_now == UINT64_MAX) { ku_task_driver_enter_clock_fault(driver); result = KU_TASK_DRIVER_INTERNAL; break; }
    if (sampled_now >= cleanup_deadline) { result = KU_TASK_DRIVER_SHUTDOWN_TIMEOUT; break; }
  }
  if (driver->fault && result == KU_TASK_DRIVER_OK) result = driver->fault;
  ku_task_driver_unlock(driver); return result;
}
/* Exclusive lifecycle validation. No worker/observer accesses records after
 * all EXITED witnesses and all native joins; REAPING never reacquires a mutex
 * which may already have been successfully destroyed on a previous attempt. */
static uint32_t ku_task_driver_reap_ready(KuTaskDriverV1* driver) {
  uint32_t checked = ku_task_driver_check_lifecycle(driver);
  if (checked != KU_TASK_DRIVER_OK) return checked;
  int live = driver->initialized == KU_TASK_DRIVER_STORAGE_LIVE;
  if (live) {
    checked = ku_task_driver_check(driver);
    if (checked != KU_TASK_DRIVER_OK) return checked;
    if (ku_task_driver_lock(driver)) return KU_TASK_DRIVER_INTERNAL;
  }
  uint32_t result = KU_TASK_DRIVER_OK;
  if (!driver->worker_target || driver->worker_target > KU_TASK_DRIVER_MAX_WORKERS
      || driver->workers_created > driver->worker_target
      || driver->workers_waiting > driver->workers_created
      || driver->workers_exited > driver->workers_created
      || driver->workers_waiting + driver->workers_exited > driver->workers_created
      || driver->workers_joined > driver->workers_created)
    result = KU_TASK_DRIVER_INTERNAL;
  else if (!driver->closing || driver->resident || driver->building
      || driver->queued || driver->running || driver->reserved_bytes
      || driver->workers_waiting || driver->workers_exited != driver->workers_created)
    result = KU_TASK_DRIVER_PENDING;
  if (result == KU_TASK_DRIVER_OK) {
    size_t joined = 0;
    for (size_t index = 0; index < driver->workers_created; index++) {
      KuTaskDriverWorkerV1* worker = &driver->workers[index];
      if (worker->driver != driver || worker->index != index
          || worker->state != KU_TASK_DRIVER_WORKER_EXITED
          || worker->joined > 1u || worker->closed > 1u || worker->closed > worker->joined) {
        result = KU_TASK_DRIVER_INTERNAL; break;
      }
      joined += worker->joined;
    }
    if (result == KU_TASK_DRIVER_OK && joined != driver->workers_joined)
      result = KU_TASK_DRIVER_INTERNAL;
  }
  if (live && ku_task_driver_unlock(driver)) return KU_TASK_DRIVER_INTERNAL;
  return result;
}
/* Joining is reaping, not another cleanup budget or permission to free Tasks.
 * Windows checks ready first, then waits only the remaining original D.
 * POSIX has no portable monotonic timed join: all worker/storage callbacks
 * have ended before pthread_join, but the OS-return tail has no hard time bound.
 * A sticky driver fault does not prevent successful resource-only reaping. */
static uint32_t ku_task_driver_join(KuTaskDriverV1* driver, uint64_t deadline) {
  uint32_t checked = ku_task_driver_reap_ready(driver);
  if (checked != KU_TASK_DRIVER_OK) return checked;
  if (deadline == UINT64_MAX) return KU_TASK_DRIVER_INVALID_ARGUMENT;
  deadline = ku_task_driver_min(deadline, driver->shutdown_deadline);
#if !defined(_WIN32)
  (void)deadline;
#endif
  for (size_t index = 0; index < driver->workers_created; index++) {
    KuTaskDriverWorkerV1* worker = &driver->workers[index];
#if defined(_WIN32)
    if (!worker->closed && !worker->thread) return KU_TASK_DRIVER_INTERNAL;
    if (!worker->joined) {
      DWORD waited = WaitForSingleObject(worker->thread, 0);
      if (waited == WAIT_TIMEOUT) {
        uint64_t now = ku_task_driver_now_ms();
        if (now == UINT64_MAX) return KU_TASK_DRIVER_INTERNAL;
        if (now >= deadline) return KU_TASK_DRIVER_PENDING;
        uint64_t remaining = deadline - now;
        DWORD timeout = remaining >= (uint64_t)INFINITE ? INFINITE - 1u : (DWORD)remaining;
        waited = WaitForSingleObject(worker->thread, timeout);
      }
      if (waited == WAIT_TIMEOUT) return KU_TASK_DRIVER_PENDING;
      if (waited != WAIT_OBJECT_0) return KU_TASK_DRIVER_INTERNAL;
      worker->joined = 1; driver->workers_joined++;
    }
    if (!worker->closed) {
      if (!CloseHandle(worker->thread)) return KU_TASK_DRIVER_INTERNAL;
      worker->closed = 1; worker->thread = NULL;
    }
#else
    if (!worker->joined) {
      if (pthread_join(worker->thread, NULL)) return KU_TASK_DRIVER_INTERNAL;
      worker->joined = 1; driver->workers_joined++;
    }
    worker->closed = 1;
#endif
  }
  return KU_TASK_DRIVER_OK;
}
static uint32_t ku_task_driver_destroy(KuTaskDriverV1* driver) {
  uint32_t checked = ku_task_driver_reap_ready(driver);
  if (checked != KU_TASK_DRIVER_OK) return checked;
  if (driver->workers_joined != driver->workers_created) return KU_TASK_DRIVER_PENDING;
  for (size_t index = 0; index < driver->workers_created; index++)
    if (!driver->workers[index].closed) return KU_TASK_DRIVER_PENDING;
  /* All OS threads are reaped, and the caller excludes all other APIs. Keep
   * each successful destructor recorded so a retry never uses a dead primitive. */
  driver->initialized = KU_TASK_DRIVER_STORAGE_REAPING;
#if defined(_WIN32)
  if (driver->sync_resources & ~(KU_TASK_DRIVER_SYNC_MUTEX
      | KU_TASK_DRIVER_SYNC_STATE | KU_TASK_DRIVER_SYNC_WORK)) return KU_TASK_DRIVER_INTERNAL;
  driver->sync_resources = 0; /* SRW locks and Windows conditions have no destructor. */
#else
  if (driver->sync_resources & ~(KU_TASK_DRIVER_SYNC_MUTEX
      | KU_TASK_DRIVER_SYNC_STATE | KU_TASK_DRIVER_SYNC_WORK
      | KU_TASK_DRIVER_SYNC_ATTRIBUTES)) return KU_TASK_DRIVER_INTERNAL;
#if !defined(__APPLE__)
  if (driver->sync_resources & KU_TASK_DRIVER_SYNC_ATTRIBUTES) {
    if (pthread_condattr_destroy(&driver->condition_attributes)) return KU_TASK_DRIVER_INTERNAL;
    driver->sync_resources &= ~KU_TASK_DRIVER_SYNC_ATTRIBUTES;
  }
#else
  if (driver->sync_resources & KU_TASK_DRIVER_SYNC_ATTRIBUTES) return KU_TASK_DRIVER_INTERNAL;
#endif
  if (driver->sync_resources & KU_TASK_DRIVER_SYNC_WORK) {
    if (pthread_cond_destroy(&driver->work_condition)) return KU_TASK_DRIVER_INTERNAL;
    driver->sync_resources &= ~KU_TASK_DRIVER_SYNC_WORK;
  }
  if (driver->sync_resources & KU_TASK_DRIVER_SYNC_STATE) {
    if (pthread_cond_destroy(&driver->condition)) return KU_TASK_DRIVER_INTERNAL;
    driver->sync_resources &= ~KU_TASK_DRIVER_SYNC_STATE;
  }
  if (driver->sync_resources & KU_TASK_DRIVER_SYNC_MUTEX) {
    if (pthread_mutex_destroy(&driver->mutex)) return KU_TASK_DRIVER_INTERNAL;
    driver->sync_resources &= ~KU_TASK_DRIVER_SYNC_MUTEX;
  }
#endif
  driver->initialized = KU_TASK_DRIVER_STORAGE_ZERO;
  return KU_TASK_DRIVER_OK;
}
"#;
