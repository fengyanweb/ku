/* One ledger per service process, shared by all event loops and database clients.
 * These are reservations and retained operation inputs, not libpq/MySQL heap
 * measurements. Third-party heap allocations remain in the process OS domain.
 * No lock is held while allocating, connecting, querying, closing or logging. */
#define KU_FEATURE_DATABASE_BUDGET 1
#include <stdbool.h>
#include <stdint.h>
#include <stdlib.h>
#include <limits.h>
#if defined(_WIN32)
#include <windows.h>
static SRWLOCK ku_database_budget_mutex = SRWLOCK_INIT;
#define KU_DATABASE_LOCK() AcquireSRWLockExclusive(&ku_database_budget_mutex)
#define KU_DATABASE_UNLOCK() ReleaseSRWLockExclusive(&ku_database_budget_mutex)
#else
#include <pthread.h>
static pthread_mutex_t ku_database_budget_mutex = PTHREAD_MUTEX_INITIALIZER;
static void ku_database_budget_lock(void) {
  if (pthread_mutex_lock(&ku_database_budget_mutex)) abort();
}
static void ku_database_budget_unlock(void) {
  if (pthread_mutex_unlock(&ku_database_budget_mutex)) abort();
}
#define KU_DATABASE_LOCK() ku_database_budget_lock()
#define KU_DATABASE_UNLOCK() ku_database_budget_unlock()
#endif

typedef struct KuDatabaseReservation {
  size_t connections;
  size_t waiters;
} KuDatabaseReservation;

typedef struct KuDatabaseBudgetSnapshot {
  size_t max_connections;
  size_t max_waiters;
  size_t max_pending_bytes;
  size_t reserved_connections;
  size_t reserved_waiters;
  size_t pending_operations;
  size_t pending_bytes;
  size_t peak_pending_bytes;
  size_t rejections;
  size_t unknown_results;
  size_t confirmed_cancellations;
  /* Native pg.client slots include an in-flight connection establishment.
   * These gauges are actual pool transitions, not capacity reservations or
   * PostgreSQL backend-session counts. No pool pointer is retained here. */
  size_t pg_client_active_slots;
  size_t pg_client_waiters;
  size_t pg_client_peak_active_slots;
  size_t pg_client_peak_waiters;
  size_t pg_client_acquire_timeouts;
  size_t pg_client_pool_rejections;
  /* Completed pool connection handles retired by close/expiry/broken reuse.
   * Failed attempts destroyed inside the connect helper are not counted. */
  size_t pg_client_discarded_connections;
  size_t pg_client_broken_discards;
  int pg_client_metrics_valid;
  int config_valid;
} KuDatabaseBudgetSnapshot;

static KuDatabaseBudgetSnapshot ku_database_budget_state;
static int ku_database_budget_initialized;

static int ku_database_budget_env(const char* name, size_t fallback,
    size_t maximum, size_t* value) {
  const char* text = getenv(name);
  if (!text || !text[0]) { *value = fallback; return 1; }
  size_t parsed = 0;
  for (size_t index = 0; text[index]; index++) {
    unsigned char digit = (unsigned char)text[index];
    if (index >= 20 || digit < '0' || digit > '9'
        || parsed > (maximum - (size_t)(digit - '0')) / 10) return 0;
    parsed = parsed * 10 + (size_t)(digit - '0');
  }
  if (!parsed || parsed > maximum) return 0;
  *value = parsed;
  return 1;
}

/* Environment is read once; changing it after service startup is unsupported. */
static void ku_database_budget_initialize_locked(void) {
  if (ku_database_budget_initialized) return;
  ku_database_budget_initialized = 1;
  KuDatabaseBudgetSnapshot* state = &ku_database_budget_state;
  state->pg_client_metrics_valid = 1;
  state->config_valid = ku_database_budget_env("KU_DB_MAX_CONNECTIONS", 256,
      65536, &state->max_connections)
      && ku_database_budget_env("KU_DB_MAX_WAITERS", 4096,
          1048576, &state->max_waiters)
      && ku_database_budget_env("KU_DB_MAX_PENDING_BYTES", 64 * 1024 * 1024,
          SIZE_MAX / 2, &state->max_pending_bytes);
}

static int ku_database_budget_reserve(KuDatabaseReservation* reservation,
    size_t connections, size_t waiters) {
  if (!reservation || reservation->connections || reservation->waiters) return 0;
  KU_DATABASE_LOCK();
  ku_database_budget_initialize_locked();
  KuDatabaseBudgetSnapshot* state = &ku_database_budget_state;
  int accepted = state->config_valid
      && connections <= state->max_connections - state->reserved_connections
      && waiters <= state->max_waiters - state->reserved_waiters;
  if (accepted) {
    state->reserved_connections += connections;
    state->reserved_waiters += waiters;
    reservation->connections = connections;
    reservation->waiters = waiters;
  } else if (state->rejections != SIZE_MAX) state->rejections++;
  KU_DATABASE_UNLOCK();
  return accepted;
}

