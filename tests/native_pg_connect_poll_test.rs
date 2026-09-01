//! Closed-loop tests for the generated native PostgreSQL connection poller.
//! The harness supplies a deterministic libpq and OS-poll surface.

#[allow(dead_code)]
#[path = "support/native_pg_harness.rs"]
mod native_pg_harness;

use std::fs;
use std::process::Command;

use native_pg_harness::{compile_harness, emit_c, run_bounded, TempDir, RUN_LIMITS, RUN_TIMEOUT};

fn pg_fixture() -> &'static str {
    r#"import pg from "std.pg"
fn main(): null! {
    client = pg.client({
        conninfo: "host=db-a,db-b password=fixture-secret",
        max_connections: 2,
        max_waiters: 8,
        connect_timeout_ms: 5000,
        acquire_timeout_ms: 5000,
        query_timeout_ms: 30000
    })?
    client.close()
    return ok(null)
}
"#
}

#[test]
fn native_pg_connect_poller_is_portable_bounded_and_lean() {
    let directory = TempDir::new("source");
    let generated = emit_c(directory.path(), pg_fixture());

    for expected in [
        "extern PGconn* PQconnectStartParams",
        "#define KU_FEATURE_LIBPQ 1",
        "extern int PQconnectPoll(PGconn*)",
        "extern int PQsocket(const PGconn*)",
        "#define KU_PGRES_POLLING_FAILED 0",
        "#define KU_PGRES_POLLING_READING 1",
        "#define KU_PGRES_POLLING_WRITING 2",
        "#define KU_PGRES_POLLING_OK 3",
        "#define KU_PGRES_POLLING_ACTIVE 4",
        "#define KU_PG_CONNECT_OUT_OF_MEMORY 3",
        "static KuPgConnectAttempt ku_pg_connect_until",
        "PGconn* h = PQconnectStartParams(keywords, values, 1)",
        "const char* keywords[] = { \"dbname\", \"client_encoding\", 0 }",
        "const char* values[] = { conninfo, \"UTF8\", 0 }",
        "int wait_result = ku_pg_wait_socket(PQsocket(h), direction, deadline)",
        "if (WSAGetLastError() == WSAEINTR) continue",
        "if (errno == EINTR) continue",
        "WSAPoll(&item, 1, timeout_ms)",
        "poll(&item, 1, timeout_ms)",
        "remaining > 2147483647ULL ? 2147483647 : (int)remaining",
        "direction == KU_PG_WAIT_ACTIVE && timeout_ms > 1",
        "if (now >= deadline) { PQfinish(h); attempt.outcome = KU_PG_CONNECT_TIMED_OUT",
        "if (PQstatus(h) != KU_PG_CONNECTION_OK || !ku_pg_connection_is_utf8(h))",
        "#define KU_PG_CLIENT_DEFAULT_CONNECT_TIMEOUT_MS 5000LL",
        "#define KU_PG_CLIENT_DEFAULT_MAX_CONNECTIONS 8LL",
        "#define KU_PG_CLIENT_DEFAULT_MAX_WAITERS 64LL",
        "KuPgConnectAttempt initial_attempt = ku_pg_connect_until(p->conninfo, initial_deadline)",
        "KuPgConnectAttempt connect_attempt = ku_pg_connect_until(p->conninfo, connect_deadline)",
        "if (__ku_handler_deadline != 0 && __ku_handler_deadline < deadline)",
        "#include <winsock2.h>",
        "#include <limits.h>",
        "#include <sys/socket.h>",
        "#include <poll.h>",
        "#include <pthread.h>",
    ] {
        assert!(
            generated.contains(expected),
            "generated PG poll runtime missed: {expected}"
        );
    }
    for forbidden in [
        "PQconnectdb(",
        "PQconnectdbParams(",
        "PQerrorMessage",
        "#pragma comment(lib, \"libpq.lib\")",
        "PGconn* initial = ku_pg_connectdb_timeout",
        "PGconn* h = ku_pg_connectdb_timeout",
    ] {
        assert!(
            !generated.contains(forbidden),
            "generated PG connection path retained forbidden synchronous/secret accessor: {forbidden}"
        );
    }

    assert!(generated.contains("static KuResult_pg_client ku_pg_client(KuObject* config)"));
    assert!(!generated.contains("KuResult_pg_conn"));
}

