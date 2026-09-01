//! Query I/O, deadline, poisoning and ownership checks without a database.

#[allow(dead_code)]
#[path = "support/native_pg_harness.rs"]
mod native_pg_harness;

use std::env;
use std::fs;
use std::path::PathBuf;
use std::process::Command;

use native_pg_harness::{
    compile_harness, compile_harness_with_libpq, emit_c, run_bounded, TempDir, RUN_LIMITS,
    RUN_TIMEOUT,
};

fn fixture() -> &'static str {
    r#"import pg from "std.pg"
fn HandleComparisons(): bool! {
    left = pg.client({ conninfo: "hostaddr=127.0.0.1", max_connections: 1 })?
    if (!(left == left)) panic("a native handle must equal itself")
    if (left == pg.client({ conninfo: "hostaddr=127.0.0.1", max_connections: 1 })?) {
        panic("distinct live connections compared equal")
    }
    if (!(left != pg.client({ conninfo: "hostaddr=127.0.0.1", max_connections: 1 })?)) {
        panic("distinct live connections must compare unequal")
    }
    left.close()
    return ok(true)
}
fn AggregateHandleComparisons(fail_array: bool, fail_result: bool): bool! {
    handles = [pg.client({ conninfo: "hostaddr=127.0.0.1", max_connections: 1 })?]
    opened = pg.client({ conninfo: "hostaddr=127.0.0.1", max_connections: 1 })
    box = { items: [0], scalar: 0 }
    if (fail_array) {
        rejected = handles == box["missing"]?
        panic("missing array comparison RHS did not fail")
    }
    if (handles == box["items"]?) panic("native array elements have no KuValue tag")
    if (!(handles != box["items"]?)) panic("native array inequality changed")
    if (box["items"]? == handles) panic("reversed native array comparison changed")
    if (fail_result) {
        rejected = opened == box["missing"]?
        panic("missing Result comparison RHS did not fail")
    }
    if (opened == box["scalar"]?) panic("native Result has no KuValue tag")
    if (!(opened != box["scalar"]?)) panic("native Result inequality changed")
    if (box["scalar"]? == opened) panic("reversed native Result comparison changed")
    if (handles.len() != 1) panic("comparison consumed the native array")
    client = opened?
    client.close()
    return ok(true)
}
fn NextRow(): int {
    return 0
}
fn main(): null! {
    client = pg.client({ conninfo: "hostaddr=127.0.0.1", max_connections: 1 })?
    result = client.query("SELECT $1::text", ["value"])?
    println(result.rows())
    println(result.value(NextRow(), 0)?)
    pooled = client.query("SELECT 1", [])?
    println(pooled.rows())
    client.close()
    return ok(null)
}
"#
}

#[test]
fn native_pg_handle_comparison_snapshots_do_not_clone_opaque_owners() {
    let directory = TempDir::new("handle-comparison-source");
    let generated = emit_c(directory.path(), fixture());
    let body = generated
        .split_once(" HandleComparisons() {\n")
        .expect("native handle comparison function")
        .1
        .split_once("\n}\n")
        .expect("native handle comparison function end")
        .0;
    assert!(generated.contains("ku_pg_client("));
    assert!(
        !body.contains("ku_clone_pg_client("),
        "an implicit binary snapshot must not call the forbidden native handle clone helper"
    );
}

#[test]
fn native_pg_owned_container_comparisons_do_not_clone_opaque_payloads() {
    let directory = TempDir::new("handle-container-comparison-source");
    let generated = emit_c(directory.path(), fixture());
    let body = generated
        .split_once(" AggregateHandleComparisons(bool fail_array, bool fail_result) {\n")
        .expect("native container comparison function")
        .1
        .split_once("\n}\n")
        .expect("native container comparison function end")
        .0;
    assert!(generated.contains("ku_pg_client("));
    for forbidden in [
        "ku_clone_pg_client(",
        "ku_array_clone_pg_client(",
        "ku_result_clone_pg_client(",
    ] {
        assert!(
            !body.contains(forbidden),
            "binary snapshots must not clone opaque payloads indirectly: {forbidden}"
        );
    }
}

#[test]
fn native_pg_queries_have_one_nonblocking_deadline_path() {
    let directory = TempDir::new("query-source");
    let generated = emit_c(directory.path(), fixture());
    for expected in [
        "extern int PQsetnonblocking(PGconn*, int)",
        "extern int PQsendQuery(PGconn*, const char*)",
        "extern int PQsendQueryParams",
        "extern int PQsetSingleRowMode(PGconn*)",
        "extern int PQfformat(const PGresult*, int)",
        "extern int PQflush(PGconn*)",
        "extern int PQconsumeInput(PGconn*)",
        "extern int PQisBusy(PGconn*)",
        "extern PGresult* PQgetResult(PGconn*)",
        "#define KU_PG_DEFAULT_QUERY_TIMEOUT_MS 30000ULL",
        "ku_pg_wait_socket_ready(PQsocket(conn), KU_PG_WAIT_READ | KU_PG_WAIT_WRITE, deadline)",
        "if ((ready & KU_PG_WAIT_READ) && (!PQconsumeInput(conn)",
        "ku_pg_wait_socket_ready(PQsocket(conn), KU_PG_WAIT_READ, deadline)",
        "if (PQisBusy(conn))",
        "PGresult* next = PQgetResult(conn)",
        "static void ku_pg_drop_query(KuPgQuery* query)",
        "typedef struct KuPgResult",
        "#define KU_PG_NULL_CELL UINT32_MAX",
        "SET client_encoding TO 'UTF8'",
        "shutdown((SOCKET)(uintptr_t)(unsigned int)fd, SD_BOTH)",
        "shutdown(fd, SHUT_RDWR)",
        "operation_deadline < deadline",
        "ku_pg_validate_query_params(params, &param_bytes, &param_error, deadline)",
        "ku_pg_result_append(&query.value, next, deadline)",
        "PostgreSQL query timed out; execution outcome may be unknown; close and reconnect",
        "PostgreSQL statement may have executed; outcome is unknown; never retry automatically; close and reconnect",
        "PostgreSQL statement completed but its result could not be delivered; never retry automatically",
        "DISCARD ALL",
        "ku_pg_client_cleanup_connection(p->conns[slot], broken, deadline)",
        "ku_pg_client_release(p, slot, broken || PQstatus(c) != KU_PG_CONNECTION_OK, deadline)",
    ] {
        assert!(
            generated.contains(expected),
            "missing PG query guard: {expected}"
        );
    }
    for forbidden in [
        "PQexec(",
        "PQexecParams(",
        "PQsetClientEncoding(",
        "PQerrorMessage(",
        "PQresultErrorMessage(",
        "ku_pg_copy_libpq_error(",
    ] {
        assert!(
            !generated.contains(forbidden),
            "unbounded/unsafe PG call: {forbidden}"
        );
    }
    assert!(
        !generated.contains("ku_clone_pg_client(client)"),
        "a PG query with an owned params literal must borrow, not clone, its client"
    );
    assert!(
        !generated.contains("ku_clone_pg_result(result)"),
        "a PG result method with an effectful index must borrow, not clone, its result"
    );
    let cleanup = generated
        .split_once("static int ku_pg_client_cleanup_connection(")
        .expect("client cleanup")
        .1
        .split_once("static void ku_pg_client_release(")
        .expect("client release")
        .0;
    assert!(cleanup.contains("return !ku_pg_connection_is_utf8(c)"));
    assert!(!cleanup.contains("ku_pg_ensure_utf8"));
    assert!(cleanup.contains("if (tx != KU_PQTRANS_IDLE) return 1"));
    assert!(cleanup.contains(
        "ku_pg_query_params_validated_impl(c, ku_string_static((const uint8_t*)\"DISCARD ALL\""
    ));
    assert!(cleanup.contains("reset_failed || ku_pg_deadline_expired(deadline)"));
    let acquire = generated
        .split_once("static int ku_pg_client_acquire(")
        .expect("client acquire")
        .1
        .split_once("static int ku_pg_client_cleanup_connection(")
        .expect("client cleanup")
        .0;
    assert!(acquire.contains("if (now >= deadline) { ku_pg_client_handoff_available_locked(p)"));
    assert!(acquire.contains("if (wait_result != 0) { ku_pg_client_handoff_available_locked(p)"));
    let handoff = generated
        .split_once("static void ku_pg_client_handoff_available_locked(")
        .expect("client waiter handoff")
        .1
        .split_once("static int ku_pg_client_acquire(")
        .expect("client acquire after handoff")
        .0;
    assert!(handoff.contains("p->waiters == 0"));
    assert!(handoff.contains("if (!p->in_use[i]) { ku_pg_cond_signal(&p->cv); return; }"));
    let poison = generated
        .split_once("static void ku_pg_break_connection(")
        .expect("connection poison")
        .1
        .split_once("static KuError ku_pg_query_connection_error(")
        .expect("poison end")
        .0;
    assert!(!poison.contains("PQfinish("));
    assert!(!poison.contains("closesocket("));
    assert!(!poison.contains("close("));
}