static void ku_database_budget_release(KuDatabaseReservation* reservation) {
  if (!reservation) return;
  KU_DATABASE_LOCK();
  KuDatabaseBudgetSnapshot* state = &ku_database_budget_state;
  if (reservation->connections > state->reserved_connections
      || reservation->waiters > state->reserved_waiters) abort();
  state->reserved_connections -= reservation->connections;
  state->reserved_waiters -= reservation->waiters;
  *reservation = (KuDatabaseReservation){0};
  KU_DATABASE_UNLOCK();
}

/* Charge retained SQL/parameters/key/value plus fixed operation metadata. The
 * caller owns those inputs for the entire call, including pool waiting and late
 * completion. A timed-out HTTP request cannot refund this charge on its behalf. */
static int ku_database_operation_begin(size_t input_bytes, size_t* charged) {
  if (!charged) return 0;
  *charged = 0;
  KU_DATABASE_LOCK();
  ku_database_budget_initialize_locked();
  KuDatabaseBudgetSnapshot* state = &ku_database_budget_state;
  int accepted = state->config_valid && input_bytes <= SIZE_MAX - 64;
  size_t bytes = accepted ? input_bytes + 64 : 0;
  accepted = accepted && bytes <= state->max_pending_bytes - state->pending_bytes
      && state->pending_operations != SIZE_MAX;
  if (accepted) {
    state->pending_operations++;
    state->pending_bytes += bytes;
    if (state->pending_bytes > state->peak_pending_bytes)
      state->peak_pending_bytes = state->pending_bytes;
    *charged = bytes;
  } else if (state->rejections != SIZE_MAX) state->rejections++;
  KU_DATABASE_UNLOCK();
  return accepted;
}

static void ku_database_operation_end(size_t charged) {
  if (!charged) return;
  KU_DATABASE_LOCK();
  KuDatabaseBudgetSnapshot* state = &ku_database_budget_state;
  if (!state->pending_operations || charged > state->pending_bytes) abort();
  state->pending_operations--;
  state->pending_bytes -= charged;
  KU_DATABASE_UNLOCK();
}

static void ku_database_budget_record_unknown(void) {
  KU_DATABASE_LOCK();
  if (ku_database_budget_state.unknown_results != SIZE_MAX)
    ku_database_budget_state.unknown_results++;
  KU_DATABASE_UNLOCK();
}

static void ku_database_budget_record_cancel_confirmed(void) {
  KU_DATABASE_LOCK();
  if (ku_database_budget_state.confirmed_cancellations != SIZE_MAX)
    ku_database_budget_state.confirmed_cancellations++;
  KU_DATABASE_UNLOCK();
}

static KuDatabaseBudgetSnapshot ku_database_budget_snapshot(void) {
  KU_DATABASE_LOCK();
  ku_database_budget_initialize_locked();
  KuDatabaseBudgetSnapshot snapshot = ku_database_budget_state;
  KU_DATABASE_UNLOCK();
  return snapshot;
}

/* Pool locks may precede this ledger lock. Ledger helpers never acquire a
 * pool lock or perform I/O. Instrumentation failure marks the gauges unknown;
 * it must not change a query result, ownership, deadline or cleanup decision. */
static void ku_database_pg_pool_delta(int active, int waiting) {
  KU_DATABASE_LOCK();
  ku_database_budget_initialize_locked();
  KuDatabaseBudgetSnapshot* state = &ku_database_budget_state;
  if ((active < 0 && !state->pg_client_active_slots)
      || (waiting < 0 && !state->pg_client_waiters)
      || (active > 0 && state->pg_client_active_slots == SIZE_MAX)
      || (waiting > 0 && state->pg_client_waiters == SIZE_MAX)) {
    state->pg_client_metrics_valid = 0;
  } else {
    if (active > 0) state->pg_client_active_slots++;
    else if (active < 0) state->pg_client_active_slots--;
    if (waiting > 0) state->pg_client_waiters++;
    else if (waiting < 0) state->pg_client_waiters--;
    if (state->pg_client_active_slots > state->pg_client_peak_active_slots)
      state->pg_client_peak_active_slots = state->pg_client_active_slots;
    if (state->pg_client_waiters > state->pg_client_peak_waiters)
      state->pg_client_peak_waiters = state->pg_client_waiters;
  }
  KU_DATABASE_UNLOCK();
}

enum { KU_PG_METRIC_ACQUIRE_TIMEOUT, KU_PG_METRIC_POOL_REJECTED,
       KU_PG_METRIC_CONNECTION_DISCARDED, KU_PG_METRIC_BROKEN_DISCARDED };
static void ku_database_pg_pool_event(int event) {
  KU_DATABASE_LOCK();
  ku_database_budget_initialize_locked();
  size_t* count = event == KU_PG_METRIC_ACQUIRE_TIMEOUT
      ? &ku_database_budget_state.pg_client_acquire_timeouts
      : event == KU_PG_METRIC_POOL_REJECTED
      ? &ku_database_budget_state.pg_client_pool_rejections
      : event == KU_PG_METRIC_BROKEN_DISCARDED
      ? &ku_database_budget_state.pg_client_broken_discards
      : &ku_database_budget_state.pg_client_discarded_connections;
  if (*count != SIZE_MAX) (*count)++;
  KU_DATABASE_UNLOCK();
}