#[test]
fn native_pg_connect_poller_stub_closes_every_owned_failure_and_reuses_pool() {
    let directory = TempDir::new("harness");
    let generated = emit_c(directory.path(), pg_fixture());
    let hook = r#"
#if defined(_WIN32)
static int ku_test_pg_os_poll(WSAPOLLFD*, ULONG, INT);
#define WSAPoll ku_test_pg_os_poll
#else
static int ku_test_pg_os_poll(struct pollfd*, nfds_t, int);
#define poll ku_test_pg_os_poll
#endif
static unsigned long long ku_test_pg_now_ms(void);
#define KU_PG_MONOTONIC_MS() ku_test_pg_now_ms()
"#;
    let mut harness = generated
        .replacen(
            "typedef struct pg_conn PGconn;",
            &format!("{hook}\ntypedef struct pg_conn PGconn;"),
            1,
        )
        .replacen(
            "int main(void) {",
            "static int ku_generated_main(void) {",
            1,
        );
    harness.push_str(
        r#"
struct pg_conn {
  int id;
  int status;
  int socket_fd;
  int poll_index;
  int poll_count;
  int poll_states[4];
  int utf8;
};
struct pg_result { int unused; };

enum {
  KU_TEST_OK = 1,
  KU_TEST_FAILED = 2,
  KU_TEST_TIMEOUT = 3,
  KU_TEST_INVALID_SOCKET = 4,
  KU_TEST_START_OVERRUN = 5,
  KU_TEST_ACTIVE_OK = 6,
  KU_TEST_READ_WRITE_OK = 7,
  KU_TEST_STATUS_BAD = 8,
  KU_TEST_ENCODING_BAD = 9,
  KU_TEST_UNKNOWN_POLL = 10,
  KU_TEST_POLL_ERROR = 11,
  KU_TEST_INTERRUPTED_OK = 12,
  KU_TEST_NULL_START = 13,
  KU_TEST_HUP_RETRY_OK = 14
};

static unsigned long long ku_test_clock = 100;
static int ku_test_mode = KU_TEST_OK;
static int ku_test_start_calls = 0;
static int ku_test_finish_calls = 0;
static int ku_test_live_connections = 0;
static int ku_test_poll_calls = 0;
static int ku_test_active_waits = 0;
static int ku_test_interrupts = 0;
static int ku_test_interrupt_pending = 0;
static int ku_test_hup_pending = 0;
static int ku_test_exec_calls = 0;
static int ku_test_clear_calls = 0;
static int ku_test_encoding_set_calls = 0;
static int ku_test_params_valid = 0;
static int ku_test_pending_result = 0;
static struct pg_result ku_test_result;

static unsigned long long ku_test_pg_now_ms(void) { return ku_test_clock; }

#if defined(_WIN32)
static int ku_test_pg_os_poll(WSAPOLLFD* item, ULONG count, INT timeout_ms) {
  if (!item || count != 1 || timeout_ms < 0) { WSASetLastError(WSAEINVAL); return -1; }
  if (ku_test_mode == KU_TEST_INTERRUPTED_OK && ku_test_interrupt_pending) {
    ku_test_interrupt_pending = 0; ku_test_interrupts++; WSASetLastError(WSAEINTR); return -1;
  }
  if (ku_test_mode == KU_TEST_POLL_ERROR) { WSASetLastError(WSAEINVAL); return -1; }
  if (ku_test_mode == KU_TEST_TIMEOUT) { ku_test_clock += (unsigned long long)timeout_ms; return 0; }
  if (ku_test_mode == KU_TEST_HUP_RETRY_OK && ku_test_hup_pending) {
    ku_test_hup_pending = 0; item->revents = POLLHUP; return 1;
  }
  if (item->events == 0) { ku_test_active_waits++; ku_test_clock += (unsigned long long)timeout_ms; return 0; }
  item->revents = item->events; return 1;
}
#else
static int ku_test_pg_os_poll(struct pollfd* item, nfds_t count, int timeout_ms) {
  if (!item || count != 1 || timeout_ms < 0) { errno = EINVAL; return -1; }
  if (ku_test_mode == KU_TEST_INTERRUPTED_OK && ku_test_interrupt_pending) {
    ku_test_interrupt_pending = 0; ku_test_interrupts++; errno = EINTR; return -1;
  }
  if (ku_test_mode == KU_TEST_POLL_ERROR) { errno = EINVAL; return -1; }
  if (ku_test_mode == KU_TEST_TIMEOUT) { ku_test_clock += (unsigned long long)timeout_ms; return 0; }
  if (ku_test_mode == KU_TEST_HUP_RETRY_OK && ku_test_hup_pending) {
    ku_test_hup_pending = 0; item->revents = POLLHUP; return 1;
  }
  if (item->events == 0) { ku_test_active_waits++; ku_test_clock += (unsigned long long)timeout_ms; return 0; }
  item->revents = item->events; return 1;
}
#endif

PGconn* PQconnectStartParams(const char* const* keys, const char* const* values, int expand) {
  ku_test_start_calls++;
  ku_test_params_valid = expand == 1
      && keys && values
      && keys[0] && strcmp(keys[0], "dbname") == 0
      && keys[1] && strcmp(keys[1], "client_encoding") == 0
      && keys[2] == 0
      && values[0]
      && values[1] && strcmp(values[1], "UTF8") == 0
      && values[2] == 0;
  if (ku_test_mode == KU_TEST_NULL_START) return 0;
  PGconn* connection = (PGconn*)calloc(1, sizeof(PGconn));
  if (!connection) return 0;
  ku_test_live_connections++;
  connection->id = ku_test_start_calls;
  connection->status = ku_test_mode == KU_TEST_STATUS_BAD ? KU_PG_CONNECTION_BAD : KU_PG_CONNECTION_OK;
  connection->socket_fd = ku_test_mode == KU_TEST_INVALID_SOCKET ? -1 : 7;
  connection->utf8 = ku_test_mode != KU_TEST_ENCODING_BAD;
  if (ku_test_mode == KU_TEST_FAILED) {
    connection->poll_states[0] = KU_PGRES_POLLING_FAILED; connection->poll_count = 1;
  } else if (ku_test_mode == KU_TEST_ACTIVE_OK) {
    connection->poll_states[0] = KU_PGRES_POLLING_ACTIVE;
    connection->poll_states[1] = KU_PGRES_POLLING_OK;
    connection->poll_count = 2;
  } else if (ku_test_mode == KU_TEST_READ_WRITE_OK) {
    connection->poll_states[0] = KU_PGRES_POLLING_READING;
    connection->poll_states[1] = KU_PGRES_POLLING_WRITING;
    connection->poll_states[2] = KU_PGRES_POLLING_OK;
    connection->poll_count = 3;
  } else if (ku_test_mode == KU_TEST_UNKNOWN_POLL) {
    connection->poll_states[0] = 77; connection->poll_count = 1;
  } else if (ku_test_mode == KU_TEST_HUP_RETRY_OK) {
    connection->poll_states[0] = KU_PGRES_POLLING_WRITING;
    connection->poll_states[1] = KU_PGRES_POLLING_OK;
    connection->poll_count = 2;
  } else {
    connection->poll_states[0] = KU_PGRES_POLLING_OK; connection->poll_count = 1;
  }
  if (ku_test_mode == KU_TEST_START_OVERRUN) ku_test_clock += 100;
  if (ku_test_mode == KU_TEST_INTERRUPTED_OK) ku_test_interrupt_pending = 1;
  if (ku_test_mode == KU_TEST_HUP_RETRY_OK) ku_test_hup_pending = 1;
  return connection;
}

int PQconnectPoll(PGconn* connection) {
  ku_test_poll_calls++;
  if (!connection || connection->poll_index >= connection->poll_count) return KU_PGRES_POLLING_FAILED;
  return connection->poll_states[connection->poll_index++];
}
int PQsocket(const PGconn* connection) { return connection ? connection->socket_fd : -1; }
int PQstatus(const PGconn* connection) { return connection ? connection->status : KU_PG_CONNECTION_BAD; }
int PQsetClientEncoding(PGconn* connection, const char* encoding) {
  (void)connection; (void)encoding; ku_test_encoding_set_calls++; return 0;
}
const char* PQparameterStatus(const PGconn* connection, const char* name) {
  (void)name; return connection && connection->utf8 ? "UTF8" : "LATIN1";
}
void PQfinish(PGconn* connection) {
  if (!connection) return;
  ku_test_finish_calls++; ku_test_live_connections--; free(connection);
}
int PQsetnonblocking(PGconn* connection, int mode) { (void)connection; return mode == 1 ? 0 : -1; }
int PQflush(PGconn* connection) { (void)connection; return 0; }
int PQconsumeInput(PGconn* connection) { (void)connection; return 1; }
int PQisBusy(PGconn* connection) { (void)connection; return 0; }
int PQsetSingleRowMode(PGconn* connection) { (void)connection; return 1; }
PGresult* PQgetResult(PGconn* connection) { (void)connection; if (ku_test_pending_result) { ku_test_pending_result = 0; return &ku_test_result; } return 0; }
int PQsendQuery(PGconn* connection, const char* sql) {
  (void)connection; (void)sql; ku_test_exec_calls++; ku_test_pending_result = 1; return 1;
}
int PQsendQueryParams(PGconn* connection, const char* sql, int count, const void* types, const char* const* values, const int* lengths, const int* formats, int result_format) {
  (void)connection; (void)sql; (void)count; (void)types; (void)values; (void)lengths; (void)formats; (void)result_format;
  ku_test_exec_calls++; ku_test_pending_result = 1; return 1;
}
int PQresultStatus(const PGresult* result) { (void)result; return KU_PGRES_COMMAND_OK; }
char* PQresultErrorMessage(const PGresult* result) { (void)result; return "query failed"; }
int PQntuples(const PGresult* result) { (void)result; return 0; }
int PQnfields(const PGresult* result) { (void)result; return 0; }
int PQfformat(const PGresult* result, int column) { (void)result; (void)column; return 0; }
char* PQgetvalue(const PGresult* result, int row, int column) { (void)result; (void)row; (void)column; return ""; }
int PQgetisnull(const PGresult* result, int row, int column) { (void)result; (void)row; (void)column; return 1; }
int PQgetlength(const PGresult* result, int row, int column) { (void)result; (void)row; (void)column; return 0; }
int PQtransactionStatus(const PGconn* connection) { (void)connection; return KU_PQTRANS_IDLE; }
void PQclear(PGresult* result) { (void)result; ku_test_clear_calls++; }

static int ku_test_attempt(int mode, unsigned long long budget_ms, int expected_outcome) {
  ku_test_mode = mode;
  int starts = ku_test_start_calls;
  int finishes = ku_test_finish_calls;
  KuPgConnectAttempt attempt = ku_pg_connect_until("host=db-a,db-b password=fixture-secret", ku_test_clock + budget_ms);
  if (attempt.outcome != expected_outcome) return 0;
  if (expected_outcome == KU_PG_CONNECT_OK) {
    if (!attempt.conn) return 0;
    PQfinish(attempt.conn);
  } else if (attempt.conn) {
    return 0;
  }
  if (ku_test_start_calls != starts + 1) return 0;
  if (mode == KU_TEST_NULL_START) return ku_test_finish_calls == finishes;
  return ku_test_finish_calls == finishes + 1;
}

static int ku_test_string_is(KuString value, const char* expected) {
  size_t length = strlen(expected);
  return value.len == length && (length == 0 || (value.ptr && memcmp(value.ptr, expected, length) == 0));
}

static int ku_test_timeout_error(KuError error) {
  return ku_test_string_is(error.domain, "pg")
      && ku_test_string_is(error.code, "connect_timeout")
      && ku_test_string_is(error.message, "PostgreSQL client connection timed out");
}

static int ku_test_oom_error(KuError error) {
  return ku_test_string_is(error.domain, "pg")
      && ku_test_string_is(error.code, "out_of_memory")
      && ku_test_string_is(error.message, "PostgreSQL allocation failed");
}

int main(void) {
  KuString conninfo = ku_string_static((const uint8_t*)"host=db-a,db-b password=fixture-secret", sizeof("host=db-a,db-b password=fixture-secret") - 1);

  ku_test_mode = KU_TEST_OK;
  KuPgConnectAttempt direct = ku_pg_connect_until("host=db-a,db-b password=fixture-secret", ku_test_clock + 5000);
  if (!direct.conn || direct.outcome != KU_PG_CONNECT_OK || !ku_test_params_valid) return 10;
  PQfinish(direct.conn);

  ku_test_mode = KU_TEST_TIMEOUT;
  unsigned long long before_default_timeout = ku_test_clock;
  KuPgConnectAttempt default_timeout = ku_pg_connect_until("host=db", ku_test_clock + 10000);
  if (default_timeout.conn || default_timeout.outcome != KU_PG_CONNECT_TIMED_OUT || ku_test_clock - before_default_timeout != 10000ULL) return 33;
  unsigned long long before_explicit_timeout = ku_test_clock;
  KuPgConnectAttempt bounded_timeout = ku_pg_connect_until("host=db", ku_test_clock + 2000);
  if (bounded_timeout.conn || bounded_timeout.outcome != KU_PG_CONNECT_TIMED_OUT || ku_test_clock - before_explicit_timeout != 2000ULL) return 34;
  __ku_handler_deadline = ku_test_clock + 3ULL;
  unsigned long long before_handler_timeout = ku_test_clock;
  KuResult_pg_client handler_timeout = ku_pg_client_open(conninfo, 1, 64, 5000, 5000, 30000);
  __ku_handler_deadline = 0;
  if (handler_timeout.ok || handler_timeout.value || ku_test_clock - before_handler_timeout != 3ULL || !ku_test_timeout_error(handler_timeout.error)) return 35;
  ku_error_drop(&handler_timeout.error);
  ku_test_mode = KU_TEST_OK;
  int starts_before_expired_handler = ku_test_start_calls;
  __ku_handler_deadline = ku_test_clock;
  KuResult_pg_client expired_handler = ku_pg_client_open(conninfo, 1, 64, 5000, 5000, 30000);
  __ku_handler_deadline = 0;
  if (expired_handler.ok || expired_handler.value || ku_test_start_calls != starts_before_expired_handler || !ku_test_timeout_error(expired_handler.error)) return 37;
  ku_error_drop(&expired_handler.error);

  int starts_before_zero = ku_test_start_calls;
  KuPgConnectAttempt zero = ku_pg_connect_until("host=db", 0);
  if (zero.conn || zero.outcome != KU_PG_CONNECT_TIMED_OUT || ku_test_start_calls != starts_before_zero) return 12;

  if (!ku_test_attempt(KU_TEST_FAILED, 50, KU_PG_CONNECT_FAILED)) return 13;
  if (!ku_test_attempt(KU_TEST_TIMEOUT, 5, KU_PG_CONNECT_TIMED_OUT)) return 14;
  if (!ku_test_attempt(KU_TEST_INVALID_SOCKET, 50, KU_PG_CONNECT_FAILED)) return 15;
  if (!ku_test_attempt(KU_TEST_START_OVERRUN, 5, KU_PG_CONNECT_TIMED_OUT)) return 16;
  if (!ku_test_attempt(KU_TEST_STATUS_BAD, 50, KU_PG_CONNECT_FAILED)) return 17;
  if (!ku_test_attempt(KU_TEST_ENCODING_BAD, 50, KU_PG_CONNECT_FAILED)) return 18;
  if (!ku_test_attempt(KU_TEST_UNKNOWN_POLL, 50, KU_PG_CONNECT_FAILED)) return 19;
  if (!ku_test_attempt(KU_TEST_POLL_ERROR, 50, KU_PG_CONNECT_FAILED)) return 20;
  if (!ku_test_attempt(KU_TEST_NULL_START, 50, KU_PG_CONNECT_OUT_OF_MEMORY)) return 21;

  ku_test_mode = KU_TEST_NULL_START;
  int starts_before_oom = ku_test_start_calls;
  int finishes_before_oom = ku_test_finish_calls;
  KuResult_pg_client start_oom = ku_pg_client_open(conninfo, 1, 64, 5000, 5000, 30000);
  if (start_oom.ok || start_oom.value || !ku_test_oom_error(start_oom.error)
      || ku_test_start_calls != starts_before_oom + 1
      || ku_test_finish_calls != finishes_before_oom
      || ku_test_live_connections != 0) return 38;
  ku_error_drop(&start_oom.error);

  int active_before = ku_test_active_waits;
  if (!ku_test_attempt(KU_TEST_ACTIVE_OK, 50, KU_PG_CONNECT_OK)) return 22;
  if (ku_test_active_waits != active_before + 1) return 23;
  if (!ku_test_attempt(KU_TEST_READ_WRITE_OK, 50, KU_PG_CONNECT_OK)) return 24;
  int interrupts_before = ku_test_interrupts;
  if (!ku_test_attempt(KU_TEST_INTERRUPTED_OK, 50, KU_PG_CONNECT_OK)) return 25;
  if (ku_test_interrupts != interrupts_before + 1) return 26;
  if (!ku_test_attempt(KU_TEST_HUP_RETRY_OK, 50, KU_PG_CONNECT_OK)) return 36;

  ku_test_mode = KU_TEST_OK;
  int pool_starts = ku_test_start_calls;
  KuResult_pg_client opened = ku_pg_client_open(conninfo, 2, 8, 5000, 5000, 30000);
  if (!opened.ok || !opened.value || ku_test_start_calls != pool_starts + 1) return 27;
  KuPgClient* pool = opened.value;
  opened.value = 0;
  KuString sql = ku_string_static((const uint8_t*)"SELECT 1", sizeof("SELECT 1") - 1);
  KuArray_str no_params = (KuArray_str){0};
  KuResult_pg_result query = ku_pg_client_query(pool, sql, no_params);
  /* The user query is followed by one bounded DISCARD ALL before pool reuse. */
  if (!query.ok || !query.value || ku_test_start_calls != pool_starts + 1 || ku_test_exec_calls != 2) return 28;
  ku_drop_pg_result(&query.value);

  PGconn* held = 0;
  KuError acquire_error = (KuError){0};
  int first_slot = ku_pg_client_acquire(pool, &held, &acquire_error, ~0ULL);
  if (first_slot < 0 || !held || ku_test_start_calls != pool_starts + 1) return 29;
  PGconn* lazy = 0;
  int second_slot = ku_pg_client_acquire(pool, &lazy, &acquire_error, ~0ULL);
  if (second_slot < 0 || !lazy || second_slot == first_slot || ku_test_start_calls != pool_starts + 2) return 30;
  ku_pg_client_release(pool, second_slot, 0, ~0ULL);
  ku_pg_client_release(pool, first_slot, 0, ~0ULL);
  ku_drop_pg_client(&pool);
  if (pool || ku_test_live_connections != 0) return 31;
  if (ku_test_encoding_set_calls != 0) return 32;

  puts("pg connect poll closed loop");
  return 0;
}
"#,
    );

    let harness_path = directory.path().join("pg-connect-poll-harness.c");
    fs::write(&harness_path, harness).expect("write PG poll C harness");
    let Some(executable) = compile_harness(directory.path(), &harness_path, "pg-connect-poll")
    else {
        return;
    };
    let mut command = Command::new(&executable);
    command.current_dir(directory.path());
    let output = run_bounded(&mut command, RUN_TIMEOUT, RUN_LIMITS)
        .unwrap_or_else(|error| panic!("PG poll harness did not complete safely: {error}"));
    assert!(
        output.status.success(),
        "PG poll harness failed with {:?}:\n{}{}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).replace('\r', ""),
        "pg connect poll closed loop\n"
    );
    assert!(
        !output
            .stdout
            .windows("fixture-secret".len())
            .any(|part| part == b"fixture-secret")
            && !output
                .stderr
                .windows("fixture-secret".len())
                .any(|part| part == b"fixture-secret"),
        "PG poll harness leaked conninfo secret"
    );
}