#[test]
fn native_pg_query_poller_stub_covers_backpressure_deadlines_and_owned_cleanup() {
    let directory = TempDir::new("query-harness");
    let generated = emit_c(directory.path(), fixture());
    let hook = r#"
#if defined(_WIN32)
static int ku_test_pg_poll(WSAPOLLFD*, ULONG, INT);
static int ku_test_pg_shutdown(SOCKET, int);
#define WSAPoll ku_test_pg_poll
#else
static int ku_test_pg_poll(struct pollfd*, nfds_t, int);
static int ku_test_pg_shutdown(int, int);
#define poll ku_test_pg_poll
#endif
#define shutdown ku_test_pg_shutdown
static unsigned long long ku_test_pg_now(void);
#define KU_PG_MONOTONIC_MS() ku_test_pg_now()
static void* ku_test_pg_malloc(size_t);
static void* ku_test_pg_calloc(size_t, size_t);
static void* ku_test_pg_realloc(void*, size_t);
static void ku_test_pg_free(void*);
#define malloc ku_test_pg_malloc
#define calloc ku_test_pg_calloc
#define realloc ku_test_pg_realloc
#define free ku_test_pg_free
"#;
    let mut harness = generated
        .replacen("#pragma comment(lib, \"libpq.lib\")\n", "", 1)
        .replacen(
            "typedef struct pg_conn PGconn;",
            &format!("{hook}\ntypedef struct pg_conn PGconn;"),
            1,
        )
        .replacen(
            "int main(void) {",
            "static int ku_generated_main(void) {",
            1,
        )
        .replacen(
            "#define KU_PG_MAX_RESULT_ROWS 1000000ULL",
            "#define KU_PG_MAX_RESULT_ROWS 4ULL",
            1,
        )
        .replacen(
            "#define KU_PG_MAX_RESULT_COLS 4096ULL",
            "#define KU_PG_MAX_RESULT_COLS 4ULL",
            1,
        )
        .replacen(
            "#define KU_PG_MAX_RESULT_CELLS 1000000ULL",
            "#define KU_PG_MAX_RESULT_CELLS 8ULL",
            1,
        )
        .replacen(
            "#define KU_PG_MAX_RESULT_BYTES (64ULL * 1024ULL * 1024ULL)",
            "#define KU_PG_MAX_RESULT_BYTES 16ULL",
            1,
        )
        .replace(
            "static void ku_pg_cond_signal(KuPgCond* cond) {",
            "static int ku_test_pg_signal_calls = 0;\nstatic void ku_pg_cond_signal(KuPgCond* cond) { ku_test_pg_signal_calls++;",
        );
    harness.push_str("\n#undef malloc\n#undef calloc\n#undef realloc\n#undef free\n");
    harness.push_str(QUERY_STUB);
    let source = directory.path().join("query-poll-harness.c");
    fs::write(&source, harness).expect("write query poll C harness");
    let Some(executable) = compile_harness(directory.path(), &source, "query-poll-harness") else {
        return;
    };
    let mut command = Command::new(executable);
    command.current_dir(directory.path());
    let output = run_bounded(&mut command, RUN_TIMEOUT, RUN_LIMITS)
        .unwrap_or_else(|error| panic!("query poll test exceeded process bounds: {error}"));
    assert!(
        output.status.success(),
        "query poll harness failed ({:?}):\n{}{}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).replace('\r', ""),
        "pg query poll closed loop\n"
    );
}

#[test]
#[ignore = "explicit opt-in: KU_PG_TEST_CONNINFO_FILE must parse as loopback before any connection"]
fn native_pg_query_poller_live_loopback_roundtrip() {
    let Some(connfile) = env::var_os("KU_PG_TEST_CONNINFO_FILE").map(PathBuf::from) else {
        eprintln!("skip: set KU_PG_TEST_CONNINFO_FILE to a local test connection file");
        return;
    };
    assert!(
        connfile.is_file(),
        "the opt-in PostgreSQL connection file is missing"
    );
    let mut discovery = Command::new("pg_config");
    discovery.args(["--libdir", "--bindir"]);
    let found = run_bounded(&mut discovery, RUN_TIMEOUT, RUN_LIMITS)
        .expect("bounded pg_config discovery for opt-in live test");
    assert!(found.status.success(), "pg_config discovery failed");
    let locations = String::from_utf8(found.stdout).expect("pg_config paths must be UTF-8");
    let mut locations = locations.lines();
    let libdir = PathBuf::from(locations.next().expect("pg_config libdir").trim());
    let bindir = PathBuf::from(locations.next().expect("pg_config bindir").trim());
    let directory = TempDir::new("query-live");
    let generated = emit_c(directory.path(), fixture());
    let mut harness = generated.replacen(
        "int main(void) {",
        "static int ku_generated_main(void) {",
        1,
    );
    harness.push_str(LIVE_LOOPBACK);
    let source = directory.path().join("query-live.c");
    fs::write(&source, harness).expect("write live PG harness without embedded credentials");
    let Some(executable) =
        compile_harness_with_libpq(directory.path(), &source, "query-live", &libdir)
    else {
        return;
    };
    let mut command = Command::new(executable);
    command
        .current_dir(directory.path())
        .env("KU_PG_TEST_CONNINFO_FILE", connfile);
    let mut search_path = vec![bindir];
    if let Some(existing) = env::var_os("PATH") {
        search_path.extend(env::split_paths(&existing));
    }
    command.env(
        "PATH",
        env::join_paths(search_path).expect("PostgreSQL runtime PATH"),
    );
    #[cfg(unix)]
    {
        let variable = if cfg!(target_os = "macos") {
            "DYLD_LIBRARY_PATH"
        } else {
            "LD_LIBRARY_PATH"
        };
        let mut libraries = vec![libdir];
        if let Some(existing) = env::var_os(variable) {
            libraries.extend(env::split_paths(&existing));
        }
        command.env(
            variable,
            env::join_paths(libraries).expect("PostgreSQL runtime libraries"),
        );
    }
    let output = run_bounded(&mut command, RUN_TIMEOUT, RUN_LIMITS)
        .expect("live PG test must not wait indefinitely");
    if output.status.code() == Some(77) {
        eprintln!("skip: connection file is not an explicit loopback configuration; no connection attempted");
        return;
    }
    assert!(
        output.status.success(),
        "live PG loopback harness failed ({:?}):\n{}{}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).replace('\r', ""),
        "pg query live loopback closed loop\n"
    );
}

