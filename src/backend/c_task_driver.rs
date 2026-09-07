//! Internal single-worker driver over the R1 frame/R2 control contracts.
//! Runtime storage is caller-owned; source async and event polling stay gated.

use crate::backend::c::output::COutput;
use crate::error::KuResult;

pub(super) fn emit_runtime(out: &mut COutput) -> KuResult<()> {
    out.check()?;
    out.push_str(DRIVER_ABI);
    out.check()
}

const DRIVER_ABI: &str = r#"
/* Internal driver v1, not a public Task API, netpoll, or M:N scheduler.
 * Caller-owned zero-filled driver/slot/ring storage remains live until destroy
 * succeeds. Every API caller protects that storage; no call races destroy.
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
 *
 * All R2 access after commit uses these wrappers or the sole worker. Published
 * take/cancel progress is notified AFTER R2's final state store. Callbacks set
 * YIELD or WAIT before returning Pending. WAIT is a trusted internal registered
 * progress source with a ticket; it is not permission for user cleanup await.
 * Wake during RUNNING sets NOTIFIED, otherwise one preallocated queue ticket is
 * used. Cancellation/owner retirement never allocate or require extra capacity.
 * Callbacks, control poll, release/dispose and payload destruction run unlocked.
 * The sole lock-held control exception is shutdown's bounded atomic-only
 * request_cancel, protected by the registry lease: it has no callback,
 * allocation, release or wait and must remain so. Cancellation must not need a
 * fresh reference when a control is already at its reference limit.
 * No callback blocks indefinitely or recursively polls another task.
 *
 * owner_drop durably moves the owner to reserved storage, not completion of its
 * cleanup. Parent scope cleanup must separately await retirement acknowledgement
 * without marking normal parent return Cancelled. Pending owner retirement is
 * kept in a slot and waits for publication/take notification, not a retry loop.
 * Terminal owners/late leases still occupy resident count/bytes until dispose.
 * Shutdown cannot steal a user owner. Timeout leaves storage and worker valid;
 * caller releases its owners/registrations and retries shutdown/destroy later.
 * Clock failure is sticky: close admission, cancel with shared deadline zero,
 * and keep the worker for deterministic cleanup. Idle faulted workers wait only
 * for OS condition notifications, never retry a failed clock periodically.
 */