const LIVE_LOOPBACK: &str = r#"
typedef struct { char* keyword; char* envvar; char* compiled; char* val; char* label; char* dispchar; int dispsize; } KuTestConninfoOption;
extern KuTestConninfoOption* PQconninfoParse(const char*, char**);
extern void PQconninfoFree(KuTestConninfoOption*);
extern void PQfreemem(void*);
static int ku_test_loopback_list(const char* value) {
  if (!value || !*value) return 0;
  const char* cursor = value;
  for (;;) {
    const char* end = strchr(cursor, ','); size_t length = end ? (size_t)(end - cursor) : strlen(cursor);
    if (!((length == 9 && memcmp(cursor, "127.0.0.1", 9) == 0)
        || (length == 9 && memcmp(cursor, "localhost", 9) == 0)
        || (length == 3 && memcmp(cursor, "::1", 3) == 0))) return 0;
    if (!end) return 1;
    cursor = end + 1;
  }
}
static int ku_test_conninfo_is_loopback(const char* conninfo) {
  char* error = 0;
  KuTestConninfoOption* options = PQconninfoParse(conninfo, &error);
  if (error) PQfreemem(error);
  if (!options) return 0;
  const char* host = 0; const char* hostaddr = 0; int service = 0;
  for (KuTestConninfoOption* option = options; option->keyword; option++) {
    if (strcmp(option->keyword, "host") == 0) host = option->val;
    else if (strcmp(option->keyword, "hostaddr") == 0) hostaddr = option->val;
    else if (strcmp(option->keyword, "service") == 0 && option->val && *option->val) service = 1;
  }
  const char* env_service = getenv("PGSERVICE");
  const char* env_addr = getenv("PGHOSTADDR");
  int local = !service && !(env_service && *env_service)
      && ((hostaddr && *hostaddr) ? ku_test_loopback_list(hostaddr)
          : (ku_test_loopback_list(host) && (!(env_addr && *env_addr) || ku_test_loopback_list(env_addr))));
  PQconninfoFree(options);
  return local;
}
static KuString ku_test_text(const char* value) { return ku_string_static((const uint8_t*)value, strlen(value)); }
static int ku_test_code(KuResult_pg_result result, const char* code) { size_t n = strlen(code); return !result.ok && !result.value && result.error.code.len == n && memcmp(result.error.code.ptr, code, n) == 0; }
static int ku_test_value(KuPgResult* result, const char* value) {
  if (!result || ku_pg_rows(result) != 1 || ku_pg_cols(result) != 1) return 0;
  KuResult_str read = ku_pg_value(result, 0, 0); size_t length = strlen(value);
  int equal = read.ok && read.value.len == length && (!length || memcmp(read.value.ptr, value, length) == 0);
  if (read.ok) ku_string_drop(&read.value); else ku_error_drop(&read.error); return equal;
}
#define LIVE_CHECK(value) do { if (!(value)) { fprintf(stderr, "live PG check failed at line %d\n", __LINE__); return 1; } } while (0)
int main(void) {
  const char* path = getenv("KU_PG_TEST_CONNINFO_FILE");
  LIVE_CHECK(path != 0);
  FILE* file = fopen(path, "rb"); LIVE_CHECK(file != 0);
  char conninfo[8193]; size_t size = fread(conninfo, 1, sizeof(conninfo) - 1, file);
  int read_failed = ferror(file); fclose(file);
  LIVE_CHECK(!read_failed && size < sizeof(conninfo) - 1); conninfo[size] = '\0';
  /* Parse only: no network call occurs until the destination is proven local. */
  if (strlen(conninfo) != size || !ku_test_conninfo_is_loopback(conninfo)) return 77;
  KuPgConnectAttempt raw_opened = ku_pg_connect_until(conninfo, __ku_handler_now_ms() + 2000);
  LIVE_CHECK(raw_opened.conn); PGconn* conn = raw_opened.conn;
  KuString value = ku_test_text("x'; SELECT pg_sleep(999); --"); KuArray_str params = { 1, &value };
  KuResult_pg_result result = ku_pg_query_params(conn, ku_test_text("SELECT $1::text"), params);
  LIVE_CHECK(result.ok && ku_test_value(result.value, "x'; SELECT pg_sleep(999); --")); ku_drop_pg_result(&result.value);
  result = ku_pg_query(conn, ku_test_text("SELECT 1; SELECT 2"));
  LIVE_CHECK(result.ok && ku_test_value(result.value, "2")); ku_drop_pg_result(&result.value);
  result = ku_pg_query(conn, ku_test_text("SELECT FROM generate_series(1, 3)"));
  LIVE_CHECK(result.ok && ku_pg_rows(result.value) == 3 && ku_pg_cols(result.value) == 0); ku_drop_pg_result(&result.value);
  result = ku_pg_query(conn, ku_test_text("SELECT 1 WHERE false"));
  LIVE_CHECK(result.ok && ku_pg_rows(result.value) == 0 && ku_pg_cols(result.value) == 1); ku_drop_pg_result(&result.value);
  result = ku_pg_query(conn, ku_test_text("SELECT NULL::text, ''::text, chr(20013) || chr(25991)"));
  LIVE_CHECK(result.ok && ku_pg_rows(result.value) == 1 && ku_pg_cols(result.value) == 3);
  KuResult_bool null_cell = ku_pg_is_null(result.value, 0, 0), empty_cell = ku_pg_is_null(result.value, 0, 1);
  LIVE_CHECK(null_cell.ok && null_cell.value && empty_cell.ok && !empty_cell.value);
  KuResult_str unicode = ku_pg_value(result.value, 0, 2); ku_drop_pg_result(&result.value);
  LIVE_CHECK(unicode.ok && unicode.value.len == 6 && memcmp(unicode.value.ptr, "\xe4\xb8\xad\xe6\x96\x87", 6) == 0); ku_string_drop(&unicode.value);
  result = ku_pg_query(conn, ku_test_text("SELECT 1 / (3 - value) FROM generate_series(1, 3) AS value"));
  LIVE_CHECK(ku_test_code(result, "query_error")); ku_error_drop(&result.error);
  result = ku_pg_query(conn, ku_test_text("BEGIN; DECLARE ku_binary BINARY CURSOR FOR SELECT 1::int4; FETCH ALL FROM ku_binary"));
  LIVE_CHECK(ku_test_code(result, "execution_completed_without_result")); ku_error_drop(&result.error);
  result = ku_pg_query(conn, ku_test_text("ROLLBACK")); LIVE_CHECK(result.ok); ku_drop_pg_result(&result.value);
  result = ku_pg_query(conn, ku_test_text("BEGIN; DECLARE ku_binary_empty BINARY CURSOR FOR SELECT 1::int4 WHERE false; FETCH ALL FROM ku_binary_empty"));
  LIVE_CHECK(ku_test_code(result, "execution_completed_without_result")); ku_error_drop(&result.error);
  result = ku_pg_query(conn, ku_test_text("ROLLBACK")); LIVE_CHECK(result.ok); ku_drop_pg_result(&result.value);
  result = ku_pg_query(conn, ku_test_text("BEGIN; DECLARE ku_binary_null BINARY CURSOR FOR SELECT NULL::int4; FETCH ALL FROM ku_binary_null"));
  LIVE_CHECK(ku_test_code(result, "execution_completed_without_result")); ku_error_drop(&result.error);
  result = ku_pg_query(conn, ku_test_text("ROLLBACK")); LIVE_CHECK(result.ok); ku_drop_pg_result(&result.value);
  result = ku_pg_query(conn, ku_test_text("BEGIN; DECLARE ku_binary_middle BINARY CURSOR FOR SELECT 1::int4; FETCH ALL FROM ku_binary_middle; SELECT 7"));
  LIVE_CHECK(result.ok && ku_test_value(result.value, "7")); ku_drop_pg_result(&result.value);
  result = ku_pg_query(conn, ku_test_text("ROLLBACK")); LIVE_CHECK(result.ok); ku_drop_pg_result(&result.value);
  result = ku_pg_query(conn, ku_test_text("BEGIN; DECLARE ku_binary_error BINARY CURSOR FOR SELECT 1 / (3 - value) FROM generate_series(1, 3) AS value; FETCH ALL FROM ku_binary_error"));
  LIVE_CHECK(ku_test_code(result, "query_error")); ku_error_drop(&result.error);
  result = ku_pg_query(conn, ku_test_text("ROLLBACK")); LIVE_CHECK(result.ok); ku_drop_pg_result(&result.value);
  result = ku_pg_query(conn, ku_test_text("BEGIN; DECLARE ku_binary_params BINARY CURSOR FOR SELECT 1::int4"));
  LIVE_CHECK(result.ok); ku_drop_pg_result(&result.value);
  KuArray_str no_params = {0}; result = ku_pg_query_params(conn, ku_test_text("FETCH ALL FROM ku_binary_params"), no_params);
  LIVE_CHECK(result.ok && ku_test_value(result.value, "1")); ku_drop_pg_result(&result.value);
  result = ku_pg_query(conn, ku_test_text("ROLLBACK")); LIVE_CHECK(result.ok); ku_drop_pg_result(&result.value);
  result = ku_pg_query(conn, ku_test_text("SELECT 1 / 0"));
  LIVE_CHECK(ku_test_code(result, "query_error")); ku_error_drop(&result.error);
  result = ku_pg_query(conn, ku_test_text("SELECT 3"));
  LIVE_CHECK(result.ok && ku_test_value(result.value, "3")); ku_drop_pg_result(&result.value);
  unsigned long long started = __ku_handler_now_ms(); __ku_handler_deadline = started + 100;
  result = ku_pg_query(conn, ku_test_text("SELECT pg_sleep(2)")); __ku_handler_deadline = 0;
  LIVE_CHECK(ku_test_code(result, "execution_unknown") && __ku_handler_now_ms() - started < 1500); ku_error_drop(&result.error);
  result = ku_pg_query(conn, ku_test_text("SELECT 4"));
  LIVE_CHECK(ku_test_code(result, "query_error")); ku_error_drop(&result.error); PQfinish(conn); conn = 0;
  LIVE_CHECK(!conn);
  KuResult_pg_client pooled = ku_pg_client_open(ku_test_text(conninfo), 1, 64, 5000, 5000, 30000);
  LIVE_CHECK(pooled.ok && pooled.value); KuPgClient* pool = pooled.value;
  KuArray_str empty_params = {0};
  started = __ku_handler_now_ms(); __ku_handler_deadline = started + 100;
  result = ku_pg_client_query(pool, ku_test_text("SELECT pg_sleep(2)"), empty_params); __ku_handler_deadline = 0;
  LIVE_CHECK(ku_test_code(result, "execution_unknown") && __ku_handler_now_ms() - started < 1500
      && !pool->conns[0] && !pool->in_use[0] && pool->active == 0); ku_error_drop(&result.error);
  result = ku_pg_client_query(pool, ku_test_text("SELECT 5"), empty_params);
  LIVE_CHECK(result.ok && ku_test_value(result.value, "5") && pool->conns[0] && !pool->in_use[0] && pool->active == 0); ku_drop_pg_result(&result.value);
  result = ku_pg_client_query(pool, ku_test_text("SELECT $1::text"), params);
  LIVE_CHECK(result.ok && ku_test_value(result.value, "x'; SELECT pg_sleep(999); --")); ku_drop_pg_result(&result.value);
  ku_drop_pg_client(&pool); LIVE_CHECK(!pool);
  puts("pg query live loopback closed loop");
  return 0;
}
"#;

const QUERY_STUB: &str = r#"
enum {
  T_OK, T_MULTI, T_ERROR, T_EMPTY, T_COPY, T_COPY_OUT, T_COPY_BOTH, T_BAD, T_BACKPRESSURE,
  T_READ_TIMEOUT, T_FLUSH_TIMEOUT, T_INTERRUPTED, T_HUP, T_POLL_ERROR,
  T_SEND_FAIL, T_FLUSH_FAIL, T_CONSUME_FAIL, T_SEND_OVERRUN,
  T_VALIDATION_OVERRUN, T_GET_OVERRUN, T_ENCODING, T_RESTORE, T_NO_RESULT, T_BAD_UTF8,
  T_ZERO_COLS, T_NULL_EMPTY_UTF8, T_NO_ROWS, T_LIMIT_INTERMEDIATE, T_LIMIT_FINAL,
  T_ROWS_LIMIT, T_COLS_LIMIT, T_CELLS_LIMIT, T_ZERO_COLS_LIMIT, T_INVALID_INTERMEDIATE,
  T_ERROR_AFTER_ROWS, T_ERROR_AFTER_INVALID, T_SINGLE_FAIL, T_PARTIAL_TIMEOUT,
  T_COLUMN_CHANGE, T_MISSING_TERMINAL, T_OOM_RESULT, T_OOM_CELLS, T_OOM_BYTES,
  T_ONLY_NULL_EMPTY, T_EXACT_BYTES, T_DISCARD_MANY, T_BAD_ROW_COUNT, T_BAD_TERMINAL_ROWS,
  T_BINARY, T_BINARY_ZERO_ROWS, T_BINARY_NULL, T_BINARY_INTERMEDIATE, T_BINARY_ERROR,
  T_BINARY_LATE, T_BINARY_TERMINAL, T_MIXED_FORMAT, T_UNKNOWN_FORMAT
};
struct pg_conn {
  int id, status, utf8, nonblocking, active, mode, index, total, pending_read;
  int received_reads, notice_consumed, interruption, shutdowns, single_row, mode_window;
  int is_reset, transaction_status, transaction_after_query;
};
struct pg_result { int status, mode, number; };
static PGconn* connections[128];
static unsigned long long clock_ms = 1000, clock_step = 0, test_deadline = 0;
static int starts = 0, finishes = 0, sends = 0, param_sends = 0, clears = 0;
static int live_results = 0, live_connections = 0, shutdowns = 0, bad_calls = 0;
static int both_waits = 0, read_waits = 0, get_gate = 0, restore_mode = T_RESTORE;
static int mode_on_send = T_OK, fail_nonblocking = 0, parameter_checks = 0;
static int reset_mode_on_send = T_RESTORE, reset_sends = 0, user_sends = 0;
static int next_user_transaction_status = KU_PQTRANS_IDLE;
static int expire_encoding_observation = 0, connect_elapsed = 0;
static int fail_connect_on_attempt = 0;
static int poisoned_consumes = 0;
static size_t fail_malloc_size = 0;
static int injected_allocation_failures = 0;
static int fail_runtime_allocation_after = 0, single_row_calls = 0, read_ahead_calls = 0;
static int track_runtime_allocations = 0, live_runtime_allocations = 0;
static int expire_oom_after_shutdown = 0;
static struct { void* pointer; size_t size; } runtime_allocations[16];
static size_t runtime_bytes = 0, runtime_peak_bytes = 0;
static unsigned long long last_wait_budget = 0;
#define CHECK(value) do { if (!(value)) { fprintf(stderr, "check failed at %d: %s\n", __LINE__, #value); return 1; } } while (0)
static int runtime_allocation_index(void* pointer) {
  if (pointer) for (int i = 0; i < 16; i++) if (runtime_allocations[i].pointer == pointer) return i;
  return -1;
}
static void record_runtime_allocation(void* pointer, size_t size) {
  if (!pointer || !track_runtime_allocations) return;
  for (int i = 0; i < 16; i++) if (!runtime_allocations[i].pointer) {
    runtime_allocations[i].pointer = pointer; runtime_allocations[i].size = size;
    live_runtime_allocations++; runtime_bytes += size;
    if (runtime_bytes > runtime_peak_bytes) runtime_peak_bytes = runtime_bytes;
    return;
  }
  bad_calls++;
}
static void* ku_test_pg_malloc(size_t size) {
  if (fail_runtime_allocation_after > 0 && --fail_runtime_allocation_after == 0) { injected_allocation_failures++; return 0; }
  if (fail_malloc_size && size == fail_malloc_size) { fail_malloc_size = 0; injected_allocation_failures++; return 0; }
  void* pointer = malloc(size); record_runtime_allocation(pointer, size); return pointer;
}
static void* ku_test_pg_calloc(size_t count, size_t size) {
  if (count != 0 && size > SIZE_MAX / count) return 0;
  size_t total = count * size; void* pointer = ku_test_pg_malloc(total);
  if (pointer) memset(pointer, 0, total); return pointer;
}
static void* ku_test_pg_realloc(void* pointer, size_t size) {
  if (fail_runtime_allocation_after > 0 && --fail_runtime_allocation_after == 0) { injected_allocation_failures++; return 0; }
  int index = runtime_allocation_index(pointer);
  void* replacement = realloc(pointer, size);
  if (replacement && index >= 0) {
    runtime_bytes -= runtime_allocations[index].size;
    runtime_allocations[index].pointer = replacement; runtime_allocations[index].size = size;
    runtime_bytes += size; if (runtime_bytes > runtime_peak_bytes) runtime_peak_bytes = runtime_bytes;
  } else if (replacement) record_runtime_allocation(replacement, size);
  return replacement;
}
static void ku_test_pg_free(void* pointer) {
  int index = runtime_allocation_index(pointer);
  if (index >= 0) {
    runtime_bytes -= runtime_allocations[index].size; live_runtime_allocations--;
    runtime_allocations[index].pointer = 0; runtime_allocations[index].size = 0;
  }
  free(pointer);
}