#define KU_TASK_DRIVER_ABI_VERSION 1u
#define KU_TASK_DRIVER_MAX_SLOTS ((size_t)1024u)
enum {
  KU_TASK_DRIVER_OK = 0u, KU_TASK_DRIVER_PENDING = 1u,
  KU_TASK_DRIVER_INVALID_ARGUMENT = 7u, KU_TASK_DRIVER_ABI_MISMATCH = 8u,
  KU_TASK_DRIVER_LIMIT = 9u, KU_TASK_DRIVER_INVALID_STATE = 10u,
  KU_TASK_DRIVER_STALE = 32u, KU_TASK_DRIVER_CLOSED = 33u,
  KU_TASK_DRIVER_SHUTDOWN_TIMEOUT = 34u, KU_TASK_DRIVER_INTERNAL = 35u,
  KU_TASK_DRIVER_START = 0u, KU_TASK_DRIVER_ABORT = 1u,
  KU_TASK_DRIVER_YIELD = 1u, KU_TASK_DRIVER_WAIT = 2u
};
enum {
  KU_TASK_DRIVER_FREE = 0u, KU_TASK_DRIVER_BUILDING = 1u,
  KU_TASK_DRIVER_QUEUED = 2u, KU_TASK_DRIVER_RUNNING = 3u,
  KU_TASK_DRIVER_PARKED = 4u, KU_TASK_DRIVER_RETIRING = 5u,
  KU_TASK_DRIVER_TERMINAL_HELD = 6u,
  KU_TASK_DRIVER_OWNER_USER = 0u, KU_TASK_DRIVER_OWNER_DEFERRED = 1u,
  KU_TASK_DRIVER_OWNER_WORKER = 2u, KU_TASK_DRIVER_OWNER_RELEASED = 3u
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
typedef struct KuTaskDriverSlotV1 {
  uint64_t generation;
  size_t charged_bytes;
  uint32_t state, notified, intent, owner_location;
  uint32_t deadline_fired, cancel_pending;
  uint64_t cancel_deadline;
  size_t wrapper_active;
  KuTaskControlV1* binding;
  KuTaskControlLeaseV1 driver_lease;
  KuTaskControlLeaseV1 execution_lease;
  KuTaskControlOwnerV1 deferred_owner;
} KuTaskDriverSlotV1;
typedef struct KuTaskDriverSnapshotV1 {
  size_t resident, building, queued, running, parked, retiring, terminal_held;
  size_t reserved_bytes, fixed_bytes, byte_limit;
  uint64_t polls, wakes, waits;
  uint32_t closing, worker_exited, worker_waiting, fault, clock_fault;
} KuTaskDriverSnapshotV1;
struct KuTaskDriverV1 {
  uint32_t abi_version, initialized, closing, worker_exited, worker_waiting, fault, clock_fault;
  size_t storage_size, capacity, head, queued, running, resident, building;
  size_t reserved_bytes, fixed_bytes, byte_limit;
  uint64_t polls, wakes, waits, shutdown_deadline, next_deadline;
  KuTaskDriverSlotV1* slots;
  size_t* ring;
  KuTaskDriverMutexV1 mutex;
  KuTaskDriverConditionV1 condition;
#if defined(_WIN32)
  HANDLE thread;
#else
  pthread_t thread;
#endif
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
/* Requires mutex. 0: notification/spurious wake; 1: deadline; -1: OS error;
 * -2: clock failure (distinct from a broken synchronization primitive).
 * The predicate is always rechecked. No fixed-interval polling is used. */
static int ku_task_driver_wait(KuTaskDriverV1* driver, uint64_t deadline) {
  uint64_t now = ku_task_driver_now_ms();
  if (now == UINT64_MAX) return -2;
  if (deadline != UINT64_MAX && now >= deadline) return 1;
#if defined(_WIN32)
  DWORD delay = INFINITE;
  if (deadline != UINT64_MAX) {
    uint64_t remaining = deadline - now;
    delay = remaining >= (uint64_t)INFINITE ? INFINITE - 1u : (DWORD)remaining;
  }
  if (SleepConditionVariableSRW(&driver->condition, &driver->mutex, delay, 0)) return 0;
  return GetLastError() == ERROR_TIMEOUT ? 1 : -1;
#else
  if (deadline == UINT64_MAX) return pthread_cond_wait(&driver->condition, &driver->mutex) == 0 ? 0 : -1;
  uint64_t remaining = deadline - now;
  /* Use at most a day for the OS conversion, then recheck the absolute budget.
   * This is a platform range bound, not a progress-poll interval. */
  if (remaining > UINT64_C(86400000)) remaining = UINT64_C(86400000);
  struct timespec timeout;
#if defined(__APPLE__)
  timeout.tv_sec = (time_t)(remaining / 1000u);
  timeout.tv_nsec = (long)((remaining % 1000u) * 1000000u);
  int result = pthread_cond_timedwait_relative_np(&driver->condition, &driver->mutex, &timeout);
#else
  /* The condition uses CLOCK_MONOTONIC too. Reconstructing from a SECOND clock
   * sample plus the OLD remaining duration would renew the deadline by any
   * preemption between samples. Convert the original absolute budget directly. */
  uint64_t wake_at = now + remaining;
  timeout.tv_sec = (time_t)(wake_at / 1000u);
  if ((uint64_t)timeout.tv_sec != wake_at / 1000u) return -1;
  timeout.tv_nsec = (long)((wake_at % 1000u) * 1000000u);
  int result = pthread_cond_timedwait(&driver->condition, &driver->mutex, &timeout);
#endif
  return result == 0 ? 0 : result == ETIMEDOUT ? 1 : -1;
#endif
}
static int ku_task_driver_wait_without_clock(KuTaskDriverV1* driver) {
#if defined(_WIN32)
  return SleepConditionVariableSRW(&driver->condition, &driver->mutex, INFINITE, 0) ? 0 : -1;
#else
  return pthread_cond_wait(&driver->condition, &driver->mutex) == 0 ? 0 : -1;
#endif
}
static uint32_t ku_task_driver_check(KuTaskDriverV1* driver) {
  if (!ku_task_frame_storage_valid(driver, sizeof(*driver), sizeof(*driver),
                                   KU_TASK_FRAME_ALIGNOF(KuTaskDriverV1)))
    return KU_TASK_DRIVER_INVALID_ARGUMENT;
  if (driver->abi_version != KU_TASK_DRIVER_ABI_VERSION) return KU_TASK_DRIVER_ABI_MISMATCH;
  if (!driver->initialized || driver->storage_size != sizeof(*driver)) return KU_TASK_DRIVER_INVALID_STATE;
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
static void ku_task_driver_arm_deadline(KuTaskDriverV1* driver, KuTaskDriverSlotV1* slot) {
  if (!slot->deadline_fired)
    driver->next_deadline = ku_task_driver_min(driver->next_deadline, slot->cancel_deadline);
}
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
  if (slot->state == KU_TASK_DRIVER_RUNNING) { slot->notified = 1; return; }
  if (slot->state == KU_TASK_DRIVER_QUEUED || slot->state == KU_TASK_DRIVER_BUILDING
      || slot->state == KU_TASK_DRIVER_RETIRING || slot->state == KU_TASK_DRIVER_FREE) return;
  /* At most one entry per resident slot: capacity is reserved at admission. */
  if (driver->queued >= driver->capacity) { driver->fault = KU_TASK_DRIVER_INTERNAL; return; }
  driver->ring[(driver->head + driver->queued) % driver->capacity] = index;
  driver->queued++;
  slot->state = KU_TASK_DRIVER_QUEUED;
  ku_task_driver_signal(driver);
}
/* Requires mutex. Only the first clock failure scans; no periodic retry or
 * normal-code recovery follows. Existing owners/BUILDING reservations remain
 * protected and can later transfer to deterministic worker cleanup. */
static void ku_task_driver_enter_clock_fault(KuTaskDriverV1* driver) {
  if (driver->clock_fault) { ku_task_driver_signal(driver); return; }
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
    ku_task_driver_enqueue(driver, i);
  }
  ku_task_driver_signal(driver);
}
static uint32_t ku_task_driver_wake(const KuTaskDriverTicketV1* ticket) {
  uint32_t checked = ku_task_driver_check_ticket(ticket);
  if (checked != KU_TASK_DRIVER_OK) return checked;
  KuTaskDriverV1* driver = ticket->driver;
  if (ku_task_driver_lock(driver)) return KU_TASK_DRIVER_INTERNAL;
  KuTaskDriverSlotV1* slot = ku_task_driver_find(ticket);
  uint32_t result = slot ? KU_TASK_DRIVER_OK : KU_TASK_DRIVER_STALE;
  if (slot) {
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
      || slot->state == KU_TASK_DRIVER_RETIRING ? KU_TASK_DRIVER_INVALID_STATE : KU_TASK_DRIVER_OK;
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
  else { slot->wrapper_active--; ku_task_driver_enqueue(driver, ticket->slot); }
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
static uint32_t ku_task_driver_owner_drop(
    const KuTaskDriverTicketV1* ticket, KuTaskControlOwnerV1* owner, uint64_t deadline) {
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
      slot->cancel_deadline = ku_task_driver_min(slot->cancel_deadline, deadline);
      ku_task_driver_arm_deadline(driver, slot);
      slot->cancel_pending = 1;
      ku_task_driver_enqueue(driver, ticket->slot);
    }
  }
  ku_task_driver_unlock(driver);
  return result;
}
static uint32_t ku_task_driver_reserve(
    KuTaskDriverV1* driver, size_t task_bytes, KuTaskDriverTicketV1* output) {
  uint32_t checked = ku_task_driver_check(driver);
  if (checked != KU_TASK_DRIVER_OK) return checked;
  if (!ku_task_driver_external_storage(driver, output, sizeof(*output),
                                       KU_TASK_FRAME_ALIGNOF(KuTaskDriverTicketV1)) || output->driver)
    return KU_TASK_DRIVER_INVALID_ARGUMENT;
  if (!task_bytes) return KU_TASK_DRIVER_INVALID_ARGUMENT;
  if (ku_task_driver_lock(driver)) return KU_TASK_DRIVER_INTERNAL;
  uint32_t result = KU_TASK_DRIVER_LIMIT;
  if (driver->closing || driver->worker_exited) result = KU_TASK_DRIVER_CLOSED;
  else if (driver->fault) result = driver->fault;
  else if (driver->resident < driver->capacity
      && task_bytes <= driver->byte_limit - driver->fixed_bytes - driver->reserved_bytes) {
    for (size_t i = 0; i < driver->capacity; i++) {
      KuTaskDriverSlotV1* slot = &driver->slots[i];
      if (slot->state != KU_TASK_DRIVER_FREE || slot->generation == UINT64_MAX) continue;
      uint64_t generation = slot->generation + 1;
      memset(slot, 0, sizeof(*slot));
      slot->generation = generation;
      slot->state = KU_TASK_DRIVER_BUILDING;
      slot->charged_bytes = task_bytes;
      slot->cancel_deadline = UINT64_MAX;
      driver->resident++; driver->building++; driver->reserved_bytes += task_bytes;
      *output = (KuTaskDriverTicketV1){ driver, i, generation };
      result = KU_TASK_DRIVER_OK; break;
    }
  }
  ku_task_driver_unlock(driver);
  return result;
}
static void ku_task_driver_return_slot(KuTaskDriverV1* driver, KuTaskDriverSlotV1* slot) {
  driver->reserved_bytes -= slot->charged_bytes;
  driver->resident--;
  uint64_t generation = slot->generation;
  memset(slot, 0, sizeof(*slot));
  slot->generation = generation;
  ku_task_driver_signal(driver);
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
    driver->building--; ku_task_driver_return_slot(driver, slot);
    *ticket = (KuTaskDriverTicketV1){0};
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
    ku_task_driver_return_slot(driver, slot);
    *ticket = (KuTaskDriverTicketV1){0};
  }
  ku_task_driver_unlock(driver);
  return result;
}
static uint32_t ku_task_driver_commit(
    const KuTaskDriverTicketV1* ticket, KuTaskControlOwnerV1* owner,
    uint32_t mode, uint64_t deadline) {
  uint32_t checked = ku_task_driver_check_ticket(ticket);
  if (checked != KU_TASK_DRIVER_OK) return checked;
  if (mode != KU_TASK_DRIVER_START && mode != KU_TASK_DRIVER_ABORT) return KU_TASK_DRIVER_INVALID_ARGUMENT;
  if (!ku_task_frame_storage_valid(owner, sizeof(*owner), sizeof(*owner),
                                   KU_TASK_FRAME_ALIGNOF(KuTaskControlOwnerV1))) return KU_TASK_DRIVER_INVALID_ARGUMENT;
  checked = ku_task_control_check_lease(&owner->lease);
  if (checked != KU_TASK_CONTROL_OK) return checked;
  KuTaskControlLeaseV1 registry = {0}, execution = {0};
  checked = ku_task_control_lease_retain(&owner->lease, &registry);
  if (checked != KU_TASK_CONTROL_OK) return checked;
  checked = ku_task_control_lease_retain(&owner->lease, &execution);
  if (checked != KU_TASK_CONTROL_OK) { ku_task_control_lease_release(&registry); return checked; }
  KuTaskDriverV1* driver = ticket->driver;
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
  uint32_t result = !slot ? KU_TASK_DRIVER_STALE : slot->state != KU_TASK_DRIVER_BUILDING
      ? KU_TASK_DRIVER_INVALID_STATE : KU_TASK_DRIVER_OK;
  if (result == KU_TASK_DRIVER_OK) {
    for (size_t i = 0; i < driver->capacity; i++) {
      if (driver->slots[i].binding == registry.control) { result = KU_TASK_DRIVER_INVALID_STATE; break; }
    }
  }
  if (result == KU_TASK_DRIVER_OK && mode == KU_TASK_DRIVER_ABORT)
    result = ku_task_control_owner_move(&slot->deferred_owner, owner);
  if (result == KU_TASK_DRIVER_OK) {
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
static void ku_task_driver_snapshot_locked(KuTaskDriverV1* driver, KuTaskDriverSnapshotV1* output) {
  memset(output, 0, sizeof(*output));
  output->resident = driver->resident; output->building = driver->building;
  output->queued = driver->queued; output->running = driver->running;
  output->reserved_bytes = driver->reserved_bytes; output->fixed_bytes = driver->fixed_bytes;
  output->byte_limit = driver->byte_limit;
  output->polls = driver->polls; output->wakes = driver->wakes; output->waits = driver->waits;
  output->closing = driver->closing; output->worker_exited = driver->worker_exited;
  output->worker_waiting = driver->worker_waiting; output->fault = driver->fault;
  output->clock_fault = driver->clock_fault;
  for (size_t i = 0; i < driver->capacity; i++) {
    switch (driver->slots[i].state) {
      case KU_TASK_DRIVER_PARKED: output->parked++; break;
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
        || slot->state == KU_TASK_DRIVER_RETIRING || slot->deadline_fired
        || slot->cancel_deadline == UINT64_MAX) continue;
    if (slot->cancel_deadline <= now) {
      slot->deadline_fired = 1;
      ku_task_driver_enqueue(driver, i);
    } else next = ku_task_driver_min(next, slot->cancel_deadline);
  }
  driver->next_deadline = next;
  return next;
}
static void ku_task_driver_worker(KuTaskDriverV1* driver) {
  if (ku_task_driver_lock(driver)) return;
  for (;;) {
    uint64_t sampled_now = driver->clock_fault ? 0 : ku_task_driver_now_ms();
    if (sampled_now == UINT64_MAX) ku_task_driver_enter_clock_fault(driver);
    uint64_t next_deadline = driver->next_deadline;
    if (!driver->clock_fault && next_deadline != UINT64_MAX && sampled_now >= next_deadline)
      next_deadline = ku_task_driver_deadlines_locked(driver, sampled_now);
    if (driver->closing && !driver->resident && !driver->running && !driver->queued) break;
    if (!driver->queued) {
      driver->worker_waiting = 1; if (driver->waits != UINT64_MAX) driver->waits++;
      ku_task_driver_signal(driver);
      int waited = driver->clock_fault ? ku_task_driver_wait_without_clock(driver)
          : ku_task_driver_wait(driver, next_deadline);
      driver->worker_waiting = 0;
      if (waited == -2) { ku_task_driver_enter_clock_fault(driver); continue; }
      if (waited < 0) { driver->fault = KU_TASK_DRIVER_INTERNAL; break; }
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
    KuTaskControlLeaseV1 registry = {0};
    if (terminal && slot->owner_location == KU_TASK_DRIVER_OWNER_RELEASED && !slot->wrapper_active) {
      registry = slot->driver_lease; slot->driver_lease.control = NULL;
      /* The disposer frees its allocation BEFORE returning this slot's budget.
       * Another builder may legitimately receive the same malloc address in
       * that interval. Retire the executable binding now, while keeping its
       * generation/count/bytes until final disposed acknowledgement. */
      slot->binding = NULL;
      slot->state = KU_TASK_DRIVER_RETIRING;
    } else {
      slot->execution_lease = execution; execution.control = NULL;
      slot->state = terminal ? KU_TASK_DRIVER_TERMINAL_HELD : KU_TASK_DRIVER_PARKED;
      if (slot->notified || (!terminal && slot->intent == KU_TASK_DRIVER_YIELD && !slot->deadline_fired))
        ku_task_driver_enqueue(driver, index);
      else if (!terminal && outcome == KU_TASK_CONTROL_PENDING && !slot->intent
               && !slot->wrapper_active && dropped != KU_TASK_CONTROL_PENDING && request != KU_TASK_CONTROL_PENDING) {
        /* An adapter returned Pending without a registered progress source.
         * Retain its storage and report fault; do not hide it by busy polling. */
        driver->fault = KU_TASK_DRIVER_INTERNAL;
      } else if (!terminal && outcome != KU_TASK_CONTROL_PENDING) driver->fault = KU_TASK_DRIVER_INTERNAL;
    }
    ku_task_driver_signal(driver);
    ku_task_driver_unlock(driver);
    if (execution.control) ku_task_control_lease_release(&execution);
    if (registry.control) ku_task_control_lease_release(&registry);
    if (ku_task_driver_lock(driver)) return;
  }
  driver->worker_exited = 1; driver->worker_waiting = 0;
  ku_task_driver_signal(driver);
  ku_task_driver_unlock(driver);
  /* No further driver or task storage access after this point. */
}
#if defined(_WIN32)
static unsigned __stdcall ku_task_driver_thread(void* raw) {
  ku_task_driver_worker((KuTaskDriverV1*)raw); return 0;
}
#else
static void* ku_task_driver_thread(void* raw) {
  ku_task_driver_worker((KuTaskDriverV1*)raw); return NULL;
}
#endif
static uint32_t ku_task_driver_init(
    KuTaskDriverV1* driver, size_t bytes, uint32_t abi,
    KuTaskDriverSlotV1* slots, size_t capacity, size_t* ring, size_t ring_capacity,
    size_t byte_limit) {
  if (abi != KU_TASK_DRIVER_ABI_VERSION) return KU_TASK_DRIVER_ABI_MISMATCH;
  if (!capacity || capacity > KU_TASK_DRIVER_MAX_SLOTS || ring_capacity != capacity)
    return KU_TASK_DRIVER_LIMIT;
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
  if (ku_task_driver_now_ms() == UINT64_MAX) return KU_TASK_DRIVER_INTERNAL;
#if defined(_WIN32)
  InitializeSRWLock(&driver->mutex); InitializeConditionVariable(&driver->condition);
#else
  if (pthread_mutex_init(&driver->mutex, NULL)) { memset(driver, 0, sizeof(*driver)); return KU_TASK_DRIVER_INTERNAL; }
  int condition_result;
#if defined(__APPLE__)
  condition_result = pthread_cond_init(&driver->condition, NULL);
#else
  pthread_condattr_t attributes;
  condition_result = pthread_condattr_init(&attributes);
  if (!condition_result) {
    condition_result = pthread_condattr_setclock(&attributes, CLOCK_MONOTONIC);
    if (!condition_result) condition_result = pthread_cond_init(&driver->condition, &attributes);
    pthread_condattr_destroy(&attributes);
  }
#endif
  if (condition_result) { pthread_mutex_destroy(&driver->mutex); memset(driver, 0, sizeof(*driver)); return KU_TASK_DRIVER_INTERNAL; }
#endif
  driver->abi_version = KU_TASK_DRIVER_ABI_VERSION; driver->initialized = 1;
  driver->storage_size = sizeof(*driver); driver->capacity = capacity;
  driver->slots = slots; driver->ring = ring; driver->fixed_bytes = fixed;
  driver->byte_limit = byte_limit; driver->shutdown_deadline = UINT64_MAX;
  driver->next_deadline = UINT64_MAX;
#if defined(_WIN32)
  driver->thread = (HANDLE)_beginthreadex(NULL, 0, ku_task_driver_thread, driver, 0, NULL);
  if (!driver->thread) { memset(driver, 0, sizeof(*driver)); return KU_TASK_DRIVER_INTERNAL; }
#else
  if (pthread_create(&driver->thread, NULL, ku_task_driver_thread, driver)) {
    pthread_cond_destroy(&driver->condition); pthread_mutex_destroy(&driver->mutex);
    memset(driver, 0, sizeof(*driver)); return KU_TASK_DRIVER_INTERNAL;
  }
#endif
  return KU_TASK_DRIVER_OK;
}
static uint32_t ku_task_driver_wait_idle(KuTaskDriverV1* driver, uint64_t deadline) {
  uint32_t checked = ku_task_driver_check(driver);
  if (checked != KU_TASK_DRIVER_OK) return checked;
  if (ku_task_driver_lock(driver)) return KU_TASK_DRIVER_INTERNAL;
  uint32_t result = KU_TASK_DRIVER_OK;
  if (driver->clock_fault) { ku_task_driver_unlock(driver); return KU_TASK_DRIVER_INTERNAL; }
  while (driver->queued || driver->running || (!driver->worker_waiting && !driver->worker_exited)
         || (driver->closing && !driver->resident && !driver->worker_exited)) {
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
    uint64_t root = now > UINT64_MAX - 1000u ? UINT64_MAX : now + 1000u;
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
    ku_task_driver_enqueue(driver, i);
  }
  ku_task_driver_signal(driver);
  uint32_t result = KU_TASK_DRIVER_OK;
  while (!driver->worker_exited || driver->resident) {
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
static uint32_t ku_task_driver_destroy(KuTaskDriverV1* driver) {
  uint32_t checked = ku_task_driver_check(driver);
  if (checked != KU_TASK_DRIVER_OK) return checked;
  if (ku_task_driver_lock(driver)) return KU_TASK_DRIVER_INTERNAL;
  int safe = driver->worker_exited && !driver->resident && !driver->building
      && !driver->queued && !driver->running;
  ku_task_driver_unlock(driver);
  if (!safe) return KU_TASK_DRIVER_PENDING;
  /* Worker has completed all callback/driver storage access. It only returns
   * from its OS entrypoint after publishing worker_exited. No join waits for
   * user code; the deadline-sensitive wait belongs to shutdown above. */
#if defined(_WIN32)
  if (WaitForSingleObject(driver->thread, 0) != WAIT_OBJECT_0) return KU_TASK_DRIVER_PENDING;
  if (!CloseHandle(driver->thread)) return KU_TASK_DRIVER_INTERNAL;
#else
  if (pthread_join(driver->thread, NULL)) return KU_TASK_DRIVER_INTERNAL;
  if (pthread_cond_destroy(&driver->condition) || pthread_mutex_destroy(&driver->mutex)) return KU_TASK_DRIVER_INTERNAL;
#endif
  driver->initialized = 0;
  return KU_TASK_DRIVER_OK;
}
"#;