static unsigned long long ku_test_pg_now(void) { unsigned long long now = clock_ms; clock_ms += clock_step; return now; }
static PGconn* by_fd(uintptr_t fd) { return fd >= 10 && fd < 138 ? connections[fd - 10] : 0; }

#if defined(_WIN32)
static int ku_test_pg_shutdown(SOCKET fd, int direction) {
  if (direction != SD_BOTH) bad_calls++;
#else
static int ku_test_pg_shutdown(int fd, int direction) {
  if (direction != SHUT_RDWR) bad_calls++;
#endif
  PGconn* conn = by_fd((uintptr_t)fd);
  if (!conn) { bad_calls++; return -1; }
  /* OS shutdown does not itself mutate libpq's cached status. */
  conn->shutdowns++; shutdowns++;
  if (expire_oom_after_shutdown) { expire_oom_after_shutdown = 0; clock_ms = test_deadline - 1; clock_step = 1; }
  return 0;
}
#if defined(_WIN32)
static int ku_test_pg_poll(WSAPOLLFD* item, ULONG count, INT timeout_ms) {
  short read_event = POLLRDNORM, write_event = POLLWRNORM;
#else
static int ku_test_pg_poll(struct pollfd* item, nfds_t count, int timeout_ms) {
  short read_event = POLLIN, write_event = POLLOUT;
#endif
  PGconn* conn = item ? by_fd((uintptr_t)item->fd) : 0;
  if (!conn || count != 1 || timeout_ms < 0) { bad_calls++; return -1; }
  last_wait_budget = (unsigned long long)timeout_ms;
  if (!conn->active) { item->revents = item->events; return 1; }
  if ((item->events & read_event) && (item->events & write_event)) both_waits++;
  else if (item->events == read_event) read_waits++;
  else { bad_calls++; return -1; }
  if (conn->mode == T_READ_TIMEOUT || conn->mode == T_FLUSH_TIMEOUT || (conn->mode == T_PARTIAL_TIMEOUT && conn->index > 0)) {
    clock_ms += (unsigned long long)timeout_ms; return 0;
  }
  if (conn->mode == T_POLL_ERROR) {
#if defined(_WIN32)
    WSASetLastError(WSAEINVAL);
#else
    errno = EINVAL;
#endif
    return -1;
  }
  if (conn->mode == T_INTERRUPTED && !conn->interruption) {
    conn->interruption = 1; clock_ms++;
#if defined(_WIN32)
    WSASetLastError(WSAEINTR);
#else
    errno = EINTR;
#endif
    return -1;
  }
  if (conn->mode == T_BACKPRESSURE && item->events != (read_event | write_event)) { bad_calls++; return -1; }
  conn->pending_read = 1; clock_ms++;
  item->revents = conn->mode == T_HUP ? POLLHUP : read_event;
  return 1;
}
PGconn* PQconnectStartParams(const char* const* keys, const char* const* values, int expand) {
  (void)keys; (void)values; (void)expand;
  if (fail_connect_on_attempt > 0 && --fail_connect_on_attempt == 0) return 0;
  if (starts >= 127) { bad_calls++; return 0; }
  PGconn* conn = (PGconn*)calloc(1, sizeof(PGconn));
  if (!conn) abort();
  conn->id = ++starts; conn->utf8 = 1; connections[conn->id] = conn; live_connections++;
  return conn;
}
int PQconnectPoll(PGconn* conn) { (void)conn; clock_ms += (unsigned long long)connect_elapsed; return KU_PGRES_POLLING_OK; }
int PQsocket(const PGconn* conn) { return conn ? conn->id + 10 : -1; }
int PQstatus(const PGconn* conn) { return conn ? conn->status : KU_PG_CONNECTION_BAD; }
int PQtransactionStatus(const PGconn* conn) {
  return conn->active ? KU_PQTRANS_ACTIVE : conn->transaction_status;
}
const char* PQparameterStatus(const PGconn* conn, const char* parameter) {
  (void)parameter;
  if (!conn->utf8 && expire_encoding_observation) { clock_ms = test_deadline; expire_encoding_observation = 0; }
  return conn->utf8 ? "UTF8" : "LATIN1";
}
int PQsetnonblocking(PGconn* conn, int mode) { if (fail_nonblocking) return -1; conn->nonblocking = mode; return 0; }
void PQfinish(PGconn* conn) {
  if (!conn) return;
  /* Check identity before dereferencing so a double-drop is reported without
     the mock itself reading freed storage. */
  for (int id = 1; id < 128; id++) if (connections[id] == conn) {
    connections[id] = 0; live_connections--; finishes++; free(conn); return;
  }
  bad_calls++;
}
static int send_query(PGconn* conn, const char* sql) {
  if (!conn->nonblocking || conn->active || conn->status != KU_PG_CONNECTION_OK) { bad_calls++; return 0; }
  sends++;
  conn->is_reset = strcmp(sql, "DISCARD ALL") == 0;
  if (conn->is_reset) {
    reset_sends++;
    conn->mode = reset_mode_on_send;
  } else {
    user_sends++;
    conn->mode = strcmp(sql, "SET client_encoding TO 'UTF8'") == 0 ? restore_mode : mode_on_send;
    conn->transaction_after_query = next_user_transaction_status;
    next_user_transaction_status = KU_PQTRANS_IDLE;
  }
  conn->index = 0; conn->single_row = 0; conn->mode_window = 1;
  switch (conn->mode) {
    case T_DISCARD_MANY: conn->total = 43; break;
    case T_MULTI: case T_ROWS_LIMIT: case T_ZERO_COLS_LIMIT: conn->total = 6; break;
    case T_ZERO_COLS: case T_CELLS_LIMIT: case T_LIMIT_INTERMEDIATE: case T_LIMIT_FINAL: case T_INVALID_INTERMEDIATE: case T_BINARY_INTERMEDIATE: conn->total = 4; break;
    case T_NULL_EMPTY_UTF8: case T_COLUMN_CHANGE: case T_BINARY_LATE: conn->total = 3; break;
    case T_NO_RESULT: conn->total = 0; break;
    case T_EMPTY: case T_COPY: case T_COPY_OUT: case T_COPY_BOTH: case T_BAD: case T_ENCODING: case T_RESTORE: case T_NO_ROWS: case T_MISSING_TERMINAL: case T_BINARY_ZERO_ROWS: conn->total = 1; break;
    default: conn->total = 2; break;
  }
  conn->received_reads = 0; conn->notice_consumed = 0; conn->interruption = 0; conn->active = 1;
  if (conn->mode == T_SEND_OVERRUN) clock_ms = test_deadline;
  return conn->mode != T_SEND_FAIL;
}
int PQsendQuery(PGconn* conn, const char* sql) { return send_query(conn, sql); }
int PQsetSingleRowMode(PGconn* conn) {
  single_row_calls++;
  if (!conn->mode_window || !conn->active) { bad_calls++; return 0; }
  conn->mode_window = 0;
  if (conn->mode == T_SINGLE_FAIL) return 0;
  conn->single_row = 1; return 1;
}
int PQsendQueryParams(PGconn* conn, const char* sql, int count, const void* types, const char* const* values, const int* lengths, const int* formats, int format) {
  param_sends++;
  int valid_empty = count == 0 && !values;
  int valid_two = count == 2 && values && strcmp(sql, "SELECT $1::text, $2::text") == 0
      && strcmp(values[0], "x'; SELECT pg_sleep(999); --") == 0 && strcmp(values[1], "UTF8") == 0
      && values[1] == values[0] + strlen(values[0]) + 1;
  if (types || lengths || formats || format || (!valid_empty && !valid_two)) bad_calls++;
  else parameter_checks++;
  return send_query(conn, sql);
}
int PQflush(PGconn* conn) {
  conn->mode_window = 0;
  if (!conn->nonblocking) bad_calls++;
  if (conn->mode == T_FLUSH_FAIL) return -1;
  if (conn->mode == T_FLUSH_TIMEOUT || (conn->mode == T_BACKPRESSURE && !conn->notice_consumed)) return 1;
  return 0;
}
int PQconsumeInput(PGconn* conn) {
  conn->mode_window = 0;
  if (!conn->nonblocking) bad_calls++;
  if (conn->shutdowns) { poisoned_consumes++; conn->status = KU_PG_CONNECTION_BAD; return 0; }
  if (conn->status != KU_PG_CONNECTION_OK) return 0;
  if (!conn->active) return 1;
  /* All normal response packets are already buffered after the first row.
     Reading again before asking isBusy would model libpq input accumulation. */
  if (conn->index > 0 && conn->index < conn->total && conn->mode != T_PARTIAL_TIMEOUT) read_ahead_calls++;
  if (conn->mode == T_CONSUME_FAIL || (conn->mode == T_HUP && conn->pending_read)) return 0;
  if (conn->pending_read) { conn->pending_read = 0; conn->received_reads++; conn->notice_consumed = 1; }
  return 1;
}
int PQisBusy(PGconn* conn) {
  conn->mode_window = 0;
  int busy = conn->active && (conn->mode == T_READ_TIMEOUT || conn->mode == T_POLL_ERROR || conn->mode == T_HUP
      || conn->mode == T_CONSUME_FAIL
      || (conn->mode == T_INTERRUPTED && conn->received_reads < 2)
      || (conn->mode == T_PARTIAL_TIMEOUT && conn->index > 0));
  get_gate = !busy; return busy;
}
PGresult* PQgetResult(PGconn* conn) {
  if (!get_gate) { bad_calls++; return 0; }
  get_gate = 0;
  if (conn->index == conn->total) {
    conn->active = 0;
    if (conn->mode == T_ENCODING) conn->utf8 = 0;
    if (conn->mode == T_RESTORE) conn->utf8 = 1;
    if (conn->is_reset && conn->mode == T_RESTORE) conn->transaction_status = KU_PQTRANS_IDLE;
    else if (!conn->is_reset) conn->transaction_status = conn->transaction_after_query;
    return 0;
  }
  PGresult* result = (PGresult*)malloc(sizeof(PGresult));
  if (!result) abort();
  live_results++; result->mode = conn->mode; result->number = ++conn->index;
  result->status = result->number == conn->total ? KU_PGRES_TUPLES_OK : 9;
  if (conn->mode == T_MULTI || conn->mode == T_LIMIT_INTERMEDIATE || conn->mode == T_LIMIT_FINAL || conn->mode == T_INVALID_INTERMEDIATE || conn->mode == T_BINARY_INTERMEDIATE)
    result->status = (result->number & 1) ? 9 : KU_PGRES_TUPLES_OK;
  if (conn->mode == T_DISCARD_MANY) result->status = result->number <= 40 || result->number == 42 ? 9 : KU_PGRES_TUPLES_OK;
  if (conn->mode == T_ERROR) result->status = result->number == 1 ? KU_PGRES_COMMAND_OK : 7;
  else if ((conn->mode == T_ERROR_AFTER_ROWS || conn->mode == T_ERROR_AFTER_INVALID || conn->mode == T_BINARY_ERROR) && result->number == 2) result->status = 7;
  else if (conn->mode == T_MISSING_TERMINAL) result->status = 9;
  else if (conn->mode == T_EMPTY) result->status = KU_PGRES_EMPTY_QUERY;
  else if (conn->mode == T_COPY) result->status = KU_PGRES_COPY_IN;
  else if (conn->mode == T_COPY_OUT) result->status = KU_PGRES_COPY_OUT;
  else if (conn->mode == T_COPY_BOTH) result->status = KU_PGRES_COPY_BOTH;
  else if (conn->mode == T_BAD) result->status = KU_PGRES_BAD_RESPONSE;
  else if (conn->mode == T_ENCODING || conn->mode == T_RESTORE) result->status = KU_PGRES_COMMAND_OK;
  if (conn->mode == T_GET_OVERRUN && result->number == 2) clock_ms = test_deadline;
  if (result->number == 1 && conn->mode >= T_OOM_RESULT && conn->mode <= T_OOM_BYTES)
    fail_runtime_allocation_after = conn->mode - T_OOM_RESULT + 1;
  return result;
}
int PQresultStatus(const PGresult* result) { return result->status; }
char* PQresultErrorMessage(const PGresult* result) { (void)result; return "fixture reflected secret"; }
int PQntuples(const PGresult* result) {
  if (result->mode == T_BAD_ROW_COUNT && result->number == 1) return 0;
  if (result->mode == T_BAD_TERMINAL_ROWS && result->number == 2) return 1;
  return result->status == 9 ? 1 : 0;
}
int PQnfields(const PGresult* result) {
  if (result->status != 9 && result->status != KU_PGRES_TUPLES_OK) return 0;
  if (result->mode == T_ZERO_COLS || result->mode == T_ZERO_COLS_LIMIT) return 0;
  if (result->mode == T_NULL_EMPTY_UTF8 || result->mode == T_CELLS_LIMIT) return 3;
  if (result->mode == T_ONLY_NULL_EMPTY || result->mode == T_MIXED_FORMAT) return 2;
  if (result->mode == T_COLS_LIMIT) return 5;
  if (result->mode == T_COLUMN_CHANGE && result->number > 1) return 2;
  return 1;
}
static int result_format(const PGresult* result, int column) {
  if (result->mode == T_UNKNOWN_FORMAT) return 2;
  if (result->mode == T_MIXED_FORMAT) return column == 1;
  if (result->mode == T_BINARY_INTERMEDIATE) return result->number <= 2;
  if (result->mode == T_BINARY_LATE || result->mode == T_BINARY_TERMINAL) return result->number >= 2;
  return result->mode == T_BINARY || result->mode == T_BINARY_ZERO_ROWS || result->mode == T_BINARY_NULL || result->mode == T_BINARY_ERROR;
}
int PQfformat(const PGresult* result, int column) {
  if (column < 0 || column >= PQnfields(result)) { bad_calls++; return -1; }
  return result_format(result, column);
}
int PQgetisnull(const PGresult* result, int row, int column) {
  (void)row;
  return result->mode == T_BINARY_NULL || (result->mode == T_ONLY_NULL_EMPTY && column == 0)
      || (result->mode == T_NULL_EMPTY_UTF8 && ((result->number == 1 && column == 0) || (result->number == 2 && column == 2)));
}
char* PQgetvalue(const PGresult* result, int row, int column) {
  (void)row;
  if (result_format(result, column)) return "\0\0\0\1";
  if (result->mode == T_BAD_UTF8 || result->mode == T_ERROR_AFTER_INVALID || (result->mode == T_INVALID_INTERMEDIATE && result->number == 1)) return "\xff";
  if (result->mode == T_ONLY_NULL_EMPTY) return "";
  if (result->mode == T_EXACT_BYTES) return "0123456789abcdef";
  if (result->mode == T_DISCARD_MANY && result->number == 1) return "0123456789abcdefg";
  if ((result->mode == T_LIMIT_INTERMEDIATE && result->number == 1) || (result->mode == T_LIMIT_FINAL && result->number == 3)) return "0123456789abcdefg";
  if (result->mode == T_NULL_EMPTY_UTF8) return column == 1 ? "" : (column == 2 ? "\xe4\xb8\xad\xe6\x96\x87" : "v");
  if (result->mode == T_MULTI) return result->number == 1 ? "1" : (result->number == 3 ? "2" : "3");
  if (result->mode == T_ROWS_LIMIT || result->mode == T_CELLS_LIMIT) return "x";
  return "last";
}
int PQgetlength(const PGresult* result, int row, int column) {
  if (result->mode == T_DISCARD_MANY && result->number > 1 && result->number <= 40) bad_calls++;
  if (result->mode == T_VALIDATION_OVERRUN) clock_ms = test_deadline;
  if (result_format(result, column)) return 4;
  return (int)strlen(PQgetvalue(result, row, column));
}
void PQclear(PGresult* result) { if (result) { live_results--; clears++; free(result); } }
static KuString text(const char* value) { return ku_string_static((const uint8_t*)value, strlen(value)); }
static int equals(KuString value, const char* expected) { size_t length = strlen(expected); return value.len == length && (!length || memcmp(value.ptr, expected, length) == 0); }
static int error_is(KuResult_pg_result result, const char* code) { return !result.ok && !result.value && equals(result.error.domain, "pg") && equals(result.error.code, code); }
static KuObject* pg_config_with_conninfo(KuValue conninfo) {
  KuObject* config = ku_object_new(0);
  ku_object_set(config, text("conninfo"), conninfo);
  return config;
}
static PGconn* open_connection(void) {
  KuPgConnectAttempt result = ku_pg_connect_until("hostaddr=127.0.0.1", clock_ms + 10000);
  if (!result.conn) abort();
  return result.conn;
}
static void close_connection(PGconn** connection) { if (connection && *connection) { PQfinish(*connection); *connection = 0; } }

int main(void) {
  int comparison_starts = starts, comparison_finishes = finishes;
  KuResult_bool comparison = HandleComparisons();
  CHECK(comparison.ok && comparison.value);
  CHECK(starts == comparison_starts + 3);
  CHECK(finishes == comparison_finishes + 3);
  CHECK(!live_connections && !bad_calls);
  CHECK(!live_runtime_allocations && !runtime_bytes);
  ku_error_drop(&comparison.error);
  comparison_starts = starts; comparison_finishes = finishes;
  fail_connect_on_attempt = 2;
  comparison = HandleComparisons();
  CHECK(!comparison.ok && equals(comparison.error.domain, "pg") && equals(comparison.error.code, "connect_error")
      && !fail_connect_on_attempt && starts == comparison_starts + 1
      && finishes == comparison_finishes + 1 && !live_connections && !bad_calls
      && !live_runtime_allocations && !runtime_bytes);
  ku_error_drop(&comparison.error);

  /* Exercise real generated Ku owners, not only helper text: both containers
     must remain usable and each underlying connection must finish once. Config
     object allocations are generic runtime state, so this PG-only ledger stays
     disabled for generated constructor expressions. */
  for (int failure = 0; failure < 3; failure++) {
    comparison_starts = starts; comparison_finishes = finishes;
    comparison = AggregateHandleComparisons(failure == 1, failure == 2);
    if (!failure) CHECK(comparison.ok && comparison.value);
    else CHECK(!comparison.ok && equals(comparison.error.domain, "object") && equals(comparison.error.code, "missing_key"));
    CHECK(starts == comparison_starts + 2 && finishes == comparison_finishes + 2
        && !live_connections && !live_results && !bad_calls && !live_runtime_allocations && !runtime_bytes);
    ku_error_drop(&comparison.error);
  }

  PGconn* conn = open_connection();
  int broken = 99, before_sends = sends, before_clears = clears;
  mode_on_send = T_MULTI; track_runtime_allocations = 1;
  KuResult_pg_result result = ku_pg_query_impl(conn, text("SELECT 1; SELECT 2; SELECT 3"), clock_ms + 50, &broken);
  track_runtime_allocations = 0;
  CHECK(result.ok && ku_pg_rows(result.value) == 1 && broken == 0 && sends == before_sends + 1 && clears == before_clears + 6 && !conn->active);
  CHECK(result.value->cell_capacity <= KU_PG_MAX_RESULT_CELLS && result.value->bytes_capacity <= KU_PG_MAX_RESULT_BYTES
      && runtime_peak_bytes <= sizeof(KuPgResult) + KU_PG_MAX_RESULT_CELLS * sizeof(KuPgCell) + KU_PG_MAX_RESULT_BYTES + sizeof("SELECT 1; SELECT 2; SELECT 3"));
  KuResult_str first_read = ku_pg_value(result.value, 0, 0); CHECK(first_read.ok && equals(first_read.value, "3")); ku_string_drop(&first_read.value);
  /* A fallible value copy must not terminate the process or consume the table.
     Keep allocation failure deterministic: this is not a host-memory stress test. */
  KuPgCell* original_cells = result.value->cells; uint8_t* original_bytes = result.value->bytes;
  int retained_allocations = live_runtime_allocations, before_value_failures = injected_allocation_failures;
  size_t retained_bytes = runtime_bytes;
  fail_runtime_allocation_after = 1;
  KuResult_str failed_value = ku_pg_value(result.value, 0, 0);
  CHECK(!failed_value.ok && !failed_value.value.ptr && !failed_value.value.len
      && equals(failed_value.error.domain, "pg") && equals(failed_value.error.code, "out_of_memory")
      && failed_value.error.domain.storage == KU_STRING_STATIC
      && failed_value.error.code.storage == KU_STRING_STATIC
      && failed_value.error.message.storage == KU_STRING_STATIC
      && !fail_runtime_allocation_after && injected_allocation_failures == before_value_failures + 1);
  ku_error_drop(&failed_value.error);
  CHECK(ku_pg_rows(result.value) == 1 && ku_pg_cols(result.value) == 1
      && result.value->cells == original_cells && result.value->bytes == original_bytes
      && live_runtime_allocations == retained_allocations && runtime_bytes == retained_bytes);
  KuResult_str recovered_value = ku_pg_value(result.value, 0, 0);
  CHECK(recovered_value.ok && equals(recovered_value.value, "3"));
  ku_drop_pg_result(&result.value); CHECK(live_results == 0 && live_runtime_allocations == 0 && runtime_bytes == 0);
  CHECK(equals(recovered_value.value, "3")); ku_string_drop(&recovered_value.value);

  mode_on_send = T_ZERO_COLS; result = ku_pg_query(conn, text("SELECT FROM generate_series(1, 3)"));
  CHECK(result.ok && ku_pg_rows(result.value) == 3 && ku_pg_cols(result.value) == 0);
  KuResult_str missing = ku_pg_value(result.value, 0, 0); CHECK(!missing.ok); ku_error_drop(&missing.error);
  ku_drop_pg_result(&result.value);
  mode_on_send = T_NO_ROWS; result = ku_pg_query(conn, text("SELECT 1 WHERE false"));
  CHECK(result.ok && ku_pg_rows(result.value) == 0 && ku_pg_cols(result.value) == 1); ku_drop_pg_result(&result.value);

  mode_on_send = T_NULL_EMPTY_UTF8; result = ku_pg_query(conn, text("SELECT NULL, '', 'unicode'"));
  CHECK(result.ok && ku_pg_rows(result.value) == 2 && ku_pg_cols(result.value) == 3);
  KuResult_bool is_null = ku_pg_is_null(result.value, 0, 0); CHECK(is_null.ok && is_null.value);
  is_null = ku_pg_is_null(result.value, 0, 1); CHECK(is_null.ok && !is_null.value);
  KuResult_str copied_read = ku_pg_value(result.value, 0, 2); CHECK(copied_read.ok && equals(copied_read.value, "\xe4\xb8\xad\xe6\x96\x87")); KuString copied = copied_read.value;
  KuResult_str empty = ku_pg_value(result.value, 0, 1); CHECK(empty.ok && empty.value.len == 0); ku_string_drop(&empty.value);
  fail_runtime_allocation_after = 1;
  KuResult_str null_copy = ku_pg_value(result.value, 0, 0);
  KuResult_str empty_copy = ku_pg_value(result.value, 0, 1);
  CHECK(!null_copy.ok && error_is((KuResult_pg_result){ false, 0, null_copy.error }, "null_value")
      && empty_copy.ok && !empty_copy.value.ptr && !empty_copy.value.len && fail_runtime_allocation_after == 1);
  ku_error_drop(&null_copy.error); ku_string_drop(&empty_copy.value);
  fail_runtime_allocation_after = 0;
  ku_drop_pg_result(&result.value); CHECK(equals(copied, "\xe4\xb8\xad\xe6\x96\x87")); ku_string_drop(&copied);
  mode_on_send = T_ONLY_NULL_EMPTY; result = ku_pg_query(conn, text("SELECT NULL, ''"));
  CHECK(result.ok && ku_pg_rows(result.value) == 1 && ku_pg_cols(result.value) == 2 && !result.value->bytes && !result.value->bytes_len && !result.value->bytes_capacity);
  empty = ku_pg_value(result.value, 0, 0); CHECK(!empty.ok && equals(empty.error.code, "null_value")); ku_error_drop(&empty.error);
  empty = ku_pg_value(result.value, 0, 1); CHECK(empty.ok && !empty.value.ptr && !empty.value.len); ku_string_drop(&empty.value);
  is_null = ku_pg_is_null(result.value, 0, 0); CHECK(is_null.ok && is_null.value);
  is_null = ku_pg_is_null(result.value, 0, 1); CHECK(is_null.ok && !is_null.value); ku_drop_pg_result(&result.value);
  mode_on_send = T_EXACT_BYTES; result = ku_pg_query(conn, text("SELECT exact_limit"));
  CHECK(result.ok && result.value->bytes_len == KU_PG_MAX_RESULT_BYTES && result.value->bytes_capacity <= KU_PG_MAX_RESULT_BYTES); ku_drop_pg_result(&result.value);
  const int binary_modes[] = { T_BINARY, T_BINARY_ZERO_ROWS, T_BINARY_NULL, T_BINARY_LATE, T_BINARY_TERMINAL, T_MIXED_FORMAT, T_UNKNOWN_FORMAT };
  for (size_t i = 0; i < sizeof(binary_modes) / sizeof(binary_modes[0]); i++) {
    mode_on_send = binary_modes[i]; track_runtime_allocations = 1; result = ku_pg_query(conn, text("FETCH binary_cursor")); track_runtime_allocations = 0;
    CHECK(error_is(result, "execution_completed_without_result") && !conn->active && !conn->shutdowns && !live_results && !live_runtime_allocations && !runtime_bytes); ku_error_drop(&result.error);
  }
  int limited_modes[] = { T_LIMIT_FINAL, T_ROWS_LIMIT, T_COLS_LIMIT, T_CELLS_LIMIT, T_ZERO_COLS_LIMIT };
  for (size_t i = 0; i < sizeof(limited_modes) / sizeof(limited_modes[0]); i++) {
    mode_on_send = limited_modes[i]; track_runtime_allocations = 1; result = ku_pg_query(conn, text("SELECT bounded")); track_runtime_allocations = 0;
    CHECK(error_is(result, "execution_completed_without_result") && !conn->active && !conn->shutdowns && live_results == 0 && !live_runtime_allocations && !runtime_bytes); ku_error_drop(&result.error);
  }
  int discarded_modes[] = { T_LIMIT_INTERMEDIATE, T_INVALID_INTERMEDIATE, T_DISCARD_MANY, T_BINARY_INTERMEDIATE };
  for (size_t i = 0; i < sizeof(discarded_modes) / sizeof(discarded_modes[0]); i++) {
    mode_on_send = discarded_modes[i]; result = ku_pg_query(conn, text("SELECT discarded; SELECT 'last'"));
    CHECK(result.ok && ku_pg_rows(result.value) == 1 && !conn->shutdowns);
    copied_read = ku_pg_value(result.value, 0, 0); CHECK(copied_read.ok && equals(copied_read.value, "last")); ku_string_drop(&copied_read.value); ku_drop_pg_result(&result.value);
  }
  int partial_error_modes[] = { T_ERROR_AFTER_ROWS, T_ERROR_AFTER_INVALID, T_BINARY_ERROR };
  for (size_t i = 0; i < sizeof(partial_error_modes) / sizeof(partial_error_modes[0]); i++) {
    mode_on_send = partial_error_modes[i]; result = ku_pg_query(conn, text("SELECT may_fail"));
    CHECK(error_is(result, "query_error") && equals(result.error.message, "PostgreSQL query failed")
        && !equals(result.error.message, "fixture reflected secret")
        && !conn->active && !conn->shutdowns && live_results == 0); ku_error_drop(&result.error);
  }

  mode_on_send = T_ERROR;
  result = ku_pg_query(conn, text("SELECT 1; invalid SQL; SELECT 3"));
  CHECK(error_is(result, "query_error") && equals(result.error.message, "PostgreSQL query failed")
      && !equals(result.error.message, "fixture reflected secret") && !conn->active && !conn->shutdowns);
  ku_error_drop(&result.error); CHECK(live_results == 0);
  mode_on_send = T_EMPTY; result = ku_pg_query(conn, text(""));
  CHECK(error_is(result, "query_error") && !conn->shutdowns); ku_error_drop(&result.error);
  mode_on_send = T_NO_RESULT; before_sends = sends;
  result = ku_pg_query(conn, text("SELECT 1"));
  CHECK(error_is(result, "execution_unknown") && conn->shutdowns && sends == before_sends + 1); ku_error_drop(&result.error);
  mode_on_send = T_OK; before_sends = sends;
  result = ku_pg_query(conn, text("SELECT 2"));
  CHECK(error_is(result, "query_error") && sends == before_sends); ku_error_drop(&result.error);
  close_connection(&conn); conn = open_connection();
  mode_on_send = T_BAD_UTF8; result = ku_pg_query(conn, text("SELECT 1"));
  CHECK(error_is(result, "execution_completed_without_result") && !conn->shutdowns); ku_error_drop(&result.error);

  mode_on_send = T_OK;
  KuString params_data[] = { text("x'; SELECT pg_sleep(999); --"), text("UTF8") };
  KuArray_str params = { 2, params_data };
  result = ku_pg_query_params(conn, text("SELECT $1::text, $2::text"), params);
  CHECK(result.ok && param_sends == 1 && parameter_checks == 1); ku_drop_pg_result(&result.value);
  before_sends = sends;
  uint8_t nul_bytes[] = { 's', 0, 'q' };
  KuString nul = { nul_bytes, 3, 3, KU_STRING_STATIC };
  result = ku_pg_query(conn, nul);
  CHECK(error_is(result, "query_error") && sends == before_sends && !conn->shutdowns); ku_error_drop(&result.error);
  KuString oversized_sql = { (uint8_t*)"x", (size_t)KU_PG_MAX_SQL_BYTES + 1, 0, KU_STRING_STATIC };
  result = ku_pg_query(conn, oversized_sql);
  CHECK(error_is(result, "query_too_large") && sends == before_sends && !conn->shutdowns); ku_error_drop(&result.error);

  __ku_handler_deadline = clock_ms; before_sends = sends;
  result = ku_pg_query(conn, text("SELECT 1"));
  CHECK(error_is(result, "query_timeout") && sends == before_sends && !conn->shutdowns); ku_error_drop(&result.error);
  __ku_handler_timed_out = 1; __ku_handler_unwind_depth = 1; __ku_handler_cleanup_deadline = clock_ms + 1000;
  result = ku_pg_query(conn, text("SELECT 1"));
  CHECK(error_is(result, "query_timeout") && sends == before_sends && !conn->shutdowns); ku_error_drop(&result.error);
  __ku_handler_deadline = 0; __ku_handler_timed_out = 0; __ku_handler_unwind_depth = 0; __ku_handler_cleanup_deadline = 0;
  clock_step = 1;
  result = ku_pg_query_params_impl(conn, text("SELECT $1::text, $2::text"), params, clock_ms + 2, &broken);
  clock_step = 0;
  CHECK(error_is(result, "query_timeout") && sends == before_sends && !conn->shutdowns && !broken); ku_error_drop(&result.error);

  mode_on_send = T_BACKPRESSURE;
  result = ku_pg_query(conn, text("SELECT 1"));
  CHECK(result.ok && both_waits == 1 && conn->notice_consumed); ku_drop_pg_result(&result.value);
  mode_on_send = T_INTERRUPTED;
  result = ku_pg_query_impl(conn, text("SELECT 1"), clock_ms + 20, &broken);
  CHECK(result.ok && conn->interruption && conn->received_reads == 2 && read_waits >= 3); ku_drop_pg_result(&result.value);

  mode_on_send = T_ENCODING; restore_mode = T_RESTORE; before_sends = sends;
  result = ku_pg_query(conn, text("SET client_encoding TO LATIN1"));
  CHECK(error_is(result, "execution_completed_without_result") && conn->utf8 && sends == before_sends + 2 && !conn->shutdowns); ku_error_drop(&result.error);
  mode_on_send = T_OK; result = ku_pg_query(conn, text("SELECT 1"));
  CHECK(result.ok); ku_drop_pg_result(&result.value); close_connection(&conn);

  const int failures[] = { T_READ_TIMEOUT, T_FLUSH_TIMEOUT, T_HUP, T_POLL_ERROR, T_SEND_FAIL, T_FLUSH_FAIL, T_CONSUME_FAIL, T_SEND_OVERRUN, T_VALIDATION_OVERRUN, T_COPY, T_COPY_OUT, T_COPY_BOTH, T_BAD, T_SINGLE_FAIL, T_PARTIAL_TIMEOUT, T_COLUMN_CHANGE, T_MISSING_TERMINAL, T_BAD_ROW_COUNT };
  for (size_t i = 0; i < sizeof(failures) / sizeof(failures[0]); i++) {
    conn = open_connection(); mode_on_send = failures[i]; test_deadline = clock_ms + 7;
    before_sends = sends; int before_finishes = finishes;
    track_runtime_allocations = 1; result = ku_pg_query_impl(conn, text("SELECT 1"), test_deadline, &broken); track_runtime_allocations = 0;
    CHECK(error_is(result, "execution_unknown") && broken && conn->shutdowns && live_results == 0 && sends == before_sends + 1 && finishes == before_finishes && !live_runtime_allocations && !runtime_bytes);
    ku_error_drop(&result.error); mode_on_send = T_OK; before_sends = sends;
    result = ku_pg_query(conn, text("SELECT 2"));
    CHECK(error_is(result, "query_error") && sends == before_sends); ku_error_drop(&result.error);
    close_connection(&conn); CHECK(!conn && finishes == before_finishes + 1);
  }
  const int completed_failures[] = { T_GET_OVERRUN, T_BAD_TERMINAL_ROWS };
  for (size_t i = 0; i < sizeof(completed_failures) / sizeof(completed_failures[0]); i++) {
    conn = open_connection(); mode_on_send = completed_failures[i]; test_deadline = clock_ms + 7;
    before_sends = sends; int before_finishes = finishes;
    track_runtime_allocations = 1; result = ku_pg_query_impl(conn, text("SELECT 1"), test_deadline, &broken); track_runtime_allocations = 0;
    CHECK(error_is(result, "execution_completed_without_result") && broken && conn->shutdowns && live_results == 0 && sends == before_sends + 1 && finishes == before_finishes && !live_runtime_allocations && !runtime_bytes);
    ku_error_drop(&result.error); mode_on_send = T_OK; before_sends = sends;
    result = ku_pg_query(conn, text("SELECT 2"));
    CHECK(error_is(result, "query_error") && sends == before_sends); ku_error_drop(&result.error);
    close_connection(&conn); CHECK(!conn && finishes == before_finishes + 1);
  }
  const int allocation_modes[] = { T_OOM_RESULT, T_OOM_CELLS, T_OOM_BYTES };
  for (size_t i = 0; i < sizeof(allocation_modes) / sizeof(allocation_modes[0]); i++) {
    conn = open_connection(); mode_on_send = allocation_modes[i]; before_sends = sends;
    int before_alloc_failures = injected_allocation_failures;
    track_runtime_allocations = 1; result = ku_pg_query(conn, text("SELECT 'last'")); track_runtime_allocations = 0;
    CHECK(error_is(result, "execution_unknown") && conn->shutdowns && live_results == 0
        && sends == before_sends + 1 && !fail_runtime_allocation_after
        && injected_allocation_failures == before_alloc_failures + 1 && !live_runtime_allocations && !runtime_bytes);
    ku_error_drop(&result.error); mode_on_send = T_OK; before_sends = sends;
    result = ku_pg_query(conn, text("SELECT 2"));
    CHECK(error_is(result, "query_error") && sends == before_sends); ku_error_drop(&result.error); close_connection(&conn);
  }
  conn = open_connection(); mode_on_send = T_OOM_RESULT; test_deadline = clock_ms + 50;
  expire_oom_after_shutdown = 1; track_runtime_allocations = 1;
  result = ku_pg_query_impl(conn, text("SELECT 'last'"), test_deadline, &broken);
  clock_step = 0; track_runtime_allocations = 0;
  CHECK(error_is(result, "execution_unknown") && broken && conn->shutdowns && !live_results && !live_runtime_allocations && !runtime_bytes);
  ku_error_drop(&result.error); close_connection(&conn);

  conn = open_connection(); mode_on_send = T_READ_TIMEOUT;
  unsigned long long before_clock = clock_ms;
  result = ku_pg_query(conn, text("SELECT 1"));
  CHECK(error_is(result, "execution_unknown") && clock_ms - before_clock == 30000);
  ku_error_drop(&result.error); close_connection(&conn);

  conn = open_connection(); fail_nonblocking = 1; mode_on_send = T_OK; before_sends = sends;
  result = ku_pg_query(conn, text("SELECT 1")); fail_nonblocking = 0;
  CHECK(error_is(result, "query_error") && sends == before_sends && conn->shutdowns); ku_error_drop(&result.error); close_connection(&conn);

  conn = open_connection(); mode_on_send = T_ENCODING; restore_mode = T_READ_TIMEOUT; before_sends = sends;
  test_deadline = clock_ms + 5;
  result = ku_pg_query_impl(conn, text("SET client_encoding TO LATIN1"), test_deadline, &broken);
  CHECK(error_is(result, "execution_completed_without_result") && broken && conn->shutdowns && sends == before_sends + 2 && clock_ms == test_deadline && live_results == 0);
  ku_error_drop(&result.error); close_connection(&conn); restore_mode = T_RESTORE;

  conn = open_connection(); mode_on_send = T_ENCODING; restore_mode = T_ERROR; before_sends = sends;
  result = ku_pg_query(conn, text("SET client_encoding TO LATIN1"));
  CHECK(error_is(result, "execution_completed_without_result") && conn->shutdowns && sends == before_sends + 2 && live_results == 0);
  ku_error_drop(&result.error); close_connection(&conn); restore_mode = T_RESTORE;

  conn = open_connection(); mode_on_send = T_ENCODING; expire_encoding_observation = 1; test_deadline = clock_ms + 5; before_sends = sends;
  result = ku_pg_query_impl(conn, text("SET client_encoding TO LATIN1"), test_deadline, &broken);
  CHECK(error_is(result, "execution_completed_without_result") && broken && conn->shutdowns && sends == before_sends + 1 && live_results == 0);
  ku_error_drop(&result.error); mode_on_send = T_OK; before_sends = sends;
  result = ku_pg_query(conn, text("SELECT 1"));
  CHECK(error_is(result, "query_error") && sends == before_sends); ku_error_drop(&result.error); close_connection(&conn);

  int before_invalid_config_starts = starts;
  KuResult_pg_client invalid_client = ku_pg_client(0);
  CHECK(!invalid_client.ok && !invalid_client.value && equals(invalid_client.error.code, "invalid_config"));
  ku_error_drop(&invalid_client.error);
  KuObject* invalid_config = pg_config_with_conninfo(ku_v_int(1));
  invalid_client = ku_pg_client(invalid_config);
  CHECK(!invalid_client.ok && !invalid_client.value && equals(invalid_client.error.code, "invalid_config"));
  ku_error_drop(&invalid_client.error); ku_object_drop(invalid_config);
  invalid_config = pg_config_with_conninfo(ku_v_str(text("hostaddr=127.0.0.1")));
  ku_object_set(invalid_config, text("unknown"), ku_v_int(1));
  invalid_client = ku_pg_client(invalid_config);
  CHECK(!invalid_client.ok && !invalid_client.value && equals(invalid_client.error.code, "invalid_config"));
  ku_error_drop(&invalid_client.error); ku_object_drop(invalid_config);
  invalid_config = pg_config_with_conninfo(ku_v_str(text("hostaddr=127.0.0.1")));
  ku_object_set(invalid_config, text("max_connections"), ku_v_int(0));
  invalid_client = ku_pg_client(invalid_config);
  CHECK(!invalid_client.ok && !invalid_client.value && equals(invalid_client.error.code, "invalid_config"));
  ku_error_drop(&invalid_client.error); ku_object_drop(invalid_config);
  uint8_t embedded_nul[] = { 'h', 'o', 's', 't', 0, 'x' };
  invalid_config = pg_config_with_conninfo(ku_v_str((KuString){ embedded_nul, sizeof(embedded_nul), 0, KU_STRING_STATIC }));
  invalid_client = ku_pg_client(invalid_config);
  CHECK(!invalid_client.ok && !invalid_client.value && equals(invalid_client.error.code, "invalid_config")
      && invalid_client.error.domain.storage == KU_STRING_STATIC && invalid_client.error.code.storage == KU_STRING_STATIC
      && invalid_client.error.message.storage == KU_STRING_STATIC && starts == before_invalid_config_starts);
  ku_error_drop(&invalid_client.error); ku_object_drop(invalid_config);
  KuString oversized_conninfo = { (uint8_t*)"x", (size_t)KU_PG_MAX_CONNINFO_BYTES + 1, 0, KU_STRING_STATIC };
  invalid_client = ku_pg_client_open(oversized_conninfo, 1, 64, 5000, 5000, 30000);
  CHECK(!invalid_client.ok && !invalid_client.value && equals(invalid_client.error.code, "invalid_config")
      && starts == before_invalid_config_starts);
  ku_error_drop(&invalid_client.error);

  for (int allocation = 1; allocation <= 4; allocation++) {
    int before_client_failures = injected_allocation_failures, before_client_starts = starts;
    fail_runtime_allocation_after = allocation; track_runtime_allocations = 1;
    KuResult_pg_client failed_client = ku_pg_client_open(text("hostaddr=127.0.0.1"), 1, 64, 5000, 5000, 30000);
    track_runtime_allocations = 0;
    CHECK(!failed_client.ok && !failed_client.value && equals(failed_client.error.domain, "pg")
        && equals(failed_client.error.code, "out_of_memory")
        && failed_client.error.domain.storage == KU_STRING_STATIC
        && failed_client.error.code.storage == KU_STRING_STATIC
        && failed_client.error.message.storage == KU_STRING_STATIC
        && injected_allocation_failures == before_client_failures + 1
        && starts == before_client_starts && !live_runtime_allocations && !runtime_bytes);
    ku_error_drop(&failed_client.error);
  }

  KuResult_pg_client opened = ku_pg_client_open(text("hostaddr=127.0.0.1"), 1, 64, 5000, 5000, 30000);
  CHECK(opened.ok); KuPgClient* pool = opened.value; KuArray_str empty_params = {0};
  int before_handoff_signals = ku_test_pg_signal_calls;
  pool->waiters = 1;
  ku_pg_client_handoff_available_locked(pool);
  CHECK(ku_test_pg_signal_calls == before_handoff_signals + 1);
  pool->waiters = 0;
  PGconn* held = 0; KuError held_error = {0};
  int held_slot = ku_pg_client_acquire(pool, &held, &held_error, clock_ms + 50);
  CHECK(held_slot == 0 && held && pool->active == 1 && pool->waiters == 0);
  pool->max_waiters = 0; PGconn* rejected = 0; KuError rejected_error = {0};
  CHECK(ku_pg_client_acquire(pool, &rejected, &rejected_error, clock_ms + 50) < 0
      && !rejected && equals(rejected_error.code, "too_many_waiters")
      && pool->active == 1 && pool->waiters == 0);
  ku_error_drop(&rejected_error); pool->max_waiters = 64;
  int before_resets = reset_sends;
  unsigned long long release_deadline = clock_ms + 50;
  ku_pg_client_release(pool, held_slot, 0, release_deadline);
  CHECK(pool->active == 0 && pool->waiters == 0 && !pool->in_use[0]
      && pool->conns[0] == held && reset_sends == before_resets + 1);

  /* A reset failure evicts only the pooled connection. The already detached
     user result remains readable and carries no server-provided error text. */
  reset_mode_on_send = T_ERROR; mode_on_send = T_OK; before_resets = reset_sends;
  result = ku_pg_client_query(pool, text("SELECT reset_failure"), empty_params);
  CHECK(result.ok && ku_pg_rows(result.value) == 1 && reset_sends == before_resets + 1
      && pool->active == 0 && !pool->in_use[0] && !pool->conns[0]);
  KuResult_str detached_after_reset_failure = ku_pg_value(result.value, 0, 0);
  CHECK(detached_after_reset_failure.ok && equals(detached_after_reset_failure.value, "last"));
  ku_drop_pg_result(&result.value);
  CHECK(equals(detached_after_reset_failure.value, "last"));
  ku_string_drop(&detached_after_reset_failure.value);

  /* Successful reuse always performs DISCARD ALL. */
  reset_mode_on_send = T_RESTORE; int before_starts = starts; before_resets = reset_sends;
  result = ku_pg_client_query(pool, text("SELECT reset_success"), empty_params);
  CHECK(result.ok && starts == before_starts + 1 && reset_sends == before_resets + 1
      && pool->active == 0 && pool->conns[0]);
  ku_drop_pg_result(&result.value);

  /* A transaction/session that is not IDLE is never handed to another user;
     cleanup must evict it without attempting a potentially blocking reset. */
  next_user_transaction_status = 2; before_resets = reset_sends;
  result = ku_pg_client_query(pool, text("SELECT leaves_transaction_open"), empty_params);
  CHECK(result.ok && reset_sends == before_resets && !pool->conns[0] && pool->active == 0);
  KuResult_str detached_non_idle = ku_pg_value(result.value, 0, 0);
  CHECK(detached_non_idle.ok && equals(detached_non_idle.value, "last"));
  ku_drop_pg_result(&result.value); ku_string_drop(&detached_non_idle.value);

  /* Reset uses the original query deadline, not a fresh timeout. A timeout in
     hidden cleanup evicts the slot but does not consume the detached result. */
  reset_mode_on_send = T_READ_TIMEOUT; pool->query_timeout_ms = 7;
  unsigned long long operation_started = clock_ms; before_resets = reset_sends;
  result = ku_pg_client_query(pool, text("SELECT reset_deadline"), empty_params);
  CHECK(result.ok && reset_sends == before_resets + 1 && clock_ms == operation_started + 7
      && !pool->conns[0] && pool->active == 0);
  KuResult_str detached_after_reset_timeout = ku_pg_value(result.value, 0, 0);
  CHECK(detached_after_reset_timeout.ok && equals(detached_after_reset_timeout.value, "last"));
  ku_drop_pg_result(&result.value); ku_string_drop(&detached_after_reset_timeout.value);
  reset_mode_on_send = T_RESTORE; pool->query_timeout_ms = 30000;

  mode_on_send = T_READ_TIMEOUT; __ku_handler_deadline = clock_ms + 7; before_sends = sends;
  result = ku_pg_client_query(pool, text("SELECT 1"), empty_params); __ku_handler_deadline = 0;
  CHECK(error_is(result, "execution_unknown") && sends == before_sends + 1 && pool->active == 0 && !pool->in_use[0] && !pool->conns[0]); ku_error_drop(&result.error);
  mode_on_send = T_OK; before_starts = starts;
  result = ku_pg_client_query(pool, text("SELECT 2"), empty_params);
  CHECK(result.ok && starts == before_starts + 1 && pool->active == 0 && pool->conns[0]); ku_drop_pg_result(&result.value);
  PGconn* unused = 0; KuError acquire_error = {0};
  CHECK(ku_pg_client_acquire(pool, &unused, &acquire_error, clock_ms) < 0 && !unused && pool->active == 0);
  ku_error_drop(&acquire_error);

  mode_on_send = T_READ_TIMEOUT; __ku_handler_deadline = clock_ms + 7;
  result = ku_pg_client_query(pool, text("SELECT 1"), empty_params); __ku_handler_deadline = 0;
  CHECK(error_is(result, "execution_unknown") && !pool->conns[0]); ku_error_drop(&result.error);
  connect_elapsed = 3; __ku_handler_deadline = clock_ms + 7;
  result = ku_pg_client_query(pool, text("SELECT 1"), empty_params); __ku_handler_deadline = 0; connect_elapsed = 0;
  CHECK(error_is(result, "execution_unknown") && last_wait_budget == 4 && !pool->conns[0] && pool->active == 0); ku_error_drop(&result.error);
  ku_drop_pg_client(&pool);
  CHECK(!pool && live_connections == 0 && live_results == 0 && starts == finishes);
  CHECK(bad_calls == 0);
  CHECK(shutdowns > 0 && poisoned_consumes >= 2 && single_row_calls > 0 && read_ahead_calls == 0);
  CHECK(!live_runtime_allocations && !runtime_bytes);
  puts("pg query poll closed loop");
  return 0;
}
"#;
