//! Query I/O, deadline, poisoning and ownership checks without a database.

#[allow(dead_code)]
#[path = "support/native_pg_harness.rs"]
mod native_pg_harness;

use std::env;
use std::fs;
use std::path::PathBuf;
use std::process::Command;

use native_pg_harness::{
    compile_harness, compile_harness_with_libpq, emit_c, run_bounded, TempDir,
    NATIVE_THREAD_LIFECYCLE_HARNESS, RUN_LIMITS, RUN_TIMEOUT,
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
fn ReadTyped(): int! {
    client = pg.client({ conninfo: "hostaddr=127.0.0.1", max_connections: 1 })?
    result = client.query("SELECT 3", [])?
    client.close()
    integer = result.value_int(NextRow(), NextRow())?
    original = result.value(0, 0)?
    if (original != "3") { panic("integer read consumed the original table") }
    again = result.value_int(0, 0)?
    return ok(integer + again)
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
fn native_pg_integer_receiver_checks_arity_types_and_result_propagation() {
    ku::cli::check_source("pg-integer.ku", fixture()).expect("typed getter public receiver checks");
    for invalid in [
        "result.value_int(0)",
        "result.value_int(0, 0, 0)",
        "result.value_int(\"zero\", 0)",
        "result.value_int(0, false)",
    ] {
        let source = fixture().replacen("result.value_int(NextRow(), NextRow())", invalid, 1);
        ku::cli::check_source("pg-integer-invalid.ku", &source)
            .expect_err("getter rejects invalid arguments");
    }
    let missing_propagation = fixture().replacen(
        "return ok(integer + again)",
        "return ok(result.value_int(0, 0) + again)",
        1,
    );
    ku::cli::check_source("pg-integer-result.ku", &missing_propagation)
        .expect_err("integer getter remains a Result until explicitly unwrapped");
}

#[test]
fn native_pg_handle_comparison_snapshots_do_not_clone_opaque_owners() {
    let directory = TempDir::new("handle-comparison-source");
    let generated = emit_c(directory.path(), fixture());
    let body = generated
        .split_once(" HandleComparisons(void) {\n")
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
fn native_pg_posix_sync_destroy_failures_stop_before_pool_free() {
    let directory = TempDir::new("sync-destroy");
    let generated = emit_c(directory.path(), fixture());
    let signature = "static void ku_pg_sync_destroy(KuPgMutex* mutex, KuPgCond* cond) {";
    let destroy_start = generated
        .rfind(signature)
        .expect("generated POSIX PG synchronization destructor");
    let destroy_suffix = &generated[destroy_start..];
    let destroy_end = destroy_suffix
        .find("\n}\n")
        .expect("generated POSIX PG synchronization destructor end")
        + 3;
    let destroy = &destroy_suffix[..destroy_end];

    let condition_call = destroy
        .find("pthread_cond_destroy(cond)")
        .expect("PG condition destructor call");
    let condition_failure = destroy
        .find("pg client condition destroy failed")
        .expect("PG condition destruction failure guard");
    let mutex_call = destroy
        .find("pthread_mutex_destroy(mutex)")
        .expect("PG mutex destructor call");
    let mutex_failure = destroy
        .find("pg client mutex destroy failed")
        .expect("PG mutex destruction failure guard");
    assert!(
        condition_call < condition_failure
            && condition_failure < mutex_call
            && mutex_call < mutex_failure,
        "PG must validate condition destruction before attempting mutex destruction"
    );
    let client_dispose = generated
        .split_once("static void ku_pg_client_dispose(KuPgClient* p) {")
        .expect("generated PG client disposer")
        .1
        .split_once("static void ku_pg_client_close_owned(KuPgClient* p)")
        .expect("generated PG client disposer end")
        .0;
    let sync_destroy = client_dispose
        .find("ku_pg_sync_destroy(&p->lock, &p->cv)")
        .expect("PG synchronization destruction before pool free");
    let first_pool_free = client_dispose
        .find("free(p->conns)")
        .expect("PG connection-array free");
    let owner_free = client_dispose.rfind("free(p)").expect("PG pool owner free");
    assert!(
        sync_destroy < first_pool_free && first_pool_free < owner_free,
        "PG synchronization must be destroyed before freeing enclosing pool allocations"
    );

    let harness = directory.path().join("pg_sync_destroy_harness.c");
    fs::write(
        &harness,
        format!(
            r#"#include <stdio.h>
#include <stdlib.h>
#include <string.h>
typedef int KuPgMutex;
typedef int KuPgCond;
static int ku_test_condition_result = 0;
static int ku_test_mutex_result = 0;
static int pthread_cond_destroy(int* condition) {{
  (void)condition;
  fputs("condition\n", stdout);
  return ku_test_condition_result;
}}
static int pthread_mutex_destroy(int* mutex) {{
  (void)mutex;
  fputs("mutex\n", stdout);
  return ku_test_mutex_result;
}}
{destroy}
int main(int argc, char** argv) {{
  if (argc != 2) return 64;
  if (strcmp(argv[1], "condition") == 0) ku_test_condition_result = 1;
  else if (strcmp(argv[1], "mutex") == 0) ku_test_mutex_result = 1;
  else if (strcmp(argv[1], "ok") != 0) return 65;
  KuPgMutex mutex = 0;
  KuPgCond condition = 0;
  ku_pg_sync_destroy(&mutex, &condition);
  fputs("free\n", stdout);
  return 0;
}}
"#
        ),
    )
    .expect("write PG synchronization destruction harness");

    let Some(executable) = compile_harness(directory.path(), &harness, "pg-sync-destroy") else {
        eprintln!("skip: no C compiler available for PG synchronization destruction test");
        return;
    };
    for (case, success, stdout, stderr) in [
        (
            "condition",
            false,
            "condition\n",
            "pg client condition destroy failed\n",
        ),
        (
            "mutex",
            false,
            "condition\nmutex\n",
            "pg client mutex destroy failed\n",
        ),
        ("ok", true, "condition\nmutex\nfree\n", ""),
    ] {
        let mut command = Command::new(&executable);
        command.current_dir(directory.path()).arg(case);
        let output = run_bounded(&mut command, RUN_TIMEOUT, RUN_LIMITS).unwrap_or_else(|error| {
            panic!("PG synchronization destruction case {case} was not bounded: {error}")
        });
        assert_eq!(
            output.status.success(),
            success,
            "unexpected PG synchronization destruction status for {case}"
        );
        let actual_stdout = String::from_utf8_lossy(&output.stdout).replace('\r', "");
        let actual_stderr = String::from_utf8_lossy(&output.stderr).replace('\r', "");
        assert_eq!(actual_stdout, stdout);
        assert_eq!(actual_stderr, stderr);
        if !success {
            assert_eq!(output.status.code(), Some(1));
            assert!(
                !actual_stdout.contains("free\n"),
                "PG synchronization destruction failure continued to pool free"
            );
        }
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
        "ku_pg_validate_sql_input(sql, &sql_error, deadline)",
        "ku_pg_sql_has_explicit_session_control(sql, deadline)",
        "next_deadline_check = SIZE_MAX - index < 4096",
        "ku_pg_result_append(&query.value, next, deadline)",
        "PostgreSQL query budget expired before the requested statement was sent",
        "PostgreSQL statement may have executed; outcome is unknown; never retry automatically; close and reconnect",
        "PostgreSQL statement completed but its result could not be delivered; never retry automatically",
        "PostgreSQL statement was not sent because explicit transaction or session-control SQL is unsupported by the pooled client",
        "PostgreSQL statement completed or may have completed; session state is unsupported and its payload was discarded; never retry automatically",
        "DISCARD ALL",
        "ku_pg_client_cleanup_connection(connection, broken, deadline)",
        "ku_pg_client_release(p, slot, broken || PQstatus(c) != KU_PG_CONNECTION_OK, deadline)",
        "ku_pg_client_record_connect_failure_locked",
        "!p->connect_in_flight && now >= p->reconnect_not_before_ms",
        "p->reconnect_not_before_ms > now && !p->backoff_timer_armed",
        "ku_pg_client_release_backoff_timer_locked(p, owns_backoff_timer)",
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
        "ku_mysql_sql_next_top_token(",
        "ku_sql_session_state_unsupported(",
    ] {
        assert!(
            !generated.contains(forbidden),
            "unbounded/unsafe PG call: {forbidden}"
        );
    }
    let client_constructor = generated
        .split_once("static KuResult_pg_client ku_pg_client(KuObject* config) {")
        .expect("PG client constructor")
        .1
        .split_once("static void ku_pg_client_handoff_available_locked(")
        .expect("PG client constructor end")
        .0;
    assert!(
        !client_constructor.contains("malloc(")
            && !client_constructor.contains("calloc(")
            && !client_constructor.contains("realloc(")
            && !client_constructor.contains("PQ"),
        "pg.client(config) must finish all object-shape validation without allocation or libpq access"
    );
    let client_open = generated
        .split_once("static KuResult_pg_client ku_pg_client_open(")
        .expect("PG client open")
        .1
        .split_once("static KuResult_pg_client ku_pg_client(KuObject* config)")
        .expect("PG client open end")
        .0;
    let range_validation = client_open
        .find("size < 1 || size > 256")
        .expect("PG client numeric validation");
    let conninfo_validation = client_open
        .find("conninfo.len == SIZE_MAX")
        .expect("PG client conninfo validation");
    let first_allocation = client_open.find("calloc(").expect("PG client allocation");
    let first_connection = client_open
        .find("ku_pg_connect_until(")
        .expect("PG initial connection");
    assert!(
        range_validation < first_allocation
            && conninfo_validation < first_allocation
            && first_allocation < first_connection,
        "PG client configuration must be fully rejected before pool allocation and libpq connection work"
    );
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
    let release = generated
        .split_once("static void ku_pg_client_release(")
        .expect("client release")
        .1
        .split_once("static KuResult_pg_result ku_pg_client_query(")
        .expect("client query after release")
        .0;
    assert!(release.contains("int cleanup_allowed = !p->closing"));
    assert!(release.contains("if (!cleanup_allowed) broken = 1"));
    let detach = release.find("PGconn* discard = 0").expect("detached close");
    let unlock = release[detach..]
        .find("ku_pg_mutex_unlock(&p->lock)")
        .map(|offset| detach + offset)
        .expect("unlock after detach");
    let finish = release
        .find("PQfinish(discard)")
        .expect("finish detached connection");
    assert!(
        unlock < finish,
        "PQfinish must run after releasing the pool mutex"
    );
    let acquire = generated
        .split_once("static int ku_pg_client_acquire(")
        .expect("client acquire")
        .1
        .split_once("static int ku_pg_client_cleanup_connection(")
        .expect("client cleanup")
        .0;
    assert!(acquire.contains("if (now >= deadline) { ku_pg_diagnostics_sync_acquire_locked(p, started, deadline, operation_deadline, 0, -1, now, NULL); ku_pg_client_handoff_available_locked(p)"));
    assert!(acquire.contains("if (wait_result != 0) { now = ku_pg_now_ms()"));
    assert!(acquire.contains(
        "if (wait_result == 1 && wait_deadline < deadline && now < deadline) { has_waited = 1; continue; } if (wait_result == 1) ku_pg_diagnostics_sync_acquire_locked(p, started, deadline, operation_deadline, 0, -1, now, NULL); ku_pg_client_handoff_available_locked(p)"
    ));
    let handoff = generated
        .split_once("static void ku_pg_client_handoff_available_locked(")
        .expect("client waiter handoff")
        .1
        .split_once("static int ku_pg_client_acquire(")
        .expect("client acquire after handoff")
        .0;
    assert!(handoff.contains("p->waiters == 0"));
    assert!(handoff.contains(
        "if (p->conns[i] && !p->in_use[i]) { ku_pg_client_signal_locked(p,0); return; }"
    ));
    assert!(handoff.contains(
        "if (p->connect_in_flight || ku_pg_now_ms() < p->reconnect_not_before_ms) return;"
    ));
    assert!(handoff.contains(
        "if (!p->conns[i] && !p->in_use[i]) { ku_pg_client_signal_locked(p,0); return; }"
    ));
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
fn native_pg_real_threads_close_wakes_waiter_and_defers_one_dispose() {
    let directory = TempDir::new("pool-thread-lifecycle");
    let generated = emit_c(directory.path(), fixture());
    let hook = format!(
        r#"
{NATIVE_THREAD_LIFECYCLE_HARNESS}
#if defined(_WIN32)
static int ku_test_pg_poll(WSAPOLLFD*, ULONG, INT);
static int ku_test_pg_shutdown(SOCKET, int);
#define WSAPoll ku_test_pg_poll
static volatile LONG ku_test_pg_dispose_calls = 0;
static void ku_test_pg_record_dispose(void) {{
  InterlockedIncrement(&ku_test_pg_dispose_calls);
}}
static long ku_test_pg_load_dispose_calls(void) {{
  return InterlockedCompareExchange(&ku_test_pg_dispose_calls, 0, 0);
}}
#else
static int ku_test_pg_poll(struct pollfd*, nfds_t, int);
static int ku_test_pg_shutdown(int, int);
#define poll ku_test_pg_poll
static _Atomic int ku_test_pg_dispose_calls = 0;
static void ku_test_pg_record_dispose(void) {{
  atomic_fetch_add_explicit(&ku_test_pg_dispose_calls, 1, memory_order_relaxed);
}}
static int ku_test_pg_load_dispose_calls(void) {{
  return atomic_load_explicit(&ku_test_pg_dispose_calls, memory_order_relaxed);
}}
#endif
static KuTestEvent ku_test_pg_dispose_entered;
static KuTestEvent ku_test_pg_dispose_release;
static int ku_test_pg_block_dispose = 0;
static void ku_test_pg_pause_dispose(void) {{
  if (!ku_test_pg_block_dispose) return;
  if (!ku_test_event_set(&ku_test_pg_dispose_entered)
      || !ku_test_event_wait(&ku_test_pg_dispose_release, 5000)) {{
    fputs("PG disposer barrier failed\n", stderr);
    abort();
  }}
}}
#define shutdown ku_test_pg_shutdown
static unsigned long long ku_test_pg_now(void);
#define KU_PG_MONOTONIC_MS() ku_test_pg_now()
static int ku_test_pg_pool_lock_depth = 0;
static int ku_test_pg_finishes_while_pool_locked = 0;
static int ku_test_pg_acquire_calls = 0;
"#
    );
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
        )
        .replacen(
            "static void ku_pg_client_dispose(KuPgClient* p) {",
            "static void ku_pg_client_dispose(KuPgClient* p) { ku_test_pg_record_dispose(); ku_test_pg_pause_dispose();",
            1,
        );
    let (query_stub, _) = QUERY_STUB
        .split_once("\nint main(void) {\n")
        .expect("PG fake libpq definitions before sequential harness main");
    harness.push_str(query_stub);
    harness.push_str(PG_THREAD_LIFECYCLE_MAIN);
    let source = directory.path().join("pg-pool-thread-lifecycle.c");
    fs::write(&source, harness).expect("write PG real-thread lifecycle harness");
    let Some(executable) = compile_harness(directory.path(), &source, "pg-pool-thread-lifecycle")
    else {
        eprintln!("skip: no C compiler available for PG real-thread lifecycle test");
        return;
    };
    let mut command = Command::new(executable);
    command.current_dir(directory.path());
    let output = run_bounded(&mut command, RUN_TIMEOUT, RUN_LIMITS)
        .unwrap_or_else(|error| panic!("PG real-thread lifecycle test was not bounded: {error}"));
    assert!(
        output.status.success(),
        "PG real-thread lifecycle harness failed ({:?}):\n{}{}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).replace('\r', ""),
        "pg real-thread lifecycle closed loop\n"
    );
    assert!(output.stderr.is_empty());
}

fn query_stub_harness(directory: &TempDir) -> String {
    query_stub_harness_source(directory, fixture())
}

fn query_stub_harness_source(directory: &TempDir, source: &str) -> String {
    let generated = emit_c(directory.path(), source);
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
static int ku_test_pg_pool_lock_depth = 0;
static int ku_test_pg_finishes_while_pool_locked = 0;
static int ku_test_pg_acquire_calls = 0;
static int ku_test_pg_index_calls = 0;
#define malloc ku_test_pg_malloc
#define calloc ku_test_pg_calloc
#define realloc ku_test_pg_realloc
#define free ku_test_pg_free
"#;
    let mut harness = generated
        .replacen("int64_t NextRow(void) {", "int64_t NextRow(void) { ku_test_pg_index_calls++;", 1)
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
        )
        .replace(
            "static void ku_pg_mutex_lock(KuPgMutex* mutex) {",
            "static void ku_pg_mutex_lock(KuPgMutex* mutex) { ku_test_pg_pool_lock_depth++;",
        )
        .replace(
            "static void ku_pg_mutex_unlock(KuPgMutex* mutex) {",
            "static void ku_pg_mutex_unlock(KuPgMutex* mutex) { ku_test_pg_pool_lock_depth--;",
        )
        .replace(
            "static int ku_pg_cond_wait_ms(KuPgCond* cond, KuPgMutex* mutex, unsigned long long timeout_ms) {",
            "static int ku_test_pg_wait_failure = 0;\nstatic int ku_pg_cond_wait_ms(KuPgCond* cond, KuPgMutex* mutex, unsigned long long timeout_ms) { if (ku_test_pg_wait_failure) { ku_test_pg_wait_failure = 0; return -1; }",
        )
        .replace(
            "static int ku_pg_client_acquire(KuPgClient* p, PGconn** out, KuError* err, unsigned long long operation_deadline) {",
            "static int ku_pg_client_acquire(KuPgClient* p, PGconn** out, KuError* err, unsigned long long operation_deadline) { ku_test_pg_acquire_calls++;",
        );
    harness.push_str("\n#undef malloc\n#undef calloc\n#undef realloc\n#undef free\n");
    harness.push_str(QUERY_STUB);
    harness
}

#[test]
fn native_pg_query_poller_stub_covers_backpressure_deadlines_and_owned_cleanup() {
    let directory = TempDir::new("query-harness");
    let harness = query_stub_harness(&directory);
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
fn native_pg_async_pump_suspends_preserves_owned_results_and_drains_all_waiter_pins() {
    let directory = TempDir::new("async-query-pump");
    let harness = query_stub_harness(&directory).replacen(
        "int main(void) {",
        "static int ku_legacy_query_tests(void) {",
        1,
    ) + ASYNC_QUERY_SUITE;
    let source = directory.path().join("async-query-pump.c");
    fs::write(&source, harness).expect("write resumable PG harness");
    let Some(executable) = compile_harness(directory.path(), &source, "async-query-pump") else {
        return;
    };
    let output = run_bounded(&mut Command::new(executable), RUN_TIMEOUT, RUN_LIMITS)
        .unwrap_or_else(|error| panic!("resumable PG test exceeded process bounds: {error}"));
    assert!(
        output.status.success(),
        "resumable PG harness failed ({:?}):\n{}{}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).replace('\r', ""),
        "pg async pump closed loop\n"
    );
}

#[test]
fn native_pg_async_input_owner_prepays_aligned_storage_and_releases_failures() {
    let directory = TempDir::new("async-pg-input-owner");
    let harness = query_stub_harness(&directory)
        .replacen(
            "int main(void) {",
            "static int ku_legacy_query_tests(void) {",
            1,
        )
        .replacen(
            "static void* ku_test_pg_malloc(size_t size) {",
            "static void* input_real_malloc(size_t size) {",
            1,
        )
        + &ASYNC_QUERY_SUITE.replace("int main(void) {", "static int ku_async_pump_tests(void) {")
        + ASYNC_INPUT_OWNER_SUITE;
    let source = directory.path().join("async-pg-input-owner.c");
    fs::write(&source, harness).expect("write PG input owner harness");
    let Some(executable) = compile_harness(directory.path(), &source, "async-pg-input-owner")
    else {
        return;
    };
    let output = run_bounded(&mut Command::new(executable), RUN_TIMEOUT, RUN_LIMITS)
        .expect("PG input owner harness exceeded bounds");
    assert!(
        output.status.success(),
        "PG input owner harness failed: {}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).replace('\r', ""),
        "pg async input owner closed loop\n"
    );
}

fn run_async_acquire_waiter_handoff(kind: u8) {
    let name = match kind {
        0 => "async-query-acquire-handoff-expired",
        1 => "async-query-acquire-handoff-cancel",
        2 => "async-query-acquire-handoff-success",
        _ => panic!("unknown PG acquisition handoff case"),
    };
    let directory = TempDir::new(name);
    let harness = query_stub_harness(&directory).replacen(
        "int main(void) {",
        "static int ku_legacy_query_tests(void) {",
        1,
    ) + &ASYNC_QUERY_SUITE.replace(
        "int main(void) {",
        "static int ku_async_sequential_tests(void) {",
    ) + &format!("\n#define KU_TEST_HANDOFF_KIND {kind}\n")
        + ASYNC_ACQUIRE_HANDOFF_SUITE;
    let source = directory.path().join("async-query-acquire-handoff.c");
    fs::write(&source, harness).expect("write PG acquisition handoff harness");
    let Some(executable) =
        compile_harness(directory.path(), &source, "async-query-acquire-handoff")
    else {
        return;
    };
    let output = run_bounded(&mut Command::new(executable), RUN_TIMEOUT, RUN_LIMITS)
        .unwrap_or_else(|error| panic!("PG acquisition handoff test exceeded bounds: {error}"));
    assert!(
        output.status.success(),
        "PG acquisition handoff (kind={kind}) failed ({:?}):\n{}{}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).replace('\r', ""),
        "pg async waiter handoff closed loop\n"
    );
}

#[test]
fn native_pg_async_expired_waiter_hands_idle_lease_to_later_waiter() {
    run_async_acquire_waiter_handoff(0);
}

#[test]
fn native_pg_async_cancelled_waiter_hands_idle_lease_to_later_waiter() {
    run_async_acquire_waiter_handoff(1);
}

#[test]
fn native_pg_async_successful_waiter_hands_remaining_idle_lease_to_later_waiter() {
    run_async_acquire_waiter_handoff(2);
}

fn run_async_acquire_lock_deadline(connect: bool, out_of_memory: bool) {
    let directory = TempDir::new("async-acquire-lock-deadline");
    let harness = query_stub_harness(&directory)
        .replacen("static int ku_test_pg_pool_lock_depth = 0;", "static void ku_test_pg_expire_acquire_lock(void);\nstatic int ku_test_pg_pool_lock_depth = 0;", 1)
        .replace("ku_test_pg_pool_lock_depth++;", "ku_test_pg_pool_lock_depth++; ku_test_pg_expire_acquire_lock();")
        .replacen("int main(void) {", "static int ku_legacy_query_tests(void) {", 1)
        + &ASYNC_QUERY_SUITE.replace("int main(void) {", "static int ku_async_sequential_tests(void) {")
        + &format!("\n#define KU_TEST_EXPIRED_CONNECT {}\n#define KU_TEST_CONNECT_OOM {}\n", u8::from(connect), u8::from(out_of_memory))
        + ASYNC_ACQUIRE_LOCK_DEADLINE_SUITE;
    let source = directory.path().join("async-acquire-lock-deadline.c");
    fs::write(&source, harness).expect("write PG acquisition lock deadline harness");
    let Some(executable) =
        compile_harness(directory.path(), &source, "async-acquire-lock-deadline")
    else {
        return;
    };
    let output = run_bounded(&mut Command::new(executable), RUN_TIMEOUT, RUN_LIMITS)
        .expect("PG acquisition lock deadline test exceeded bounds");
    assert!(
        output.status.success(),
        "PG acquisition lock deadline (connect={connect}) failed ({:?}):\n{}{}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).replace('\r', ""),
        "pg async acquisition lock deadline closed loop\n"
    );
}

#[test]
fn native_pg_async_idle_lease_rechecks_deadline_after_pool_lock() {
    run_async_acquire_lock_deadline(false, false);
}

#[test]
fn native_pg_async_connected_lease_rechecks_deadline_after_pool_lock() {
    run_async_acquire_lock_deadline(true, false);
}

#[test]
fn native_pg_async_connect_oom_at_acquire_deadline_is_not_a_timeout_witness() {
    run_async_acquire_lock_deadline(true, true);
}

#[test]
fn native_pg_async_acquire_failures_keep_bounded_correlated_snapshots() {
    let directory = TempDir::new("async-acquire-diagnostics");
    let harness = query_stub_harness(&directory)
        // Nine queued operations plus the pool/reset owner exceed the original
        // sequential fixture's 16 allocation records; retain bounded tracking.
        .replacen(
            "#define KU_PG_TEST_ALLOCATION_RECORDS 16",
            "#define KU_PG_TEST_ALLOCATION_RECORDS 64",
            1,
        )
        .replacen(
            "int main(void) {",
            "static int ku_legacy_query_tests(void) {",
            1,
        )
        + &ASYNC_QUERY_SUITE.replace(
            "int main(void) {",
            "static int ku_async_sequential_tests(void) {",
        )
        + ASYNC_ACQUIRE_DIAGNOSTIC_SUITE;
    let source = directory.path().join("async-acquire-diagnostics.c");
    fs::write(&source, harness).expect("write bounded PG acquisition diagnostic harness");
    let Some(executable) = compile_harness(directory.path(), &source, "async-acquire-diagnostics")
    else {
        return;
    };
    let output = run_bounded(&mut Command::new(executable), RUN_TIMEOUT, RUN_LIMITS)
        .expect("PG acquisition diagnostic test exceeded bounds");
    assert!(
        output.status.success(),
        "PG acquisition diagnostics failed ({:?}):\n{}{}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).expect("PG diagnostic JSON is UTF-8");
    eprint!("{}", String::from_utf8_lossy(&output.stderr));
    let lines = stdout.lines().collect::<Vec<_>>();
    assert_eq!(
        lines.len(),
        5,
        "actual, maximum, partial, truncated and busy JSON witnesses"
    );
    for (index, line) in lines.iter().enumerate() {
        // Ku's JSON numeric contract rejects unsigned values above int64. The
        // formatter's maximum-width u64 tokens have already been checked in C;
        // replace only that exact valid decimal token to validate the entire
        // JSON structure using the existing parser, without a new dependency.
        let json = line.replace("18446744073709551615", "0");
        let parsed = ku::stdlib::json::eval(
            "decode",
            &[ku::value::Value::String(json)],
            ku::span::Span::default(),
        )
        .expect("parse PG diagnostic JSON");
        let Some(ku::value::Value::Result { ok: true, value }) = parsed else {
            panic!("invalid JSON witness {index}: {line}");
        };
        let ku::value::Value::Object(mut root) = *value else {
            panic!("JSON root must be an object");
        };
        let Some(ku::value::Value::Object(mut diagnostic)) = root.remove("pg_acquire_failures")
        else {
            panic!("missing PG diagnostic object");
        };
        let Some(ku::value::Value::Array(samples)) = diagnostic.remove("samples") else {
            panic!("missing PG samples");
        };
        let Some(ku::value::Value::Int(exported)) = diagnostic.get("exported_samples") else {
            panic!("missing exported sample count");
        };
        assert_eq!(
            *exported as usize,
            samples.len(),
            "sample count must describe complete exported objects"
        );
        if index == 1 || index == 2 {
            let expected = if index == 1 { 8..=8 } else { 1..=2 };
            assert!(
                expected.contains(&samples.len()),
                "maximum/partial sample count: {}",
                samples.len()
            );
            let Some(ku::value::Value::Int(omitted)) = diagnostic.get("omitted_samples") else {
                panic!("missing omitted sample count");
            };
            assert_eq!(samples.len() + *omitted as usize, 8);
            for (offset, sample) in samples.iter().enumerate() {
                let ku::value::Value::Object(fields) = sample else {
                    panic!("partial JSON sample is not complete");
                };
                assert!(
                    matches!(fields.get("sequence"), Some(ku::value::Value::Int(sequence)) if *sequence == 1 + offset as i64),
                    "export must preserve first failure then state changes and latest"
                );
                assert!(
                    matches!(fields.get("slots"), Some(ku::value::Value::Array(slots)) if slots.len() == 8)
                );
                assert!(
                    matches!(fields.get("queue"), Some(ku::value::Value::Array(queue)) if queue.len() == 8)
                );
            }
        }
    }
    assert!(lines[0].contains("\"overwritten\":7") && lines[0].contains("\"contention_drops\":1"));
    assert!(
        lines[0].contains("\"http_task_slot\":42")
            && lines[0].contains("\"lease_generation_saturated\":1")
    );
    assert!(lines[1].contains("18446744073709551615") && !lines[1].contains("\"truncated\":true"));
    assert!(lines[2].contains("\"truncated\":true") && !lines[2].contains("\"samples\":[]"));
    assert!(lines[3].contains("\"truncated\":true") && lines[3].contains("\"samples\":[]"));
    assert!(lines[4].contains("\"export_busy\":true") && lines[4].contains("\"samples\":[]"));
}

#[test]
fn native_pg_async_notification_pins_survive_close_and_owner_cancellation() {
    let directory = TempDir::new("async-query-notification-pin");
    let harness = query_stub_harness(&directory)
        // The sequential lock-depth probe is not a cross-thread counter.
        // Product mutexes, callback pins and barriers remain fully instrumented.
        .replace("ku_test_pg_pool_lock_depth++;", "")
        .replace("ku_test_pg_pool_lock_depth--;", "")
        .replacen(
            "int main(void) {",
            "static int ku_legacy_query_tests(void) {",
            1,
        )
        + NATIVE_THREAD_LIFECYCLE_HARNESS
        + &ASYNC_QUERY_SUITE.replace(
            "int main(void) {",
            "static int ku_async_sequential_tests(void) {",
        )
        + ASYNC_NOTIFICATION_PIN_SUITE;
    let source = directory.path().join("async-query-notification-pin.c");
    fs::write(&source, harness).expect("write PG notification pin harness");
    let Some(executable) =
        compile_harness(directory.path(), &source, "async-query-notification-pin")
    else {
        return;
    };
    let output = run_bounded(&mut Command::new(executable), RUN_TIMEOUT, RUN_LIMITS)
        .unwrap_or_else(|error| {
            panic!("PG notification pin test exceeded process bounds: {error}")
        });
    assert!(
        output.status.success(),
        "PG notification pin harness failed ({:?}):\n{}{}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).replace('\r', ""),
        "pg async notification pin closed loop\n"
    );
}

#[test]
fn native_pg_compact_result_growth_oom_and_detached_lifetime_keep_physical_budget() {
    let directory = TempDir::new("pg-compact-result");
    let harness = query_stub_harness(&directory)
        .replacen(
            "int main(void) {",
            "static int ku_legacy_query_tests(void) {",
            1,
        )
        .replacen(
            "#define KU_PG_MAX_RESULT_CELLS 8ULL",
            "#define KU_PG_MAX_RESULT_CELLS 4096ULL",
            1,
        )
        .replacen(
            "#define KU_PG_MAX_RESULT_BYTES 16ULL",
            "#define KU_PG_MAX_RESULT_BYTES 1048576ULL",
            1,
        )
        + &ASYNC_QUERY_SUITE.replace(
            "int main(void) {",
            "static int ku_async_sequential_tests(void) {",
        )
        + ASYNC_COMPACT_RESULT_SUITE;
    // PG's malloc hook is installed after common KuString helpers. Route the
    // real string drop through the same free hook in this lifetime fixture;
    // otherwise a physically freed getter copy remains in its test ledger.
    let string_free = "if (value->storage == KU_STRING_OWNED && value->ptr) free(value->ptr);";
    assert_eq!(harness.matches(string_free).count(), 1);
    let harness = format!(
        "static void ku_test_pg_free(void*);\n{}",
        harness.replacen(
            string_free,
            "if (value->storage == KU_STRING_OWNED && value->ptr) ku_test_pg_free(value->ptr);",
            1,
        )
    );
    let source = directory.path().join("pg-compact-result.c");
    fs::write(&source, harness).expect("write compact owned PG result harness");
    let Some(executable) = compile_harness(directory.path(), &source, "pg-compact-result") else {
        return;
    };
    let output = run_bounded(&mut Command::new(executable), RUN_TIMEOUT, RUN_LIMITS)
        .expect("compact owned PG result harness exceeded bounds");
    assert!(
        output.status.success(),
        "compact owned PG result harness failed ({:?}):\n{}{}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).replace('\r', ""),
        "pg compact result ownership and physical budget closed loop\n"
    );
}

#[test]
fn native_pg_first_row_size_classes_cover_shapes_and_growth_without_leaking() {
    let directory = TempDir::new("pg-first-row-classes");
    let harness = query_stub_harness(&directory)
        .replacen("int main(void) {", "static int ku_legacy_query_tests(void) {", 1)
        .replacen("#define KU_PG_MAX_RESULT_ROWS 4ULL", "#define KU_PG_MAX_RESULT_ROWS 64ULL", 1)
        .replacen("#define KU_PG_MAX_RESULT_COLS 4ULL", "#define KU_PG_MAX_RESULT_COLS 32ULL", 1)
        .replacen("#define KU_PG_MAX_RESULT_CELLS 8ULL", "#define KU_PG_MAX_RESULT_CELLS 4096ULL", 1)
        .replacen("#define KU_PG_MAX_RESULT_BYTES 16ULL", "#define KU_PG_MAX_RESULT_BYTES 1048576ULL", 1)
        .replacen("enum {\n  T_OK", "static int first_class_columns,first_class_rows,first_class_large,first_class_changed;static char first_class_bytes[1025];\nenum {\n  T_FIRST_CLASS,T_OK", 1)
        .replacen("switch (conn->mode) {", "switch (conn->mode) {\n    case T_FIRST_CLASS: conn->total=first_class_rows+1;break;", 1)
        .replacen("int PQnfields(const PGresult* result) {", "int PQnfields(const PGresult* result) { if(result->mode==T_FIRST_CLASS)return first_class_columns+(first_class_changed && result->number>1);", 1)
        .replacen("int PQgetisnull(const PGresult* result, int row, int column) {", "int PQgetisnull(const PGresult* result, int row, int column) { if(result->mode==T_FIRST_CLASS)return !first_class_large && column%3==0;", 1)
        .replacen("char* PQgetvalue(const PGresult* result, int row, int column) {", "char* PQgetvalue(const PGresult* result, int row, int column) { if(result->mode==T_FIRST_CLASS)return first_class_large?first_class_bytes:column%3==1?\"\":\"\\xe4\\xb8\\xad\\xe6\\x96\\x87\\xf0\\x9f\\x99\\x82\";", 1)
        + &ASYNC_QUERY_SUITE.replace("int main(void) {", "static int ku_async_sequential_tests(void) {")
        + ASYNC_FIRST_CLASS_SUITE;
    let source = directory.path().join("pg-first-row-classes.c");
    fs::write(&source, harness).expect("write first-row PG class harness");
    let Some(executable) = compile_harness(directory.path(), &source, "pg-first-row-classes")
    else {
        return;
    };
    let output = run_bounded(&mut Command::new(executable), RUN_TIMEOUT, RUN_LIMITS)
        .expect("PG first-row class harness exceeded bounds");
    assert!(
        output.status.success(),
        "PG first-row class harness failed: {}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).replace('\r', ""),
        "pg first-row size classes and physical ownership closed loop\n"
    );
}

#[test]
fn native_pg_typed_single_row_binary_values_validate_types_and_survive_reset() {
    let directory = TempDir::new("pg-typed-single-row");
    let globals = format!("{}\nenum {{\n  T_TYPED,T_OK", TYPED_STUB_GLOBALS);
    let harness=query_stub_harness(&directory)
        .replacen("int main(void) {", "static int ku_legacy_query_tests(void) {", 1)
        .replacen("#define KU_PG_MAX_RESULT_COLS 4ULL", "#define KU_PG_MAX_RESULT_COLS 16ULL", 1)
        .replacen("#define KU_PG_MAX_RESULT_CELLS 8ULL", "#define KU_PG_MAX_RESULT_CELLS 1024ULL", 1)
        .replacen("#define KU_PG_MAX_RESULT_BYTES 16ULL", "#define KU_PG_MAX_RESULT_BYTES 1048576ULL", 1)
        .replacen("enum {\n  T_OK", &globals, 1)
        .replacen("switch (conn->mode) {", "switch (conn->mode) {\n    case T_TYPED: conn->total=typed_rows+1;break;", 1)
        .replacen("int PQsendQueryParams(PGconn* conn, const char* sql, int count, const void* types, const char* const* values, const int* lengths, const int* formats, int format) {", "int PQsendQueryParams(PGconn* conn, const char* sql, int count, const void* types, const char* const* values, const int* lengths, const int* formats, int format) { if(format==1){ if(!typed_accept_parameters(count,types,values,lengths,formats))return 0;return send_query(conn,sql); }", 1)
        .replacen("int PQnfields(const PGresult* result) {", "int PQnfields(const PGresult* result) { if(result->mode==T_TYPED)return 10;", 1)
        .replacen("static int result_format(const PGresult* result, int column) {", "static int result_format(const PGresult* result, int column) { if(result->mode==T_TYPED)return (typed_errors&2)?0:1;", 1)
        .replacen("unsigned int PQftype(const PGresult* result,int column) {", "unsigned int PQftype(const PGresult* result,int column) { if(result->mode==T_TYPED)return (typed_errors&1)&&column==0?9999u:typed_column_oids[column];", 1)
        .replacen("int PQgetisnull(const PGresult* result, int row, int column) {", "int PQgetisnull(const PGresult* result, int row, int column) { if(result->mode==T_TYPED)return column==5 || column==9 || ((typed_errors&8)&&column==0);", 1)
        .replacen("char* PQgetvalue(const PGresult* result, int row, int column) {", "char* PQgetvalue(const PGresult* result, int row, int column) { if(result->mode==T_TYPED){ if(column==3)return (typed_errors&16)?\"\\xff\":typed_large?typed_large_bytes:typed_source_text;if(column==4)return \"\";if(column==2 && (typed_errors&32))return \"\\2\";return (char*)typed_numbers[column]; }", 1)
        .replacen("int PQgetlength(const PGresult* result, int row, int column) {", "int PQgetlength(const PGresult* result, int row, int column) { if(result->mode==T_TYPED)return column==0?(typed_errors&4)?3:4:column==1||column==7?8:column==2||column==8?1:column==3?(typed_errors&16)?1:typed_large?131079:14:column==6?4:0;", 1)
        + &ASYNC_QUERY_SUITE.replace("int main(void) {", "static int ku_async_sequential_tests(void) {")
        + TYPED_QUERY_SUITE;
    let string_free = "if (value->storage == KU_STRING_OWNED && value->ptr) free(value->ptr);";
    assert_eq!(harness.matches(string_free).count(), 1);
    let harness = format!(
        "static void ku_test_pg_free(void*);\n{}",
        harness.replacen(
            string_free,
            "if (value->storage == KU_STRING_OWNED && value->ptr) ku_test_pg_free(value->ptr);",
            1
        )
    );
    let source = directory.path().join("pg-typed-single-row.c");
    fs::write(&source, harness).expect("write typed PG binary fixture");
    let Some(executable) = compile_harness(directory.path(), &source, "pg-typed-single-row") else {
        return;
    };
    let output = run_bounded(&mut Command::new(executable), RUN_TIMEOUT, RUN_LIMITS)
        .expect("typed PG binary fixture exceeded bounds");
    assert!(
        output.status.success(),
        "typed PG binary fixture failed: {}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).replace('\r', ""),
        "pg typed single-row binary values and strict lifetime closed loop\n"
    );
}

#[test]
fn native_pg_async_large_utf8_row_yields_at_byte_quantum_and_detaches_storage() {
    let directory = TempDir::new("async-query-large-row");
    let harness=query_stub_harness(&directory)
        .replacen("int main(void) {", "static int ku_legacy_query_tests(void) {", 1)
        .replacen("#define KU_PG_MAX_RESULT_BYTES 16ULL", "#define KU_PG_MAX_RESULT_BYTES 200000ULL", 1)
        .replacen("enum {\n  T_OK", "static int async_large_fixture; static char async_large_data[131080];\nenum {\n  T_OK", 1)
        .replacen("char* PQgetvalue(const PGresult* result, int row, int column) {", "char* PQgetvalue(const PGresult* result, int row, int column) { if(async_large_fixture)return async_large_data;", 1)
        + &ASYNC_QUERY_SUITE.replace("int main(void) {", "static int ku_async_sequential_tests(void) {")
        + ASYNC_LARGE_ROW_SUITE;
    let source = directory.path().join("async-query-large-row.c");
    fs::write(&source, harness).unwrap();
    let Some(executable) = compile_harness(directory.path(), &source, "async-query-large-row")
    else {
        return;
    };
    let output = run_bounded(&mut Command::new(executable), RUN_TIMEOUT, RUN_LIMITS).unwrap();
    assert!(
        output.status.success(),
        "large PG row harness failed: {}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).replace('\r', ""),
        "pg async large row closed loop\n"
    );
}

#[test]
fn native_pg_task_fresh_cancel_observer_checks_real_job_state_and_generation() {
    let directory = TempDir::new("pg-task-fresh-cancel");
    let sync_fixture = fixture()
        .split_once("fn main(): null! {")
        .expect("sync fixture main")
        .0;
    let source = format!(
        "import http from \"std.http\"\n{}\n{}",
        sync_fixture,
        r#"fn main(): null! {
    client = pg.client({conninfo: "hostaddr=127.0.0.1", max_connections: 1})?
    async fn read(): HttpResponse! {
        result = client.query("SELECT 3", [])?
        return ok(http.text(result.value(0, 0)?))
    }
    app = http.service()
    app.get("/read", read)
    app.listen("127.0.0.1:0")?
    return ok(null)
}
"#
    );
    let harness = query_stub_harness_source(&directory, &source).replacen(
        "int main(void) {",
        "static int ku_legacy_query_tests(void) {",
        1,
    ) + PG_TASK_FRESH_CANCEL_SUITE;
    assert!(harness.contains(
        "operation->fresh_cancel_context=job;operation->fresh_cancel=ku_task_pg_fresh_cancel;"
    ));
    let path = directory.path().join("pg-task-fresh-cancel.c");
    fs::write(&path, harness).expect("write real Task PG observer harness");
    let Some(executable) = compile_harness(directory.path(), &path, "pg-task-fresh-cancel") else {
        return;
    };
    let output = run_bounded(&mut Command::new(executable), RUN_TIMEOUT, RUN_LIMITS)
        .expect("real Task PG observer harness exceeded bounds");
    assert!(
        output.status.success(),
        "real Task PG observer failed: {}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).replace('\r', ""),
        "pg Task fresh cancel observer closed loop\n"
    );
}

#[test]
fn native_pg_async_rows_share_owner_turn_budgets_without_empty_growth_charges() {
    let directory = TempDir::new("async-query-turns");
    let generated = query_stub_harness(&directory);
    // The text and typed materializers are mutually exclusive. Instrument both;
    // a first-match hook would silently miss text after adding the typed helper.
    assert_eq!(
        generated
            .matches("int valid=ku_pg_utf8_valid(source+op->row_offset,count,op->deadline);")
            .count(),
        2
    );
    assert_eq!(
        generated
            .matches("if(count)memcpy(result->bytes+result->bytes_len")
            .count(),
        2
    );
    let harness = generated
        .replacen("int main(void) {", "static int ku_legacy_query_tests(void) {", 1)
        .replacen("#define KU_PG_MAX_RESULT_ROWS 4ULL", "#define KU_PG_MAX_RESULT_ROWS 1024ULL", 1)
        .replacen("#define KU_PG_MAX_RESULT_CELLS 8ULL", "#define KU_PG_MAX_RESULT_CELLS 4096ULL", 1)
        .replacen("#define KU_PG_MAX_RESULT_BYTES 16ULL", "#define KU_PG_MAX_RESULT_BYTES 1048576ULL", 1)
        .replacen("typedef struct KuPgAsyncTurn", "static size_t async_validated_bytes,async_copied_bytes,async_reported_bytes,async_reported_cells,async_protocol_results;\ntypedef struct KuPgAsyncTurn", 1)
        .replace("int valid=ku_pg_utf8_valid(source+op->row_offset,count,op->deadline);", "async_validated_bytes+=count;int valid=ku_pg_utf8_valid(source+op->row_offset,count,op->deadline);")
        .replace("if(count)memcpy(result->bytes+result->bytes_len", "if(count)async_copied_bytes+=count;\n    if(count)memcpy(result->bytes+result->bytes_len")
        .replacen("int appended=ku_pg_async_materialize(op,&turn);", "int appended=ku_pg_async_materialize(op,&turn);async_reported_bytes=turn.bytes;async_reported_cells=turn.cells;", 1)
        .replacen("enum {\n  T_OK", "static int async_fixture_rows,async_fixture_null_empty,async_fixture_invalid_row;static char async_fixture_data[4001];\nenum {\n  T_ASYNC_ROWS,T_OK", 1)
        .replacen("switch (conn->mode) {", "switch (conn->mode) {\n    case T_ASYNC_ROWS: conn->total=async_fixture_rows+1;break;", 1)
        .replacen("PGresult* PQgetResult(PGconn* conn) {", "PGresult* PQgetResult(PGconn* conn) { async_protocol_results++;", 1)
        .replacen("int PQnfields(const PGresult* result) {", "int PQnfields(const PGresult* result) { if(result->mode==T_ASYNC_ROWS)return 3;", 1)
        .replacen("int PQgetisnull(const PGresult* result, int row, int column) {", "int PQgetisnull(const PGresult* result, int row, int column) { if(result->mode==T_ASYNC_ROWS)return async_fixture_null_empty && column==0;", 1)
        .replacen("char* PQgetvalue(const PGresult* result, int row, int column) {", "char* PQgetvalue(const PGresult* result, int row, int column) { if(result->mode==T_ASYNC_ROWS)return result->number==async_fixture_invalid_row?\"\\xff\":async_fixture_null_empty && column==1?\"\":async_fixture_data;", 1)
        + &ASYNC_QUERY_SUITE.replace("int main(void) {", "static int ku_async_sequential_tests(void) {")
        + ASYNC_TURN_SUITE;
    let source = directory.path().join("async-query-turns.c");
    fs::write(&source, harness).expect("write PG shared turn budget harness");
    let Some(executable) = compile_harness(directory.path(), &source, "async-query-turns") else {
        return;
    };
    let output = run_bounded(&mut Command::new(executable), RUN_TIMEOUT, RUN_LIMITS)
        .unwrap_or_else(|error| panic!("PG turn budget test exceeded process bounds: {error}"));
    assert!(
        output.status.success(),
        "PG shared turn budget harness failed: {}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).replace('\r', ""),
        "pg async shared turns closed loop\n"
    );
}

fn run_pg_live_loopback(name: &str, suite: &str, pipeline: bool) -> Option<String> {
    let Some(connfile) = env::var_os("KU_PG_TEST_CONNINFO_FILE").map(PathBuf::from) else {
        eprintln!("skip: set KU_PG_TEST_CONNINFO_FILE to a local test connection file");
        return None;
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
    let directory = TempDir::new(name);
    let generated = emit_c(directory.path(), fixture());
    let mut harness = generated.replacen(
        "int main(void) {",
        "static int ku_generated_main(void) {",
        1,
    );
    if pipeline {
        // Interpose only this isolated experiment. Production keeps its original
        // send path and never enables pipeline mode.
        let anchor = "extern int PQsetSingleRowMode(PGconn*);";
        assert_eq!(harness.matches(anchor).count(), 1);
        harness = harness.replacen(anchor,
            "static int p8_reset_sends;\nstatic int p8_send(PGconn*, const char*, int, const void*, const char* const*, const int*, const int*, int);\n#define PQsendQueryParams p8_send\nextern int PQsetSingleRowMode(PGconn*);", 1);
        harness.push_str("\n#undef PQsendQueryParams\n");
    }
    harness.push_str(suite);
    let source = directory.path().join(format!("{name}.c"));
    fs::write(&source, harness).expect("write live PG harness without embedded credentials");
    let executable = compile_harness_with_libpq(directory.path(), &source, name, &libdir)?;
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
        return None;
    }
    assert!(
        output.status.success(),
        "live PG loopback harness failed ({:?}):\n{}{}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    Some(String::from_utf8_lossy(&output.stdout).replace('\r', ""))
}

#[test]
#[ignore = "explicit opt-in: KU_PG_TEST_CONNINFO_FILE must parse as loopback before any connection"]
fn native_pg_query_poller_live_loopback_roundtrip() {
    let Some(output) = run_pg_live_loopback("query-live", LIVE_LOOPBACK, false) else {
        return;
    };
    assert_eq!(output, "pg query live loopback closed loop\n");
}

#[test]
#[ignore = "explicit protocol experiment: KU_PG_TEST_CONNINFO_FILE must parse as loopback before any connection"]
fn native_pg_finite_pipeline_live_loopback_preserves_conditional_reset_boundary() {
    let (common, _) = LIVE_LOOPBACK
        .split_once("int main(void) {")
        .expect("live guard and helpers");
    let suite = format!("{common}{FINITE_PIPELINE_LOOPBACK}");
    let Some(output) = run_pg_live_loopback("finite-pipeline-live", &suite, true) else {
        return;
    };
    // Preserve the bounded trace with --nocapture. It contains phase counts only,
    // never the private conninfo or server diagnostics.
    eprint!("{output}");
    assert!(output.contains("\"default_changed\":false"));
    assert!(output.contains("\"strict_default_equivalent\":false"));
    assert!(output.ends_with("pg finite pipeline conditional-reset contract closed loop\n"));
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
static int ku_test_typed_live_idle(KuPgClient* pool) {
  return pool && pool->conns[0] && !pool->in_use[0] && pool->active==0
      && PQstatus(pool->conns[0])==KU_PG_CONNECTION_OK
      && PQtransactionStatus(pool->conns[0])==KU_PQTRANS_IDLE
      && ku_pg_connection_is_utf8(pool->conns[0]);
}
static int ku_test_typed_live_row(KuPgResult* row,const uint8_t* text,size_t length) {
  if(!row || ku_pg_rows(row)!=1 || ku_pg_cols(row)!=10)return 0;
  const int64_t integers[]={INT32_MIN,INT32_MAX,INT64_MIN,INT64_MAX};
  for(size_t i=0;i<4u;i++) {
    KuResult_int value=ku_pg_value_int(row,0,(int64_t)i);
    if(!value.ok) {ku_error_drop(&value.error);return 0;}
    if(value.value!=integers[i])return 0;
  }
  for(size_t i=4u;i<6u;i++) {
    KuResult_bool value=ku_pg_value_bool(row,0,(int64_t)i);
    if(!value.ok) {ku_error_drop(&value.error);return 0;}
    if(value.value!=(i==4u))return 0;
  }
  for(size_t i=6u;i<8u;i++) {
    KuResult_str value=ku_pg_value(row,0,(int64_t)i);
    if(!value.ok) {ku_error_drop(&value.error);return 0;}
    int valid=value.value.len==length && (!length || !memcmp(value.value.ptr,text,length));
    ku_string_drop(&value.value);if(!valid)return 0;
  }
  KuResult_bool null_cell=ku_pg_is_null(row,0,8),empty_cell=ku_pg_is_null(row,0,9);
  if(!null_cell.ok || !empty_cell.ok) {
    if(!null_cell.ok)ku_error_drop(&null_cell.error);if(!empty_cell.ok)ku_error_drop(&empty_cell.error);return 0;
  }
  if(!null_cell.value || empty_cell.value)return 0;
  KuResult_str empty=ku_pg_value(row,0,9);
  if(!empty.ok) {ku_error_drop(&empty.error);return 0;}
  int valid=empty.value.len==0;ku_string_drop(&empty.value);return valid;
}
static int ku_test_typed_live(KuPgClient** owned_pool) {
  KuPgClient* pool=*owned_pool;LIVE_CHECK(ku_test_typed_live_idle(pool));
  /* Both text OIDs bind real owned UTF8 bytes, including quotes and backslash.
   * The parameter headers are read-only aliases; only this source owns them. */
  static const uint8_t utf8[]="\xe4\xb8\xad\xe6\x96\x87\xf0\x9f\x99\x82 '\\\n";
  KuString source=ku_string_alloc(sizeof(utf8)-1u);memcpy(source.ptr,utf8,sizeof(utf8)-1u);
  KuValue values[]={ku_v_int(INT32_MIN),ku_v_int(INT32_MAX),ku_v_int(INT64_MIN),ku_v_int(INT64_MAX),
      ku_v_bool(true),ku_v_bool(false),ku_v_str(source),ku_v_str(source),ku_v_null(),ku_v_str(ku_test_text(""))};
  KuString types[]={ku_test_text("int4"),ku_test_text("int4"),ku_test_text("int8"),ku_test_text("int8"),
      ku_test_text("bool"),ku_test_text("bool"),ku_test_text("text"),ku_test_text("varchar"),ku_test_text("text?"),ku_test_text("varchar")};
  KuArray_str declared={10u,types};
  KuResult_pg_result result=ku_pg_client_query_one_typed(pool,
      ku_test_text("SELECT $1::int4,$2::int4,$3::int8,$4::int8,$5::bool,$6::bool,$7::text,$8::varchar,$9::text,$10::varchar"),
      values,10u,declared,declared);
  LIVE_CHECK(result.ok && ku_test_typed_live_idle(pool));
  LIVE_CHECK(values[0].as.i==INT32_MIN && values[3].as.i==INT64_MAX
      && values[6].as.s.ptr==source.ptr && values[7].as.s.ptr==source.ptr);
  LIVE_CHECK(ku_test_typed_live_row(result.value,utf8,sizeof(utf8)-1u));
  KuPgResult* retained=result.value;
  ku_string_drop(&source); /* PQclear/reset and freeing input cannot invalidate the row. */
  memset(values,0,sizeof(values));
  KuString int4=ku_test_text("int4"),text=ku_test_text("text"),nullable=ku_test_text("text?");
  KuArray_str one_int={1u,&int4},one_text={1u,&text},one_nullable={1u,&nullable},none={0};
  result=ku_pg_client_query_one_typed(pool,ku_test_text("SELECT 1::int4 WHERE false"),NULL,0,none,one_int);
  LIVE_CHECK(ku_test_code(result,"row_count_error") && ku_test_typed_live_idle(pool));ku_error_drop(&result.error);
  result=ku_pg_client_query_one_typed(pool,ku_test_text("SELECT i::int4 FROM generate_series(1,2) AS i"),NULL,0,none,one_int);
  LIVE_CHECK(ku_test_code(result,"row_count_error") && ku_test_typed_live_idle(pool));ku_error_drop(&result.error);
  /* Two preceding rows violate cardinality, then SQL fails: the later server
   * error must be drained and win instead of returning the retained first row. */
  result=ku_pg_client_query_one_typed(pool,ku_test_text("SELECT (10/(3-i))::int4 FROM generate_series(1,3) AS i"),NULL,0,none,one_int);
  LIVE_CHECK(ku_test_code(result,"query_error") && ku_test_typed_live_idle(pool));ku_error_drop(&result.error);
  result=ku_pg_client_query_one_typed(pool,ku_test_text("SELECT 1::numeric"),NULL,0,none,one_int);
  LIVE_CHECK(ku_test_code(result,"typed_value_error") && ku_test_typed_live_idle(pool));ku_error_drop(&result.error);
  result=ku_pg_client_query_one_typed(pool,ku_test_text("SELECT NULL::text"),NULL,0,none,one_text);
  LIVE_CHECK(ku_test_code(result,"null_value") && ku_test_typed_live_idle(pool));ku_error_drop(&result.error);
  result=ku_pg_client_query_one_typed(pool,ku_test_text("SELECT NULL::text"),NULL,0,none,one_nullable);
  LIVE_CHECK(result.ok && ku_test_typed_live_idle(pool));
  KuResult_bool is_null=ku_pg_is_null(result.value,0,0);LIVE_CHECK(is_null.ok && is_null.value);ku_drop_pg_result(&result.value);
  static const uint8_t nul_bytes[]={'a',0,'b'},invalid_utf8[]={0xff};
  for(size_t kind=0;kind<2u;kind++) {
    KuString parameter_type=ku_test_text(kind?"varchar":"text");KuArray_str schema={1u,&parameter_type};
    for(size_t invalid=0;invalid<2u;invalid++) {
      const uint8_t* bytes=invalid?invalid_utf8:nul_bytes;size_t length=invalid?sizeof(invalid_utf8):sizeof(nul_bytes);
      KuValue parameter=ku_v_str(ku_string_static(bytes,length));
      result=ku_pg_client_query_one_typed(pool,ku_test_text(kind?"SELECT $1::varchar":"SELECT $1::text"),&parameter,1u,schema,schema);
      LIVE_CHECK(ku_test_code(result,"typed_value_error") && ku_test_typed_live_idle(pool)
          && parameter.tag==KU_STR && parameter.as.s.ptr==bytes && parameter.as.s.len==length);ku_error_drop(&result.error);
    }
  }
  KuValue too_large=ku_v_int(INT64_MAX);
  result=ku_pg_client_query_one_typed(pool,ku_test_text("SELECT $1::int4"),&too_large,1u,one_int,one_int);
  LIVE_CHECK(ku_test_code(result,"typed_value_error") && ku_test_typed_live_idle(pool));ku_error_drop(&result.error);
  /* One original absolute D covers the real server wait; do not increase it
   * between query, drain or reset. Termination is not a database rollback claim. */
  unsigned long long started=__ku_handler_now_ms();__ku_handler_deadline=started+100u;
  result=ku_pg_client_query_one_typed(pool,ku_test_text("SELECT 7::int4 FROM pg_sleep(2)"),NULL,0,none,one_int);
  __ku_handler_deadline=0;
  LIVE_CHECK(ku_test_code(result,"execution_unknown") && __ku_handler_now_ms()-started<1500u
      && !pool->conns[0] && !pool->in_use[0] && pool->active==0);ku_error_drop(&result.error);
  /* No SQL retry: this is a distinct next request after the discarded lease. */
  result=ku_pg_client_query_one_typed(pool,ku_test_text("SELECT 8::int4"),NULL,0,none,one_int);
  LIVE_CHECK(result.ok && ku_test_typed_live_idle(pool));
  KuResult_int next=ku_pg_value_int(result.value,0,0);LIVE_CHECK(next.ok && next.value==8);ku_drop_pg_result(&result.value);
  LIVE_CHECK(ku_test_typed_live_row(retained,utf8,sizeof(utf8)-1u));
  ku_drop_pg_client(owned_pool);LIVE_CHECK(!*owned_pool);
  LIVE_CHECK(ku_test_typed_live_row(retained,utf8,sizeof(utf8)-1u));
  ku_drop_pg_result(&retained);LIVE_CHECK(!retained);return 0;
}
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
  LIVE_CHECK(!ku_test_typed_live(&pool) && !pool);
  puts("pg query live loopback closed loop");
  return 0;
}
"#;

// Explicit, bounded protocol experiment; not an alternate product execution path.
const FINITE_PIPELINE_LOOPBACK: &str = r#"
extern int PQenterPipelineMode(PGconn*);
extern int PQexitPipelineMode(PGconn*);
extern int PQpipelineSync(PGconn*);
extern int PQpipelineStatus(const PGconn*);
static int p8_send(PGconn* connection,const char* sql,int count,const void* types,
    const char* const* values,const int* lengths,const int* formats,int format) {
  if(!strcmp(sql,"DISCARD ALL"))p8_reset_sends++;
  return PQsendQueryParams(connection,sql,count,types,values,lengths,formats,format);
}
typedef struct P8Trace {
  int queued,sync_requests,sync_results,single_rows,tuples,command_ok,errors,aborted;
  int first_idle,first_utf8,reset_ok,pipeline_off,idle,utf8,expired,results,closed;
} P8Trace;
static int p8_pipeline(const char* conninfo,const char* sql,unsigned long long milliseconds,P8Trace* trace) {
  KuPgConnectAttempt opened=ku_pg_connect_until(conninfo,ku_pg_now_ms()+2000u);
  LIVE_CHECK(opened.conn);PGconn* conn=opened.conn;
  LIVE_CHECK(PQsetnonblocking(conn,1)==0 && PQenterPipelineMode(conn));
  unsigned long long deadline=ku_pg_now_ms()+milliseconds;
  if(ku_pg_deadline_expired(deadline))goto expired;
  LIVE_CHECK(PQsendQueryParams(conn,sql,0,NULL,NULL,NULL,NULL,0));trace->queued++;
  if(ku_pg_deadline_expired(deadline))goto expired;
  LIVE_CHECK(PQpipelineSync(conn));trace->sync_requests++;
  if(ku_pg_deadline_expired(deadline))goto expired;
  LIVE_CHECK(PQsendQueryParams(conn,"DISCARD ALL",0,NULL,NULL,NULL,NULL,0));trace->queued++;
  if(ku_pg_deadline_expired(deadline))goto expired;
  LIVE_CHECK(PQpipelineSync(conn));trace->sync_requests++;
  /* Select the first queued business command before retrieving any result. */
  LIVE_CHECK(PQsetSingleRowMode(conn));
  for(size_t work=0;work<32u;) {
    if(ku_pg_deadline_expired(deadline))goto expired;
    int flushed=PQflush(conn);LIVE_CHECK(flushed>=0);
    if(flushed) {
      int ready=ku_pg_wait_socket(PQsocket(conn),3,deadline);
      if(!ready)goto expired;LIVE_CHECK(ready>0);
      if(ready&1)LIVE_CHECK(PQconsumeInput(conn));
      continue;
    }
    if(PQisBusy(conn)) {
      int ready=ku_pg_wait_socket(PQsocket(conn),1,deadline);
      if(!ready)goto expired;LIVE_CHECK(ready>0 && PQconsumeInput(conn));
      continue;
    }
    PGresult* result=PQgetResult(conn);work++;
    if(!result) {if(trace->sync_results==2)break;continue;}
    trace->results++;int status=PQresultStatus(result);
    if(status==10) {
      trace->sync_results++;
      if(trace->sync_results==1) {
        trace->first_idle=PQtransactionStatus(conn)==KU_PQTRANS_IDLE;
        trace->first_utf8=ku_pg_connection_is_utf8(conn);
      }
    } else if(status==11)trace->aborted++;
    else if(status==KU_PGRES_SINGLE_TUPLE)trace->single_rows++;
    else if(status==KU_PGRES_TUPLES_OK)trace->tuples++;
    else if(status==KU_PGRES_COMMAND_OK) {
      trace->command_ok++;if(trace->sync_results==1)trace->reset_ok=1;
    } else if(status==KU_PGRES_FATAL_ERROR)trace->errors++;
    else {PQclear(result);PQfinish(conn);return 1;}
    PQclear(result);
  }
  LIVE_CHECK(trace->sync_results==2 && PQexitPipelineMode(conn));
  trace->pipeline_off=PQpipelineStatus(conn)==0;
  trace->idle=PQtransactionStatus(conn)==KU_PQTRANS_IDLE;trace->utf8=ku_pg_connection_is_utf8(conn);
  LIVE_CHECK(trace->pipeline_off && PQgetResult(conn)==NULL);
  PQfinish(conn);trace->closed=1;return 0;
expired:
  trace->expired=1;PQfinish(conn);trace->closed=1;return 0;
}
static int p8_reserve(void* raw,size_t bytes) {size_t* value=(size_t*)raw;if(bytes>SIZE_MAX-*value)return 0;*value+=bytes;return 1;}
static void p8_refund(void* raw,size_t bytes) {size_t* value=(size_t*)raw;if(bytes>*value)abort();*value-=bytes;}
static void p8_wake(void* raw) {(void)raw;}
static int p8_old_encoding(const char* conninfo,int* retained,int* evicted,int* resets) {
  KuResult_pg_client opened=ku_pg_client_open(ku_test_text(conninfo),1,8,1000,1000,2000);LIVE_CHECK(opened.ok);
  KuPgClient* pool=opened.value;size_t charge=0;KuError error={0};int before=p8_reset_sends;
  KuPgAsyncOp* op=ku_pg_async_create(pool,ku_test_text("SELECT set_config('client_encoding','LATIN1',false)"),
      (KuArray_str){0},ku_pg_now_ms()+2000u,(KuPgAsyncBudget){&charge,p8_reserve,p8_refund},p8_wake,NULL,&error);
  LIVE_CHECK(op);
  for(size_t turns=0;op->phase!=KU_PG_ASYNC_DONE && turns<512u;turns++) {
    if(ku_pg_async_step(op,3u,ku_pg_now_ms(),0))break;
    if(op->phase==KU_PG_ASYNC_CONNECT && !op->connect_ready)ku_pg_async_connect_run(op,0);
    int fd=-1;unsigned int interest=0;unsigned long long deadline=0;
    if(!ku_pg_async_interest(op,&fd,&interest,&deadline)) {
      int ready=ku_pg_wait_socket(fd,interest,deadline);LIVE_CHECK(ready>=0);
    }
  }
  LIVE_CHECK(op->phase==KU_PG_ASYNC_DONE && op->output.ok);
  *retained=ku_test_value(op->output.value,"LATIN1");*evicted=pool->conns[0]==NULL;*resets=p8_reset_sends-before;
  KuResult_pg_result result=ku_pg_async_take(op);size_t bytes=0;LIVE_CHECK(ku_pg_result_owned_bytes(result.value,&bytes));
  ku_drop_pg_result(&result.value);p8_refund(&charge,bytes);ku_pg_async_dispose(op,1);ku_pg_client_close_owned(pool);
  LIVE_CHECK(!charge);return 0;
}
static void p8_trace_json(const char* name,P8Trace* trace,int comma) {
  printf("%s\"%s\":{\"commands_queued\":%d,\"sync_requests\":%d,\"sync_results\":%d,\"single_rows\":%d,\"tuples\":%d,\"command_ok\":%d,\"errors\":%d,\"aborted\":%d,\"first_idle\":%d,\"first_utf8\":%d,\"reset_ok\":%d,\"pipeline_off\":%d,\"idle\":%d,\"utf8\":%d,\"deadline_expired\":%d,\"closed\":%d}",
      comma?",":"",name,trace->queued,trace->sync_requests,trace->sync_results,trace->single_rows,trace->tuples,
      trace->command_ok,trace->errors,trace->aborted,trace->first_idle,trace->first_utf8,trace->reset_ok,
      trace->pipeline_off,trace->idle,trace->utf8,trace->expired,trace->closed);
}
int main(void) {
  const char* path=getenv("KU_PG_TEST_CONNINFO_FILE");LIVE_CHECK(path);
  FILE* file=fopen(path,"rb");LIVE_CHECK(file);char conninfo[8193];size_t size=fread(conninfo,1,sizeof(conninfo)-1u,file);
  int read_failed=ferror(file);fclose(file);LIVE_CHECK(!read_failed && size<sizeof(conninfo)-1u);conninfo[size]=0;
  if(strlen(conninfo)!=size || !ku_test_conninfo_is_loopback(conninfo))return 77;
  int retained=0,evicted=0,resets=0;
  LIVE_CHECK(!p8_old_encoding(conninfo,&retained,&evicted,&resets));
  LIVE_CHECK(retained && evicted && !resets);
  P8Trace normal={0},syntax={0},encoding={0},transaction={0},deadline={0};
  LIVE_CHECK(!p8_pipeline(conninfo,"SELECT 42",2000u,&normal));
  LIVE_CHECK(normal.sync_results==2 && normal.single_rows==1 && normal.tuples==1 && normal.reset_ok
      && normal.pipeline_off && normal.idle && normal.utf8 && !normal.errors);
  LIVE_CHECK(!p8_pipeline(conninfo,"SELECT FROM",2000u,&syntax));
  LIVE_CHECK(syntax.errors==1 && syntax.reset_ok && syntax.sync_results==2 && syntax.pipeline_off && syntax.idle && syntax.utf8);
  LIVE_CHECK(!ku_pg_sql_has_explicit_session_control(ku_test_text("SELECT set_config('client_encoding','LATIN1',false)"),ku_pg_now_ms()+2000u));
  LIVE_CHECK(!p8_pipeline(conninfo,"SELECT set_config('client_encoding','LATIN1',false)",2000u,&encoding));
  LIVE_CHECK(encoding.queued==2 && encoding.reset_ok && encoding.pipeline_off && encoding.idle && encoding.utf8);
  LIVE_CHECK(ku_pg_sql_has_explicit_session_control(ku_test_text("BEGIN"),ku_pg_now_ms()+2000u)==1);
  LIVE_CHECK(!p8_pipeline(conninfo,"BEGIN",2000u,&transaction));
  LIVE_CHECK(transaction.errors==1 && !transaction.reset_ok && !transaction.idle && transaction.pipeline_off && transaction.closed);
  LIVE_CHECK(!p8_pipeline(conninfo,"SELECT pg_sleep(0.6)",100u,&deadline));LIVE_CHECK(deadline.expired && deadline.closed);
  printf("{\"pg_pipeline_contract\":{\"default_changed\":false,\"strict_default_equivalent\":false,\"scope\":\"isolated protocol experiment; no CPU/performance claim\",\"original_async_encoding\":{\"result_retained\":%d,\"lease_evicted\":%d,\"reset_commands\":%d},",retained,evicted,resets);
  p8_trace_json("normal",&normal,0);p8_trace_json("syntax_error",&syntax,1);p8_trace_json("encoding_change",&encoding,1);
  p8_trace_json("explicit_transaction_raw_probe",&transaction,1);p8_trace_json("deadline",&deadline,1);
  puts(",\"limits\":{\"commands_per_lease\":2,\"results\":32,\"no_statement_replay\":true,\"unknown_write_not_rollback\":true}}}");
  puts("pg finite pipeline conditional-reset contract closed loop");return 0;
}
"#;

const PG_THREAD_LIFECYCLE_MAIN: &str = r#"
typedef struct KuTestPgActiveWorker {
  KuPgClient* client;
  KuTestEvent acquired;
  KuTestEvent release;
} KuTestPgActiveWorker;

static int ku_test_pg_active_worker(void* raw) {
  KuTestPgActiveWorker* worker = (KuTestPgActiveWorker*)raw;
  PGconn* connection = NULL;
  KuError error = {0};
  int slot = ku_pg_client_acquire(
      worker->client, &connection, &error, clock_ms + 60000ULL);
  if (slot < 0 || !connection) {
    ku_error_drop(&error);
    return 10;
  }
  if (!ku_test_event_set(&worker->acquired)) {
    ku_pg_client_release(worker->client, slot, 1, clock_ms + 60000ULL);
    return 11;
  }
  int released = ku_test_event_wait(&worker->release, 5000);
  ku_pg_client_release(worker->client, slot, 0, clock_ms + 60000ULL);
  return released ? 0 : 12;
}

static int ku_test_pg_waiter_worker(void* raw) {
  KuPgClient* client = (KuPgClient*)raw;
  PGconn* connection = NULL;
  KuError error = {0};
  int slot = ku_pg_client_acquire(
      client, &connection, &error, clock_ms + 60000ULL);
  if (slot >= 0 || connection) {
    if (slot >= 0) {
      ku_pg_client_release(client, slot, 1, clock_ms + 60000ULL);
    }
    return 20;
  }
  int closed = equals(error.code, "client_closed");
  ku_error_drop(&error);
  return closed ? 0 : 21;
}

static int ku_test_pg_close_worker(void* raw) {
  ku_pg_client_close_owned((KuPgClient*)raw);
  return 0;
}

static int ku_test_pg_wait_for_registered_waiter(
    KuPgClient* client, unsigned long timeout_ms) {
  unsigned long long deadline = ku_test_real_now_ms() + timeout_ms;
  for (;;) {
    ku_pg_mutex_lock(&client->lock);
    int registered = client->waiters == 1 && client->active == 1;
    ku_pg_mutex_unlock(&client->lock);
    if (registered) return 1;
    if (ku_test_real_now_ms() >= deadline) return 0;
    ku_test_thread_yield();
  }
}

int main(void) {
  KuResult_pg_client opened = ku_pg_client_open(
      text("hostaddr=127.0.0.1"), 1, 1, 5000, 60000, 60000);
  CHECK(opened.ok && opened.value);
  KuPgClient* client = opened.value;
  KuTestPgActiveWorker active = {0};
  active.client = client;
  CHECK(ku_test_event_init(&active.acquired));
  CHECK(ku_test_event_init(&active.release));

  KuTestThread active_thread = {0};
  KuTestThread waiter_thread = {0};
  KuTestThread close_thread = {0};
  CHECK(ku_test_thread_start(
      &active_thread, ku_test_pg_active_worker, &active));
  CHECK(ku_test_event_wait(&active.acquired, 3000));
  CHECK(ku_test_thread_start(
      &waiter_thread, ku_test_pg_waiter_worker, client));
  CHECK(ku_test_pg_wait_for_registered_waiter(client, 3000));
  CHECK(ku_test_thread_start(
      &close_thread, ku_test_pg_close_worker, client));

  /* The borrower remains gated here. close() and the registered waiter must
     both finish without waiting for that borrower to release its lease. */
  CHECK(ku_test_event_wait(&close_thread.finished, 3000));
  CHECK(ku_test_event_wait(&waiter_thread.finished, 3000));
  CHECK(close_thread.outcome == 0 && waiter_thread.outcome == 0);
  CHECK(ku_test_pg_load_dispose_calls() == 0);
  CHECK(live_connections == 1 && starts == 1 && finishes == 0);

  CHECK(ku_test_event_set(&active.release));
  CHECK(ku_test_thread_join(&close_thread, 3000));
  CHECK(ku_test_thread_join(&waiter_thread, 3000));
  CHECK(ku_test_thread_join(&active_thread, 3000));
  CHECK(active_thread.outcome == 0);
  CHECK(ku_test_event_destroy(&active.acquired));
  CHECK(ku_test_event_destroy(&active.release));
  CHECK(ku_test_pg_load_dispose_calls() == 1);
  CHECK(!live_connections && starts == finishes && !bad_calls);

  /* Both raw close calls begin while the allocation is live. The first owns
     finalization and pauses before destruction; the second must return without
     selecting a second disposer. This does not permit close-after-free. */
  KuResult_pg_client double_opened = ku_pg_client_open(
      text("hostaddr=127.0.0.1"), 1, 1, 5000, 60000, 60000);
  CHECK(double_opened.ok && double_opened.value);
  KuPgClient* double_client = double_opened.value;
  CHECK(ku_test_event_init(&ku_test_pg_dispose_entered));
  CHECK(ku_test_event_init(&ku_test_pg_dispose_release));
  ku_test_pg_block_dispose = 1;
  KuTestThread first_close = {0};
  KuTestThread second_close = {0};
  CHECK(ku_test_thread_start(
      &first_close, ku_test_pg_close_worker, double_client));
  CHECK(ku_test_event_wait(&ku_test_pg_dispose_entered, 3000));
  CHECK(ku_test_thread_start(
      &second_close, ku_test_pg_close_worker, double_client));
  CHECK(ku_test_event_wait(&second_close.finished, 3000));
  CHECK(second_close.outcome == 0 && ku_test_pg_load_dispose_calls() == 2);
  CHECK(ku_test_event_set(&ku_test_pg_dispose_release));
  CHECK(ku_test_thread_join(&second_close, 3000));
  CHECK(ku_test_thread_join(&first_close, 3000));
  ku_test_pg_block_dispose = 0;
  CHECK(ku_test_event_destroy(&ku_test_pg_dispose_entered));
  CHECK(ku_test_event_destroy(&ku_test_pg_dispose_release));
  CHECK(ku_test_pg_load_dispose_calls() == 2);
  CHECK(!live_connections && starts == finishes && !bad_calls);
  puts("pg real-thread lifecycle closed loop");
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
  T_BINARY_LATE, T_BINARY_TERMINAL, T_MIXED_FORMAT, T_UNKNOWN_FORMAT, T_CANCELLED
};
struct pg_conn {
  int id, status, utf8, nonblocking, active, mode, index, total, pending_read;
  int received_reads, notice_consumed, interruption, shutdowns, single_row, mode_window;
  int is_reset, transaction_status, transaction_after_query;
};
struct pg_result { int status, mode, number; };
static PGconn* connections[128];
static unsigned long long clock_ms = 1000, clock_step = 0, test_deadline = 0;
static int connect_attempts = 0, starts = 0, finishes = 0, sends = 0, param_sends = 0, clears = 0;
static int live_results = 0, live_connections = 0, shutdowns = 0, bad_calls = 0;
static int both_waits = 0, read_waits = 0, get_gate = 0, restore_mode = T_RESTORE;
static int mode_on_send = T_OK, fail_nonblocking = 0, parameter_checks = 0;
static int reset_mode_on_send = T_RESTORE, reset_sends = 0, user_sends = 0;
static int next_user_transaction_status = KU_PQTRANS_IDLE;
static int expire_encoding_observation = 0, connect_elapsed = 0;
static int fail_connect_on_attempt = 0, bad_connect_on_attempt = 0;
static int poisoned_consumes = 0;
static int cancellation_mode, cancel_tokens, cancel_sends, cancel_frees;
struct pg_cancel { PGconn* connection; };
static struct pg_cancel cancel_token;
static size_t fail_malloc_size = 0;
static int injected_allocation_failures = 0;
static int fail_runtime_allocation_after = 0, single_row_calls = 0, read_ahead_calls = 0;
static int track_runtime_allocations = 0, live_runtime_allocations = 0;
static size_t runtime_allocation_calls = 0;
static int expire_oom_after_shutdown = 0;
#define KU_PG_TEST_ALLOCATION_RECORDS 16
static struct { void* pointer; size_t size; } runtime_allocations[KU_PG_TEST_ALLOCATION_RECORDS];
static size_t runtime_bytes = 0, runtime_peak_bytes = 0;
static unsigned long long last_wait_budget = 0;
static KuPgClient* reentrant_connect_pool = 0;
static int reentrant_connect_checked = 0, reentrant_connect_violation = 0;
#define CHECK(value) do { if (!(value)) { fprintf(stderr, "check failed at %d: %s\n", __LINE__, #value); return 1; } } while (0)
static int runtime_allocation_index(void* pointer) {
  if (pointer) for (int i = 0; i < KU_PG_TEST_ALLOCATION_RECORDS; i++) if (runtime_allocations[i].pointer == pointer) return i;
  return -1;
}
static void record_runtime_allocation(void* pointer, size_t size) {
  if (!pointer || !track_runtime_allocations) return;
  for (int i = 0; i < KU_PG_TEST_ALLOCATION_RECORDS; i++) if (!runtime_allocations[i].pointer) {
    runtime_allocations[i].pointer = pointer; runtime_allocations[i].size = size;
    live_runtime_allocations++; runtime_bytes += size;
    if (runtime_bytes > runtime_peak_bytes) runtime_peak_bytes = runtime_bytes;
    return;
  }
  bad_calls++;
}
static void* ku_test_pg_malloc(size_t size) {
  runtime_allocation_calls++;
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
  connect_attempts++;
  if (reentrant_connect_pool && !reentrant_connect_checked) {
    reentrant_connect_checked = 1;
    int attempts_before_competitor = connect_attempts;
    PGconn* competitor = 0; KuError competitor_error = {0};
    int competitor_slot = ku_pg_client_acquire(
        reentrant_connect_pool, &competitor, &competitor_error, clock_ms + 1);
    if (competitor_slot >= 0 || competitor || connect_attempts != attempts_before_competitor) {
      reentrant_connect_violation++;
    }
    if (competitor_slot >= 0) {
      ku_pg_client_release(reentrant_connect_pool, competitor_slot, 1, clock_ms + 1);
    } else {
      ku_error_drop(&competitor_error);
    }
  }
  if (fail_connect_on_attempt > 0 && --fail_connect_on_attempt == 0) return 0;
  int bad_connect = bad_connect_on_attempt > 0 && --bad_connect_on_attempt == 0;
  if (starts >= 127) { bad_calls++; return 0; }
  PGconn* conn = (PGconn*)calloc(1, sizeof(PGconn));
  if (!conn) abort();
  conn->id = ++starts; conn->status = bad_connect ? KU_PG_CONNECTION_BAD : KU_PG_CONNECTION_OK;
  conn->utf8 = 1; connections[conn->id] = conn; live_connections++;
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
/* Existing modes simulate unavailable cancellation; explicit modes exercise
   confirmed cancellation, acknowledgement without retirement, and send failure. */
int PQisthreadsafe(void) { return 1; }
int PQsslInUse(PGconn* connection) { (void)connection; return 0; }
PGcancel* PQgetCancel(PGconn* connection) {
  if (!cancellation_mode) return NULL;
  if (cancel_tokens) bad_calls++;
  cancel_tokens++; cancel_token.connection=connection; return &cancel_token;
}
int PQcancel(PGcancel* cancel, char* error, int length) {
  (void)error; (void)length; cancel_sends++;
  if (ku_database_budget_snapshot().pending_operations != 1) bad_calls++;
  if (cancellation_mode==3) return 0;
  if (cancellation_mode==1) {
    cancel->connection->mode=T_CANCELLED;
    cancel->connection->index=0; cancel->connection->total=1;
  }
  return 1;
}
void PQfreeCancel(PGcancel* cancel) {
  if (cancel!=&cancel_token || cancel_tokens!=1) bad_calls++;
  cancel_tokens--; cancel_frees++;
}
void PQfinish(PGconn* conn) {
  if (!conn) return;
  if (ku_test_pg_pool_lock_depth != 0) ku_test_pg_finishes_while_pool_locked++;
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
  if (conn->mode == T_CANCELLED) result->status = KU_PGRES_FATAL_ERROR;
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
unsigned int PQftype(const PGresult* result,int column) { (void)result;(void)column;return 25u; }
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
static int invalid_pg_config_rejected_without_side_effects(KuObject* config) {
  int before_connect_attempts = connect_attempts, before_starts = starts;
  int before_finishes = finishes, before_sends = sends, before_clears = clears;
  int before_single_row_calls = single_row_calls;
  int before_acquire_calls = ku_test_pg_acquire_calls;
  int before_allocation_failures = injected_allocation_failures;
  int before_live_allocations = live_runtime_allocations;
  size_t before_runtime_bytes = runtime_bytes;
  fail_runtime_allocation_after = 1;
  track_runtime_allocations = 1;
  KuResult_pg_client rejected = ku_pg_client(config);
  track_runtime_allocations = 0;
  int valid = !rejected.ok && !rejected.value
      && equals(rejected.error.domain, "pg")
      && equals(rejected.error.code, "invalid_config")
      && rejected.error.domain.storage == KU_STRING_STATIC
      && rejected.error.code.storage == KU_STRING_STATIC
      && rejected.error.message.storage == KU_STRING_STATIC
      && fail_runtime_allocation_after == 1
      && injected_allocation_failures == before_allocation_failures
      && live_runtime_allocations == before_live_allocations
      && runtime_bytes == before_runtime_bytes
      && connect_attempts == before_connect_attempts && starts == before_starts
      && finishes == before_finishes && sends == before_sends
      && clears == before_clears && single_row_calls == before_single_row_calls;
  valid = valid && ku_test_pg_acquire_calls == before_acquire_calls;
  fail_runtime_allocation_after = 0;
  if (!rejected.ok) ku_error_drop(&rejected.error);
  return valid;
}
static PGconn* open_connection(void) {
  KuPgConnectAttempt result = ku_pg_connect_until("hostaddr=127.0.0.1", clock_ms + 10000);
  if (!result.conn) abort();
  return result.conn;
}
static void close_connection(PGconn** connection) { if (connection && *connection) { PQfinish(*connection); *connection = 0; } }

static int integer_value_cases(void) {
  const struct {const char* text;size_t length;int ok;int64_t value;} cases[]={
    {"0",1,1,0},{"-0",2,1,0},{"1",1,1,1},{"-1",2,1,-1},
    {"2147483647",10,1,INT64_C(2147483647)},{"-2147483648",11,1,-INT64_C(2147483648)},
    {"9223372036854775807",19,1,INT64_MAX},{"-9223372036854775808",20,1,INT64_MIN},
    {"",0,0,0},{"-",1,0,0},{"+1",2,0,0},{" 1",2,0,0},{"1 ",2,0,0},
    {"00",2,0,0},{"01",2,0,0},{"-00",3,0,0},{"1.0",3,0,0},{"1e0",3,0,0},
    {"1\0x",3,0,0},{"\0",1,0,0},{"null",4,0,0},{"true",4,0,0},{"\xe4\xb8\xad",3,0,0},
    {"9223372036854775808",19,0,0},{"-9223372036854775809",20,0,0},
    {"18446744073709551615",20,0,0},{"100000000000000000000",21,0,0}
  };
  for(size_t i=0;i<sizeof(cases)/sizeof(cases[0]);i++) {
    track_runtime_allocations=1;
    KuPgResult* owned=(KuPgResult*)ku_test_pg_malloc(sizeof(*owned));CHECK(owned);
    *owned=(KuPgResult){0};owned->rows=owned->cols=owned->cell_capacity=1;
    owned->bytes_len=cases[i].length;owned->bytes_capacity=cases[i].length?cases[i].length:1;
    owned->cells=(KuPgCell*)ku_test_pg_calloc(1,sizeof(*owned->cells));
    owned->bytes=(uint8_t*)ku_test_pg_malloc(owned->bytes_capacity);CHECK(owned->cells && owned->bytes);
    if(cases[i].length)memcpy(owned->bytes,cases[i].text,cases[i].length);
    owned->cells[0]=(KuPgCell){0,(uint32_t)cases[i].length};
    KuPgResult before=*owned;KuPgCell cell=owned->cells[0];
    size_t bytes=runtime_bytes;int allocations=live_runtime_allocations,failures=injected_allocation_failures;
    fail_runtime_allocation_after=1;
    KuResult_int parsed=ku_pg_value_int(owned,0,0);
    CHECK(parsed.ok==cases[i].ok && (!parsed.ok || parsed.value==cases[i].value));
    if(!parsed.ok)CHECK(equals(parsed.error.domain,"pg") && equals(parsed.error.code,"integer_value_error"));
    ku_error_drop(&parsed.error);
    CHECK(fail_runtime_allocation_after==1 && injected_allocation_failures==failures
        && allocations==live_runtime_allocations && runtime_bytes==bytes);
    CHECK(!memcmp(&before,owned,sizeof(before)) && !memcmp(&cell,owned->cells,sizeof(cell))
        && (!cases[i].length || !memcmp(owned->bytes,cases[i].text,cases[i].length)));
    fail_runtime_allocation_after=0;
    ku_drop_pg_result(&owned);CHECK(!owned && !live_runtime_allocations && !runtime_bytes);
    if(parsed.ok)CHECK(parsed.value==cases[i].value); /* Copy value survives the result owner. */
    parsed=ku_pg_value_int(owned,0,0);
    CHECK(!parsed.ok && equals(parsed.error.code,"value_error"));ku_error_drop(&parsed.error);
    track_runtime_allocations=0;
  }
  uint8_t digit='7';KuPgCell cell={0,1};
  for(unsigned bad=0;bad<11u;bad++) {
    KuPgResult table={1,1,1,1,1,&cell,&digit};cell=(KuPgCell){0,1};
    int64_t row=0,col=0;
    switch(bad) {
      case 0:row=-1;break;case 1:col=-1;break;case 2:row=1;break;case 3:col=INT64_MAX;break;
      case 4:table.cells=NULL;break;case 5:table.cell_capacity=0;break;
      case 6:table.bytes=NULL;break;case 7:table.bytes_len=2;break;
      case 8:cell.offset=UINT32_MAX;break;case 9:cell.len=2;break;
      default:table.rows=3;table.cols=SIZE_MAX;row=2;break;
    }
    fail_runtime_allocation_after=1;
    KuResult_int parsed=ku_pg_value_int(&table,row,col);
    CHECK(!parsed.ok && equals(parsed.error.code,"value_error") && fail_runtime_allocation_after==1);
    ku_error_drop(&parsed.error);fail_runtime_allocation_after=0;
  }
  KuPgResult table={1,1,1,0,0,&cell,NULL};cell=(KuPgCell){UINT32_MAX,KU_PG_NULL_CELL};
  fail_runtime_allocation_after=1;
  KuResult_int null=ku_pg_value_int(&table,0,0);
  CHECK(!null.ok && equals(null.error.code,"null_value") && fail_runtime_allocation_after==1);
  ku_error_drop(&null.error);fail_runtime_allocation_after=0;
  cell=(KuPgCell){0,0};KuResult_int empty=ku_pg_value_int(&table,0,0);
  CHECK(!empty.ok && equals(empty.error.code,"integer_value_error"));ku_error_drop(&empty.error);
  return 0;
}
int main(void) {
  CHECK(integer_value_cases()==0);
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
  CHECK(!comparison.ok && equals(comparison.error.domain, "pg") && equals(comparison.error.code, "out_of_memory")
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
  /* The common compact result now has one actual allocation. External
   * cells/bytes growth failures are exercised by ASYNC_COMPACT_RESULT_SUITE;
   * old ordinal 2/3 no longer names a real allocation in this small query. */
  const int allocation_modes[] = { T_OOM_RESULT };
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
  CHECK(invalid_pg_config_rejected_without_side_effects(0));
  KuObject* invalid_config = ku_object_new(0);
  CHECK(invalid_pg_config_rejected_without_side_effects(invalid_config));
  ku_object_drop(invalid_config);
  invalid_config = pg_config_with_conninfo(ku_v_int(1));
  CHECK(invalid_pg_config_rejected_without_side_effects(invalid_config));
  ku_object_drop(invalid_config);
  invalid_config = pg_config_with_conninfo(ku_v_str(text("hostaddr=127.0.0.1")));
  ku_object_set(invalid_config, text("unknown"), ku_v_int(1));
  CHECK(invalid_pg_config_rejected_without_side_effects(invalid_config));
  ku_object_drop(invalid_config);
  invalid_config = pg_config_with_conninfo(ku_v_str(text("hostaddr=127.0.0.1")));
  ku_object_set(invalid_config, text("max_connections"), ku_v_int(0));
  CHECK(invalid_pg_config_rejected_without_side_effects(invalid_config));
  ku_object_drop(invalid_config);
  invalid_config = pg_config_with_conninfo(ku_v_str(text("hostaddr=127.0.0.1")));
  ku_object_set(invalid_config, text("query_timeout_ms"), ku_v_str(text("fast")));
  CHECK(invalid_pg_config_rejected_without_side_effects(invalid_config));
  ku_object_drop(invalid_config);
  uint8_t embedded_nul[] = { 'h', 'o', 's', 't', 0, 'x' };
  invalid_config = pg_config_with_conninfo(ku_v_str((KuString){ embedded_nul, sizeof(embedded_nul), 0, KU_STRING_STATIC }));
  CHECK(invalid_pg_config_rejected_without_side_effects(invalid_config)
      && starts == before_invalid_config_starts);
  ku_object_drop(invalid_config);
  KuString oversized_conninfo = { (uint8_t*)"x", (size_t)KU_PG_MAX_CONNINFO_BYTES + 1, 0, KU_STRING_STATIC };
  invalid_config = pg_config_with_conninfo(ku_v_str(oversized_conninfo));
  CHECK(invalid_pg_config_rejected_without_side_effects(invalid_config));
  ku_object_drop(invalid_config);
  KuResult_pg_client invalid_client = ku_pg_client_open(oversized_conninfo, 1, 64, 5000, 5000, 30000);
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
  int before_session_sends = sends, before_session_connects = connect_attempts;
  int before_session_acquires = ku_test_pg_acquire_calls;
  /* Dialect rules are intentionally separate. PostgreSQL does not treat `#`
     as a line comment, while MySQL executable comments are ordinary nested
     block comments here. Only an explicit top-level control token is blocked. */
  CHECK(ku_pg_sql_has_explicit_session_control(
      text(" # not a PostgreSQL comment\nSET ROLE app"), clock_ms + 50) == 0);
  CHECK(ku_pg_sql_has_explicit_session_control(
      text("/*!40101 SET search_path = hidden */ SELECT 1"), clock_ms + 50) == 0);
  CHECK(ku_pg_sql_has_explicit_session_control(
      text("/*M!100100 SET search_path = hidden */ SELECT 1"), clock_ms + 50) == 0);
  CHECK(ku_pg_sql_has_explicit_session_control(
      text("/*! harmless in PostgreSQL */ SET ROLE app"), clock_ms + 50) == 1);
  result = ku_pg_client_query(
      pool, text(" /* outer /* nested */ */ SeT ROLE app"), empty_params);
  CHECK(!result.ok && equals(result.error.code, "session_state_unsupported")
      && sends == before_session_sends && connect_attempts == before_session_connects);
  ku_error_drop(&result.error);
  result = ku_pg_client_query(
      pool, text("\v \t\r\n\fSET ROLE app"), empty_params);
  CHECK(!result.ok && equals(result.error.code, "session_state_unsupported")
      && sends == before_session_sends && connect_attempts == before_session_connects
      && ku_test_pg_acquire_calls == before_session_acquires);
  ku_error_drop(&result.error);
  result = ku_pg_client_query(
      pool, text("LOAD 'untrusted_backend_library'"), empty_params);
  CHECK(!result.ok && equals(result.error.code, "session_state_unsupported")
      && sends == before_session_sends && connect_attempts == before_session_connects);
  ku_error_drop(&result.error);
  result = ku_pg_client_query(
      pool, text("CREATE GLOBAL TEMPORARY TABLE transient_a(id int)"), empty_params);
  CHECK(!result.ok && equals(result.error.code, "session_state_unsupported")
      && sends == before_session_sends && connect_attempts == before_session_connects);
  ku_error_drop(&result.error);
  result = ku_pg_client_query(
      pool, text("CREATE LOCAL TEMP TABLE transient_b(id int)"), empty_params);
  CHECK(!result.ok && equals(result.error.code, "session_state_unsupported")
      && sends == before_session_sends && connect_attempts == before_session_connects);
  ku_error_drop(&result.error);
  result = ku_pg_client_query(
      pool, text("CREATE OR REPLACE TEMP VIEW transient_v AS SELECT 1"), empty_params);
  CHECK(!result.ok && equals(result.error.code, "session_state_unsupported")
      && sends == before_session_sends && connect_attempts == before_session_connects);
  ku_error_drop(&result.error);
  result = ku_pg_client_query(pool, text("/* unclosed"), empty_params);
  CHECK(!result.ok && equals(result.error.code, "session_state_unsupported")
      && sends == before_session_sends && connect_attempts == before_session_connects);
  ku_error_drop(&result.error);

  /* Input validity and the 16 MiB cap precede policy scanning. A hostile ABI
     caller cannot turn an oversized, invalid or NUL-containing string into a
     full preflight walk, a pool acquisition, or a send. */
  KuString preflight_oversized_sql = { (uint8_t*)" ", (size_t)KU_PG_MAX_SQL_BYTES + 1, 0, KU_STRING_STATIC };
  result = ku_pg_client_query(pool, preflight_oversized_sql, empty_params);
  CHECK(!result.ok && equals(result.error.code, "query_too_large")
      && sends == before_session_sends && connect_attempts == before_session_connects);
  ku_error_drop(&result.error);
  uint8_t nul_then_set[] = { ' ', 0, 'S', 'E', 'T', ' ', 'R', 'O', 'L', 'E' };
  result = ku_pg_client_query(
      pool, (KuString){ nul_then_set, sizeof(nul_then_set), 0, KU_STRING_STATIC }, empty_params);
  CHECK(!result.ok && equals(result.error.code, "query_error")
      && sends == before_session_sends && connect_attempts == before_session_connects);
  ku_error_drop(&result.error);
  uint8_t invalid_utf8_then_set[] = { 0xff, 'S', 'E', 'T', ' ', 'R', 'O', 'L', 'E' };
  result = ku_pg_client_query(
      pool, (KuString){ invalid_utf8_then_set, sizeof(invalid_utf8_then_set), 0, KU_STRING_STATIC }, empty_params);
  CHECK(!result.ok && equals(result.error.code, "invalid_utf8")
      && sends == before_session_sends && connect_attempts == before_session_connects);
  ku_error_drop(&result.error);
  CHECK(pool->active == 0 && pool->waiters == 0 && !pool->in_use[0]
      && pool->conns[0] != 0 && !pool->closing
      && ku_test_pg_acquire_calls == before_session_acquires);

  /* Scanner work itself has both a byte cap (validated by the caller) and an
     absolute deadline checked at most 4096 input bytes apart. Exercise long
     whitespace and a long nested-comment prefix directly so the test isolates
     policy scanning from the preceding UTF-8 validation pass. */
  size_t scan_len = 128 * 1024;
  uint8_t* scan_sql = (uint8_t*)malloc(scan_len);
  CHECK(scan_sql != 0);
  memset(scan_sql, ' ', scan_len);
  unsigned long long scan_started = clock_ms;
  clock_step = 1;
  CHECK(ku_pg_sql_has_explicit_session_control(
      (KuString){ scan_sql, scan_len, scan_len, KU_STRING_OWNED }, scan_started + 4) == -2
      && clock_ms <= scan_started + 5);
  clock_step = 0;
  memset(scan_sql, ' ', scan_len);
  scan_sql[0] = '/'; scan_sql[1] = '*';
  scan_started = clock_ms; clock_step = 1;
  CHECK(ku_pg_sql_has_explicit_session_control(
      (KuString){ scan_sql, scan_len, scan_len, KU_STRING_OWNED }, scan_started + 4) == -2
      && clock_ms <= scan_started + 5);
  clock_step = 0;
  free(scan_sql);
  int before_handoff_signals = ku_test_pg_signal_calls;
  pool->waiters = 1;
  ku_pg_client_handoff_available_locked(pool);
  CHECK(ku_test_pg_signal_calls == before_handoff_signals + 1);
  pool->waiters = 0;

  int before_timer_handoff_signals = ku_test_pg_signal_calls;
  pool->waiters = 1; /* another waiter remains after the timer owner wakes */
  pool->backoff_timer_armed = 1;
  ku_pg_client_release_backoff_timer_locked(pool, 1);
  CHECK(!pool->backoff_timer_armed
      && ku_test_pg_signal_calls == before_timer_handoff_signals + 1);
  pool->waiters = 0;

  pool->in_use[0] = 1; pool->active = 1; ku_test_pg_wait_failure = 1;
  PGconn* sync_connection = 0; KuError sync_error = {0};
  CHECK(ku_pg_client_acquire(pool, &sync_connection, &sync_error, clock_ms + 50) < 0
      && !sync_connection && equals(sync_error.code, "sync_error")
      && !ku_test_pg_wait_failure && pool->waiters == 0);
  ku_error_drop(&sync_error); pool->in_use[0] = 0; pool->active = 0;

  /* A thread that has not waited must never barge ahead of an already queued
     borrower. Cover both forms of immediately available capacity: an idle
     connection and a lazy-connect empty slot. The synthetic existing waiter
     is enough to make this deterministic without scheduler timing. */
  pool->waiters = 1;
  PGconn* barged = 0; KuError barged_error = {0}; int before_attempts = connect_attempts;
  CHECK(ku_pg_client_acquire(pool, &barged, &barged_error, clock_ms + 1) < 0
      && !barged && equals(barged_error.code, "acquire_timeout")
      && pool->waiters == 1 && pool->active == 0 && !pool->in_use[0]
      && pool->conns[0] && connect_attempts == before_attempts);
  ku_error_drop(&barged_error);
  PGconn* removed_idle = pool->conns[0]; pool->conns[0] = 0; PQfinish(removed_idle);
  barged_error = (KuError){0}; before_attempts = connect_attempts;
  CHECK(ku_pg_client_acquire(pool, &barged, &barged_error, clock_ms + 1) < 0
      && !barged && equals(barged_error.code, "acquire_timeout")
      && pool->waiters == 1 && pool->active == 0 && !pool->in_use[0]
      && !pool->conns[0] && connect_attempts == before_attempts);
  ku_error_drop(&barged_error); pool->waiters = 0;

  PGconn* held = 0; KuError held_error = {0};
  int held_slot = ku_pg_client_acquire(pool, &held, &held_error, clock_ms + 50);
  CHECK(held_slot == 0 && held && pool->active == 1 && pool->waiters == 0);
  pool->max_waiters = 0; PGconn* rejected = 0; KuError rejected_error = {0};
  CHECK(ku_pg_client_acquire(pool, &rejected, &rejected_error, clock_ms + 50) < 0
      && !rejected && equals(rejected_error.code, "pool_busy")
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

  /* A transaction/session that is not IDLE must fail loudly: returning a
     success that is immediately rolled back would violate the client API. */
  next_user_transaction_status = 2; before_resets = reset_sends;
  result = ku_pg_client_query(pool, text("SELECT leaves_transaction_open"), empty_params);
  CHECK(!result.ok && equals(result.error.code, "session_state_unsupported")
      && equals(result.error.message, "PostgreSQL statement completed or may have completed; session state is unsupported and its payload was discarded; never retry automatically")
      && reset_sends == before_resets && !pool->conns[0] && pool->active == 0);
  ku_error_drop(&result.error);

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
  KuPgDiagnosticSnapshot* sync_timeout_sample=ku_pg_diag_latest_locked();
  CHECK(sync_timeout_sample->sync_identity_missing && sync_timeout_sample->task_phase_missing && sync_timeout_sample->phase==KU_PG_ASYNC_ACQUIRE);
  ku_error_drop(&acquire_error);

  mode_on_send = T_READ_TIMEOUT; __ku_handler_deadline = clock_ms + 7;
  result = ku_pg_client_query(pool, text("SELECT 1"), empty_params); __ku_handler_deadline = 0;
  CHECK(error_is(result, "execution_unknown") && !pool->conns[0]); ku_error_drop(&result.error);
  connect_elapsed = 3; __ku_handler_deadline = clock_ms + 7;
  result = ku_pg_client_query(pool, text("SELECT 1"), empty_params); __ku_handler_deadline = 0; connect_elapsed = 0;
  CHECK(error_is(result, "execution_unknown") && last_wait_budget == 4 && !pool->conns[0] && pool->active == 0); ku_error_drop(&result.error);

  /* close() linearizes before this active borrower releases. Release must skip
     DISCARD ALL, detach under the mutex, finish outside it, and perform the
     deferred pool disposal exactly once. */
  PGconn* closing_connection = 0; KuError closing_error = {0};
  int closing_slot = ku_pg_client_acquire(
      pool, &closing_connection, &closing_error, clock_ms + 50);
  CHECK(closing_slot == 0 && closing_connection && pool->active == 1);
  before_resets = reset_sends; int before_close_finishes = finishes;
  KuPgClient* closing_pool = pool; pool = 0;
  ku_pg_client_close_owned(closing_pool);
  ku_pg_client_release(closing_pool, closing_slot, 0, clock_ms + 50);
  CHECK(reset_sends == before_resets && finishes == before_close_finishes + 1);
  CHECK(!pool && live_connections == 0 && live_results == 0);

  /* Closing an idle healthy pool retires its physical connection once, without
     classifying normal retirement as an unusable/broken connection. */
  KuResult_pg_client idle_retire = ku_pg_client_open(
      text("hostaddr=127.0.0.1"), 1, 0, 5000, 5000, 30000);
  CHECK(idle_retire.ok && idle_retire.value->active == 0);
  KuDatabaseBudgetSnapshot before_idle_retire = ku_database_budget_snapshot();
  int idle_retire_finishes = finishes;
  ku_drop_pg_client(&idle_retire.value);
  KuDatabaseBudgetSnapshot after_idle_retire = ku_database_budget_snapshot();
  CHECK(!idle_retire.value && finishes == idle_retire_finishes + 1);
  CHECK(after_idle_retire.pg_client_discarded_connections
      == before_idle_retire.pg_client_discarded_connections + 1);
  CHECK(after_idle_retire.pg_client_broken_discards
      == before_idle_retire.pg_client_broken_discards);

  /* Failed connects publish a bounded retry window. Requests whose entire
     budget lies inside that window must not redial. Once fake time advances
     beyond the cap, only one borrower may run the half-open recovery probe,
     even when another empty slot could otherwise trigger a second dial. */
  KuResult_pg_client flight_opened = ku_pg_client_open(
      text("hostaddr=127.0.0.1"), 2, 64, 5000, 5000, 30000);
  CHECK(flight_opened.ok); KuPgClient* flight_pool = flight_opened.value;
  PGconn* flight_initial = flight_pool->conns[0]; flight_pool->conns[0] = 0;
  PQfinish(flight_initial);
  /* The earlier phase budget owns the timeout code. A short acquire budget is
     not mislabeled as connect_timeout; a shorter connect budget remains a
     connect_timeout. Both paths evict the unfinished physical connection. */
  connect_elapsed = 7;
  PGconn* budget_connection = 0; KuError budget_error = {0};
  CHECK(ku_pg_client_acquire(
          flight_pool, &budget_connection, &budget_error, clock_ms + 5) < 0
      && !budget_connection && equals(budget_error.code, "acquire_timeout"));
  sync_timeout_sample=ku_pg_diag_latest_locked();
  CHECK(sync_timeout_sample->sync_identity_missing && sync_timeout_sample->phase==KU_PG_ASYNC_CONNECT && sync_timeout_sample->active==1);
  ku_error_drop(&budget_error); clock_ms += 1001;
  flight_pool->connect_timeout_ms = 5; budget_error = (KuError){0};
  CHECK(ku_pg_client_acquire(
          flight_pool, &budget_connection, &budget_error, clock_ms + 5) < 0
      && !budget_connection && equals(budget_error.code, "acquire_timeout"));
  ku_error_drop(&budget_error); clock_ms += 1001;
  flight_pool->connect_timeout_ms = 5; budget_error = (KuError){0};
  CHECK(ku_pg_client_acquire(
          flight_pool, &budget_connection, &budget_error, clock_ms + 50) < 0
      && !budget_connection && equals(budget_error.code, "connect_timeout"));
  ku_error_drop(&budget_error); connect_elapsed = 0; clock_ms += 1001;
  flight_pool->connect_timeout_ms = 5000;
  fail_connect_on_attempt = 1;
  PGconn* flight_connection = 0; KuError flight_error = {0};
  int before_flight_attempts = connect_attempts;
  CHECK(ku_pg_client_acquire(
          flight_pool, &flight_connection, &flight_error, clock_ms + 50) < 0
      && !flight_connection && equals(flight_error.code, "out_of_memory")
      && connect_attempts == before_flight_attempts + 1);
  ku_error_drop(&flight_error); before_flight_attempts = connect_attempts;
  flight_error = (KuError){0};
  CHECK(ku_pg_client_acquire(
          flight_pool, &flight_connection, &flight_error, clock_ms + 1) < 0
      && !flight_connection && equals(flight_error.code, "acquire_timeout")
      && connect_attempts == before_flight_attempts);
  ku_error_drop(&flight_error); clock_ms += 1001;
  bad_connect_on_attempt = 1; flight_error = (KuError){0};
  CHECK(ku_pg_client_acquire(
          flight_pool, &flight_connection, &flight_error, clock_ms + 50) < 0
      && !flight_connection && equals(flight_error.code, "connect_error")
      && !bad_connect_on_attempt
      && connect_attempts == before_flight_attempts + 1);
  ku_error_drop(&flight_error); before_flight_attempts = connect_attempts; clock_ms += 1001;
  reentrant_connect_pool = flight_pool;
  flight_error = (KuError){0};
  int flight_slot = ku_pg_client_acquire(
      flight_pool, &flight_connection, &flight_error, clock_ms + 50);
  reentrant_connect_pool = 0;
  CHECK(flight_slot >= 0 && flight_connection && reentrant_connect_checked
      && !reentrant_connect_violation && connect_attempts == before_flight_attempts + 1);
  ku_pg_client_release(flight_pool, flight_slot, 1, clock_ms + 50);
  ku_drop_pg_client(&flight_pool);
  CHECK(!flight_pool && live_connections == 0 && live_results == 0 && starts == finishes);
  size_t confirmed_before=ku_database_budget_snapshot().confirmed_cancellations;
  for (cancellation_mode=1; cancellation_mode<=3; cancellation_mode++) {
    KuResult_pg_client cancellable=ku_pg_client_open(text("hostaddr=127.0.0.1"),1,1,5000,5000,20);
    CHECK(cancellable.ok && cancellable.value);
    mode_on_send=T_READ_TIMEOUT;
    int sends_before=user_sends, cancel_before=cancel_sends;
    KuResult_pg_result cancelled=ku_pg_client_query(cancellable.value,text("SELECT cancelled"),empty_params);
    CHECK(error_is(cancelled,"execution_unknown") && user_sends==sends_before+1);
    CHECK(cancel_sends==cancel_before+1 && !cancel_tokens && cancel_frees==cancel_sends);
    CHECK(cancellable.value->active==0 && !cancellable.value->conns[0]);
    CHECK(ku_database_budget_snapshot().pending_operations==0
        && ku_database_budget_snapshot().pending_bytes==0);
    CHECK(ku_database_budget_snapshot().confirmed_cancellations==confirmed_before+1);
    ku_error_drop(&cancelled.error);
    mode_on_send=T_OK;
    KuResult_pg_result after_cancel=ku_pg_client_query(cancellable.value,text("SELECT fresh_request"),empty_params);
    CHECK(after_cancel.ok && user_sends==sends_before+2);
    ku_drop_pg_result(&after_cancel.value); ku_drop_pg_client(&cancellable.value);
  }
  cancellation_mode=0;

  mode_on_send=T_MULTI;reset_mode_on_send=T_RESTORE;
  KuResult_int typed=ReadTyped();
  CHECK(typed.ok && typed.value==6 && ku_test_pg_index_calls==2
      && !live_connections && !live_results && starts==finishes);
  CHECK(ku_test_pg_pool_lock_depth == 0 && ku_test_pg_finishes_while_pool_locked == 0);
  CHECK(bad_calls == 0);
  CHECK(shutdowns > 0 && poisoned_consumes >= 2 && single_row_calls > 0 && read_ahead_calls == 0);
  CHECK(!live_runtime_allocations && !runtime_bytes);
  puts("pg query poll closed loop");
  return 0;
}
"#;

const ASYNC_QUERY_SUITE: &str = r#"
typedef struct AsyncBudget {size_t bytes,peak,reserves,zero_reserves,refunds,zero_refunds;int reject,wakes;} AsyncBudget;
static int async_reserve(void* context,size_t bytes) {
  AsyncBudget* b=(AsyncBudget*)context;
  b->reserves++;if(!bytes)b->zero_reserves++;
  if(b->reject)return 0;
  if(bytes>SIZE_MAX-b->bytes)abort();b->bytes+=bytes;if(b->bytes>b->peak)b->peak=b->bytes;return 1;
}
static void async_refund(void* context,size_t bytes) {
  AsyncBudget* b=(AsyncBudget*)context;b->refunds++;if(!bytes)b->zero_refunds++;if(bytes>b->bytes)abort();b->bytes-=bytes;
}
static void async_wake(void* context) {((AsyncBudget*)context)->wakes++;}
static KuPgAsyncOp* async_create(KuPgClient* p,AsyncBudget* b) {
  KuError e={0};KuPgAsyncOp* op=ku_pg_async_create(p,ku_string_static((const uint8_t*)"SELECT 3",8),
      (KuArray_str){0},clock_ms+2000,(KuPgAsyncBudget){b,async_reserve,async_refund},async_wake,b,&e);
  if(!op) {fprintf(stderr,"create failed: %.*s\n",(int)e.code.len,e.code.ptr);abort();}return op;
}
static int async_finish(KuPgAsyncOp* op,int cancel) {
  for(size_t i=0;i<1024;i++) {
    if(ku_pg_async_step(op,3u,clock_ms,cancel))return 1;
    if(op->phase==KU_PG_ASYNC_CONNECT && !op->connect_ready)ku_pg_async_connect_run(op,cancel);
  }
  return 0;
}
static void async_drop_output(KuPgAsyncOp* op,AsyncBudget* b,int expected_ok) {
  if(!ku_pg_async_can_dispose(op))abort();
  KuResult_pg_result result=ku_pg_async_take(op);if(result.ok!=expected_ok)abort();
  size_t bytes=0;if(result.ok){if(!ku_pg_result_owned_bytes(result.value,&bytes))abort();ku_drop_pg_result(&result.value);}else ku_error_drop(&result.error);
  ku_pg_async_dispose(op,1);if(bytes)async_refund(b,bytes);if(b->bytes)abort();
}
int main(void) {
  clock_ms=1000;clock_step=0;mode_on_send=T_OK;reset_mode_on_send=T_RESTORE;
  KuResult_pg_client captured=ku_pg_client_open(ku_string_static((const uint8_t*)"hostaddr=127.0.0.1",18),1,128,1000,1000,2000);
  CHECK(captured.ok && ku_pg_client_capture_retain(captured.value));
  ku_pg_client_close_owned(captured.value);CHECK(captured.value->closing && captured.value->capture_pins==1);
  AsyncBudget capture_budget={0};KuError capture_error={0};
  KuPgAsyncOp* rejected=ku_pg_async_create(captured.value,ku_string_static((const uint8_t*)"SELECT 3",8),(KuArray_str){0},clock_ms+2000,
      (KuPgAsyncBudget){&capture_budget,async_reserve,async_refund},async_wake,&capture_budget,&capture_error);
  CHECK(!rejected && !capture_budget.bytes);CHECK(ku_string_equal(capture_error.code,ku_string_static((const uint8_t*)"client_closed",13)));
  ku_error_drop(&capture_error);ku_pg_client_capture_release(captured.value);CHECK(live_connections==0);
  KuResult_pg_client opened=ku_pg_client_open(ku_string_static((const uint8_t*)"hostaddr=127.0.0.1",18),1,128,1000,1000,2000);
  CHECK(opened.ok);KuPgClient* p=opened.value;AsyncBudget b={0};
  int waits_before=both_waits+read_waits;
  KuPgAsyncOp* op=async_create(p,&b);CHECK(async_finish(op,0));CHECK(op->output.ok);
  CHECK(both_waits+read_waits==waits_before);CHECK(reset_sends==1);CHECK(p->active==0 && p->waiters==0);
  CHECK(op->output.value->rows==1);KuResult_str value=ku_pg_value(op->output.value,0,0);CHECK(value.ok && value.value.len);ku_string_drop(&value.value);
  async_drop_output(op,&b,1);
  mode_on_send=T_NULL_EMPTY_UTF8;op=async_create(p,&b);CHECK(async_finish(op,0));CHECK(op->output.ok && op->output.value->rows==2 && op->output.value->cols==3);
  KuResult_bool absent=ku_pg_is_null(op->output.value,0,0);CHECK(absent.ok && absent.value);async_drop_output(op,&b,1);
  mode_on_send=T_BAD_UTF8;op=async_create(p,&b);CHECK(async_finish(op,0));CHECK(!op->output.ok);async_drop_output(op,&b,0);
  mode_on_send=T_BINARY;op=async_create(p,&b);CHECK(async_finish(op,0));CHECK(!op->output.ok);async_drop_output(op,&b,0);
  mode_on_send=T_ERROR;op=async_create(p,&b);CHECK(async_finish(op,0));CHECK(!op->output.ok);async_drop_output(op,&b,0);
  mode_on_send=T_OK;op=async_create(p,&b);b.reject=1;CHECK(async_finish(op,0));CHECK(!op->output.ok);b.reject=0;async_drop_output(op,&b,0);
  mode_on_send=T_OK;op=async_create(p,&b);CHECK(async_finish(op,0));async_drop_output(op,&b,1);
  mode_on_send=T_READ_TIMEOUT;op=async_create(p,&b);CHECK(!ku_pg_async_step(op,0,clock_ms,0));CHECK(op->sent && op->interest==1u);
  CHECK(async_finish(op,1));CHECK(!op->output.ok);CHECK(ku_string_equal(op->output.error.code,ku_string_static((const uint8_t*)"execution_unknown",17)));
  CHECK(p->active==0 && p->conns[0]==NULL);async_drop_output(op,&b,0);
  mode_on_send=T_OK;op=async_create(p,&b);CHECK(async_finish(op,0));CHECK(op->output.ok);async_drop_output(op,&b,1);
  reset_mode_on_send=T_FLUSH_FAIL;op=async_create(p,&b);CHECK(async_finish(op,0));CHECK(op->output.ok && !p->conns[0]);async_drop_output(op,&b,1);
  reset_mode_on_send=T_RESTORE;op=async_create(p,&b);CHECK(async_finish(op,0));async_drop_output(op,&b,1);
  /* More waiters than one notification batch, with close and no subsequent
   * lease release to hide a dropped notification. Every pin must be drained. */
  mode_on_send=T_READ_TIMEOUT;KuPgAsyncOp* held=async_create(p,&b);CHECK(!ku_pg_async_step(held,0,clock_ms,0));
  KuPgAsyncOp* waiters[128];AsyncBudget budgets[128];memset(budgets,0,sizeof(budgets));
  for(size_t i=0;i<128;i++){waiters[i]=async_create(p,&budgets[i]);CHECK(!ku_pg_async_step(waiters[i],0,clock_ms,0));CHECK(waiters[i]->waiting);}
  CHECK(p->waiters==128 && p->async_operations==129);ku_pg_client_close_owned(p);
  for(size_t i=0;i<128;i++){CHECK(budgets[i].wakes==1);CHECK(waiters[i]->waiter.in_flight==0);CHECK(async_finish(waiters[i],1));async_drop_output(waiters[i],&budgets[i],0);}
  CHECK(async_finish(held,1));async_drop_output(held,&b,0); /* Last op physically destroys closed p. */
  CHECK(live_results==0 && live_connections==0 && live_runtime_allocations==0);
  puts("pg async pump closed loop");return 0;
}
"#;

const ASYNC_ACQUIRE_DIAGNOSTIC_SUITE: &str = r#"
static int print_pg_diagnostic_json(size_t capacity) {
  char text[131072];CHECK(capacity<=sizeof(text));
  size_t size=ku_pg_diagnostics_json(text,capacity);CHECK(size && size<capacity && text[size]==0);
  printf("{%s}\n",text);return 0;
}
int main(void) {
  clock_ms=1000;clock_step=0;mode_on_send=T_OK;reset_mode_on_send=T_READ_TIMEOUT;
  track_runtime_allocations=1;
  KuResult_pg_client opened=ku_pg_client_open(ku_string_static((const uint8_t*)"hostaddr=127.0.0.1",18),1,16,1000,250,3000);
  CHECK(opened.ok);KuPgClient* p=opened.value;p->lease_generations[0]=UINT64_MAX;
  AsyncBudget held_budget={0};KuPgAsyncOp* held=async_create(p,&held_budget);
  held->waiter.diagnostic.task_slot=7;held->waiter.diagnostic.task_generation=11;
  /* Initial result capacity growth deliberately yields its own owner turn.
   * Drive that existing bounded state machine until reset actually parks. */
  for(size_t turns=0;held->phase!=KU_PG_ASYNC_RESET_READ && turns<32u;turns++)CHECK(!ku_pg_async_step(held,0,clock_ms,0));
  CHECK(held->phase==KU_PG_ASYNC_RESET_READ && held->interest==1u && p->active==1 && held->output.ok);
  CHECK(p->async_owners[0]==&held->waiter && p->lease_generations[0]==UINT64_MAX);
  AsyncBudget budgets[9]={{0}};KuPgAsyncOp* waiting[9]={0};
  for(size_t i=0;i<9;i++) {
    waiting[i]=async_create(p,&budgets[i]);
    KuPgDiagnosticLease* d=&waiting[i]->waiter.diagnostic;
    d->task_slot=i+10u;d->task_generation=i+100u;d->http_task_slot=42;d->http_task_generation=71;d->owner=1;d->job_generation=i+200u;d->parent_chain_missing=0;
    CHECK(!ku_pg_async_step(waiting[i],0,clock_ms,0) && waiting[i]->waiting);
  }
  CHECK(p->waiters==9 && !ku_pg_diag_load(&ku_pg_diagnostics.failures));
  clock_ms=1001;uint64_t previous_progress=ku_pg_diag_load(&waiting[0]->waiter.diagnostic.progress_events);
  CHECK(!ku_pg_async_step(waiting[0],0,clock_ms,0));
  CHECK(ku_pg_diag_load(&waiting[0]->waiter.diagnostic.progress_events)==previous_progress && ku_pg_diag_load(&waiting[0]->waiter.diagnostic.last_progress_ms)==1000);
  clock_ms=1250;
  for(size_t i=0;i<9;i++) {
    CHECK(ku_pg_async_step(waiting[i],0,clock_ms,0));
    CHECK(!waiting[i]->sent && ku_string_equal(waiting[i]->output.error.code,ku_string_static((const uint8_t*)"acquire_timeout",15)));
    async_drop_output(waiting[i],&budgets[i],0);
  }
  CHECK(ku_pg_diag_load(&ku_pg_diagnostics.failures)==9 && ku_pg_diagnostics.count==2 && !ku_pg_diagnostics.changes && ku_pg_diagnostics.overwritten==7);
  KuPgDiagnosticSnapshot* s=ku_pg_diag_latest_locked();
  CHECK(s->phase==KU_PG_ASYNC_ACQUIRE && s->observed_ms==1250 && s->acquire_deadline_ms==1250 && s->started_ms==1000);
  CHECK(s->active==1 && s->reset==1 && !s->idle && !s->empty && !s->owner_missing && s->waiters==1);
  CHECK(s->slots[0].task_generation==11 && s->slots[0].phase==KU_PG_ASYNC_RESET_READ
      && !s->slots[0].worker_enqueue_ms && !s->slots[0].worker_start_ms
      && !s->slots[0].worker_finish_ms); /* client_open supplied this warm lease. */
  CHECK(ku_pg_diagnostics.samples[0].sequence==1 && ku_pg_diagnostics.samples[0].task_slot==10
      && ku_pg_diagnostics.samples[0].waiters==9 && s->sequence==9
      && !ku_pg_diagnostics.change_capacity_drops);
  CHECK(s->task_slot==18 && s->http_task_slot==42 && s->http_task_generation==71 && !s->parent_chain_missing);
  CHECK(s->queue_head_task_slot==18 && s->queue_head_task_generation==108 && s->lease_generation_saturated);
  /* A competing management read must never block failure publication. */
  AsyncBudget dropped_budget={0};KuPgAsyncOp* dropped=async_create(p,&dropped_budget);
  CHECK(!ku_pg_async_step(dropped,0,clock_ms,0));clock_ms=dropped->acquire_deadline;
  CHECK(ku_pg_diag_try_lock());CHECK(ku_pg_async_step(dropped,0,clock_ms,0));ku_pg_diag_unlock();
  CHECK(ku_pg_diag_load(&ku_pg_diagnostics.contention_drops)==1 && ku_pg_diag_load(&ku_pg_diagnostics.failures)==10);
  async_drop_output(dropped,&dropped_budget,0);
  CHECK(!print_pg_diagnostic_json(131072));
  /* Request identity, absolute timing and precise queue length must not exhaust
   * category slots before a genuinely new phase/worker stage is observed. */
  CHECK(ku_pg_diag_try_lock());
  KuPgDiagnosticSnapshot varied=ku_pg_diagnostics.samples[0];
  for(uint64_t sequence=10u;sequence<138u;sequence++) {
    varied.sequence=sequence;varied.task_slot=sequence;varied.task_generation=sequence+100u;
    varied.job_generation=sequence+200u;varied.observed_ms=sequence+2000u;varied.waiters=sequence;
    for(size_t j=0;j<varied.slot_samples;j++) {
      varied.slots[j].generation=sequence+j;varied.slots[j].task_generation=sequence+j+100u;
      if(varied.slots[j].worker_start_ms)varied.slots[j].worker_start_ms=sequence+3000u;
      if(varied.slots[j].worker_finish_ms)varied.slots[j].worker_finish_ms=sequence+4000u;
    }
    for(size_t j=0;j<varied.waiter_samples;j++) {
      varied.queue[j].task_slot=sequence+j;varied.queue[j].task_generation=sequence+j+500u;
      varied.queue[j].job_generation=sequence+j+600u;
    }
    ku_pg_diag_retain_locked(&varied);
  }
  CHECK(!ku_pg_diagnostics.changes && ku_pg_diagnostics.count==2
      && ku_pg_diagnostics.samples[0].sequence==1u && ku_pg_diag_latest_locked()->sequence==137u);
  varied.sequence=138u;varied.phase=KU_PG_ASYNC_CONNECT;varied.worker_enqueue_ms=5000u;
  ku_pg_diag_retain_locked(&varied);
  varied.sequence=139u;varied.worker_start_ms=5001u;ku_pg_diag_retain_locked(&varied);
  varied.sequence=140u;varied.worker_finish_ms=5002u;varied.connect_ready=1;ku_pg_diag_retain_locked(&varied);
  CHECK(ku_pg_diagnostics.changes==3 && ku_pg_diagnostics.samples[1].sequence==138u
      && !ku_pg_diagnostics.samples[1].worker_start_ms && ku_pg_diagnostics.samples[2].worker_start_ms==5001u
      && !ku_pg_diagnostics.samples[2].worker_finish_ms && ku_pg_diagnostics.samples[3].worker_finish_ms==5002u);
  varied.sequence=141u;varied.closing=1;ku_pg_diag_retain_locked(&varied);
  varied.sequence=142u;varied.sent=1;ku_pg_diag_retain_locked(&varied);
  varied.sequence=143u;varied.idle=1;ku_pg_diag_retain_locked(&varied);
  CHECK(ku_pg_diagnostics.changes==6 && !ku_pg_diagnostics.change_capacity_drops);
  varied.sequence=144u;varied.connecting=1;ku_pg_diag_retain_locked(&varied);
  CHECK(ku_pg_diagnostics.count==8 && ku_pg_diagnostics.change_capacity_drops==1
      && ku_pg_diagnostics.samples[0].sequence==1u && ku_pg_diag_latest_locked()->sequence==144u
      && ku_pg_diagnostics.samples[1].sequence==138u && ku_pg_diagnostics.samples[3].sequence==140u);
  ku_pg_diag_unlock();
  /* Serialize every scalar at maximum width and every member slot. The real
   * management caller also exercises the explicit bounded fallback below. */
  for(size_t i=0;i<8;i++) {
    memset(&ku_pg_diagnostics.samples[i],0xff,sizeof(ku_pg_diagnostics.samples[i]));
    ku_pg_diagnostics.samples[i].slot_samples=8;ku_pg_diagnostics.samples[i].waiter_samples=8;
    ku_pg_diagnostics.samples[i].sequence=i+1u;
  }
  CHECK(!print_pg_diagnostic_json(131072));
  /* A smaller response still preserves the first complete failure and rolls
   * back only the unfinished next state-change/latest object. */
  CHECK(!print_pg_diagnostic_json(24576));CHECK(!print_pg_diagnostic_json(512));
  CHECK(ku_pg_diag_load(&ku_pg_diagnostics.serialization_truncations)==2);
  CHECK(ku_pg_diag_try_lock());CHECK(!print_pg_diagnostic_json(131072));ku_pg_diag_unlock();
  CHECK(ku_pg_diag_load(&ku_pg_diagnostics.export_contention)==1);
  CHECK(ku_pg_async_step(held,0,clock_ms,1) && held->output.ok);
  CHECK(!p->async_owners[0] && !p->in_use[0] && !p->active && !p->waiters && !p->conns[0]);
  async_drop_output(held,&held_budget,1);ku_pg_client_close_owned(p);
  CHECK(!held_budget.bytes && !live_connections && !live_results && !live_runtime_allocations && !runtime_bytes);
  fprintf(stderr,"PG diagnostic cleanup: starts=%d finishes=%d bad_calls=%d finishes_locked=%d lock_depth=%d\n",starts,finishes,bad_calls,ku_test_pg_finishes_while_pool_locked,ku_test_pg_pool_lock_depth);
  CHECK(starts==finishes && !bad_calls && !ku_test_pg_finishes_while_pool_locked);
  fprintf(stderr,"PG diagnostic layout: fixed=%zu lease=%zu waiter=%zu operation=%zu client=%zu\n",sizeof(ku_pg_diagnostics)+sizeof(ku_pg_diag_guard),sizeof(KuPgDiagnosticLease),sizeof(KuPgAsyncWaiter),sizeof(KuPgAsyncOp),sizeof(KuPgClient));
  track_runtime_allocations=0;return 0;
}
"#;

const ASYNC_ACQUIRE_LOCK_DEADLINE_SUITE: &str = r#"
static unsigned long long expire_acquire_lock_at;
static void ku_test_pg_expire_acquire_lock(void) {
  if(expire_acquire_lock_at) {clock_ms=expire_acquire_lock_at;expire_acquire_lock_at=0;}
}
int main(void) {
  clock_ms=1000;clock_step=0;mode_on_send=T_OK;reset_mode_on_send=T_RESTORE;
  track_runtime_allocations=1;
  KuResult_pg_client opened=ku_pg_client_open(ku_string_static((const uint8_t*)"hostaddr=127.0.0.1",18),1,8,1000,250,2000);
  CHECK(opened.ok);KuPgClient* p=opened.value;
  AsyncBudget budget={0};KuPgAsyncOp* op=async_create(p,&budget);
  int before_sends=user_sends;
  if(KU_TEST_EXPIRED_CONNECT) {
    PGconn* old=p->conns[0];p->conns[0]=NULL;PQfinish(old);
    CHECK(!ku_pg_async_step(op,0,clock_ms,0));
    CHECK(op->phase==KU_PG_ASYNC_CONNECT && op->connecting && !op->connect_ready);
    /* This bare pump calls the real deferred-connect worker entry, but has no
     * Task queue/job. Enqueue remains explicitly missing rather than invented. */
    op->waiter.diagnostic.job_generation=UINT64_C(4294967297);
    CHECK(!ku_pg_diag_load(&op->waiter.diagnostic.worker_enqueue_ms)
        && !ku_pg_diag_load(&op->waiter.diagnostic.worker_start_ms)
        && !ku_pg_diag_load(&op->waiter.diagnostic.worker_finish_ms));
    if(KU_TEST_CONNECT_OOM)fail_connect_on_attempt=1;
    ku_pg_async_connect_run(op,0);
    CHECK(op->connect_ready && p->active==1);
    CHECK(!ku_pg_diag_load(&op->waiter.diagnostic.worker_enqueue_ms)
        && ku_pg_diag_load(&op->waiter.diagnostic.worker_start_ms)==1000u
        && ku_pg_diag_load(&op->waiter.diagnostic.worker_finish_ms)==1000u);
    if(KU_TEST_CONNECT_OOM)CHECK(!op->connected.conn && op->connected.outcome==KU_PG_CONNECT_OUT_OF_MEMORY && !fail_connect_on_attempt);
    else CHECK(op->connected.conn);
  }
  /* The caller sampled time before attempting the pool mutex. Simulate the
   * passage to the absolute acquire deadline inside that mutex acquisition;
   * the longer query deadline must not renew the expired acquire lease. */
  uint64_t failures_before=ku_pg_diag_load(&ku_pg_diagnostics.failures),samples_before=ku_pg_diagnostics.count;
  unsigned long long sampled=op->acquire_deadline-1u;
  clock_ms=sampled;expire_acquire_lock_at=op->acquire_deadline;
  CHECK(ku_pg_async_step(op,0,sampled,0));
  CHECK(!expire_acquire_lock_at && !op->output.ok && !op->sent && user_sends==before_sends);
  if(KU_TEST_CONNECT_OOM) {
    /* OOM retains its public error priority even if acquiring the PG lock
     * crosses D. It must not publish a differently labelled timeout witness. */
    CHECK(ku_string_equal(op->output.error.code,ku_string_static((const uint8_t*)"out_of_memory",13)));
    CHECK(ku_pg_diag_load(&ku_pg_diagnostics.failures)==failures_before && ku_pg_diagnostics.count==samples_before);
  } else {
    CHECK(ku_string_equal(op->output.error.code,ku_string_static((const uint8_t*)"acquire_timeout",15)));
    CHECK(ku_pg_diag_load(&ku_pg_diagnostics.failures)==failures_before+1u && ku_pg_diagnostics.count==samples_before+1u);
    if(KU_TEST_EXPIRED_CONNECT) {
      KuPgDiagnosticSnapshot* recorded=ku_pg_diag_latest_locked();
      CHECK(recorded->job_generation==UINT64_C(4294967297) && recorded->phase==KU_PG_ASYNC_CONNECT
          && !recorded->worker_enqueue_ms && recorded->worker_start_ms==1000u
          && recorded->worker_finish_ms==1000u);
    }
  }
  CHECK(op->slot<0 && !op->connecting && !p->active && !p->connect_in_flight);
  CHECK(!!p->conns[0]==!KU_TEST_EXPIRED_CONNECT);
  async_drop_output(op,&budget,0);ku_pg_client_close_owned(p);
  CHECK(!budget.bytes && !live_connections && !live_results && !live_runtime_allocations && !runtime_bytes);
  CHECK(starts==finishes && !bad_calls && !ku_test_pg_finishes_while_pool_locked);
  track_runtime_allocations=0;
  puts("pg async acquisition lock deadline closed loop");return 0;
}
"#;

const ASYNC_INPUT_OWNER_SUITE: &str = r#"
static size_t input_limit=SIZE_MAX;
static AsyncBudget* input_allocation_budget;
static unsigned long long input_expire_on_allocation;
static size_t input_allocation_calls;
static int input_reserve(void* raw,size_t bytes) {
  AsyncBudget* budget=(AsyncBudget*)raw;
  if(budget->bytes>input_limit || bytes>input_limit-budget->bytes)return 0;
  return async_reserve(raw,bytes);
}
static void* ku_test_pg_malloc(size_t bytes) {
  if(input_allocation_budget) {
    /* All operation/input allocations must follow their actual reservation. */
    if(input_allocation_budget->bytes<bytes)abort();
    input_allocation_calls++;
  }
  void* pointer=input_real_malloc(bytes);
  if(pointer && input_expire_on_allocation) {
    clock_ms=input_expire_on_allocation;input_expire_on_allocation=0;
  }
  return pointer;
}
static KuPgAsyncOp* input_create(KuPgClient* pool,AsyncBudget* budget,KuString sql,
    KuArray_str params,int typed,const KuValue* leaves,KuArray_str types,KuError* error) {
  input_allocation_budget=budget;input_allocation_calls=0;
  KuPgAsyncBudget host={budget,input_reserve,async_refund};
  KuPgAsyncOp* operation=typed
      ?ku_pg_async_create_one_typed(pool,sql,leaves,params.len,types,(KuArray_str){0},clock_ms+100u,host,async_wake,budget,error)
      :ku_pg_async_create(pool,sql,params,clock_ms+100u,host,async_wake,budget,error);
  input_allocation_budget=NULL;return operation;
}
int main(void) {
  clock_ms=1000;clock_step=0;track_runtime_allocations=1;mode_on_send=T_OK;reset_mode_on_send=T_RESTORE;
  KuResult_pg_client opened=ku_pg_client_open(text("hostaddr=127.0.0.1"),1,8,1000,1000,2000);
  CHECK(opened.ok);KuPgClient* pool=opened.value;
  KuString strings[]={text("one"),text(""),text("中文")};
  KuString schemas[]={text("int4"),text("text?"),text("text")};
  KuValue leaves[]={ku_v_int(123),ku_v_null(),ku_v_str(strings[2])};
  /* Empty SQL and zero parameters retain their previous validation contract.
   * All combinations still copy, rather than borrow the caller's storage. */
  for(int typed=0;typed<2;typed++)for(size_t count=0;count<=3u;count++)for(int empty=0;empty<2;empty++) {
    AsyncBudget budget={0};KuError error={0};size_t base=runtime_bytes;int base_live=live_runtime_allocations;
    KuString sql=text(empty?"":"SELECT $1");KuArray_str params={count,strings,count};
    KuPgAsyncOp* operation=input_create(pool,&budget,sql,params,typed,leaves,(KuArray_str){count,schemas,count},&error);
    CHECK(operation && !error.code.ptr && input_allocation_calls==1u);
    CHECK(runtime_bytes-base==operation->input_charge && budget.bytes==operation->input_charge
        && live_runtime_allocations==base_live+1);
    uintptr_t first=(uintptr_t)operation,end=first+operation->input_charge;
    CHECK((uintptr_t)operation->sql>=first+sizeof(*operation)
        && (uintptr_t)operation->sql+sql.len<end && !operation->sql[sql.len]
        && !memcmp(operation->sql,sql.ptr,sql.len));
    CHECK(operation->sql!=(const char*)sql.ptr && operation->count==(int)count);
    if(operation->values_bytes)CHECK((uintptr_t)(typed?(void*)operation->typed:(void*)operation->values)>=first+sizeof(*operation)
        && (uintptr_t)(typed?(void*)operation->typed:(void*)operation->values)+operation->values_bytes<=end);
    if(typed)CHECK((uintptr_t)operation->typed%_Alignof(KuPgTypedQuery)==0u);
    else if(count)CHECK((uintptr_t)operation->values%_Alignof(const char*)==0u);
    if(operation->param_bytes)CHECK((uintptr_t)operation->params>=first+sizeof(*operation)
        && (uintptr_t)operation->params+operation->param_bytes<=end);
    else CHECK(!operation->params);
    if(!typed)for(size_t i=0;i<count;i++)CHECK(operation->values[i]!=(const char*)strings[i].ptr
        && !memcmp(operation->values[i],strings[i].ptr,strings[i].len) && !operation->values[i][strings[i].len]);
    if(typed && count>1u)CHECK(!operation->values[1]);
    size_t charge=operation->input_charge;
    CHECK(ku_pg_async_step(operation,0u,clock_ms,1));async_drop_output(operation,&budget,0);
    CHECK(!budget.bytes && runtime_bytes==base && live_runtime_allocations==base_live);

    input_limit=charge-1u;fail_runtime_allocation_after=1;
    operation=input_create(pool,&budget,sql,params,typed,leaves,(KuArray_str){count,schemas,count},&error);
    CHECK(!operation && !budget.bytes && !input_allocation_calls && fail_runtime_allocation_after==1);
    CHECK(ku_string_equal(error.code,text("resource_limit")));ku_error_drop(&error);
    input_limit=charge;int failures=injected_allocation_failures;
    operation=input_create(pool,&budget,sql,params,typed,leaves,(KuArray_str){count,schemas,count},&error);
    CHECK(!operation && !budget.bytes && input_allocation_calls==1u
        && injected_allocation_failures==failures+1 && !fail_runtime_allocation_after);
    CHECK(ku_string_equal(error.code,text("out_of_memory")));ku_error_drop(&error);
    CHECK(runtime_bytes==base && live_runtime_allocations==base_live);
    operation=input_create(pool,&budget,sql,params,typed,leaves,(KuArray_str){count,schemas,count},&error);
    CHECK(operation && budget.bytes==charge);CHECK(ku_pg_async_step(operation,0u,clock_ms,1));
    async_drop_output(operation,&budget,0);input_limit=SIZE_MAX;
    CHECK(!ku_database_budget_snapshot().pending_operations && !ku_database_budget_snapshot().pending_bytes);
  }
  /* Reject representational overflow before any caller charge or allocation. */
  size_t offset=17,charge=19;
  CHECK(!ku_pg_async_input_layout(SIZE_MAX,0,0,0,&offset,&charge) && offset==17 && charge==19);
  CHECK(!ku_pg_async_input_layout(1,SIZE_MAX,0,1,&offset,&charge));
  CHECK(!ku_pg_async_input_layout(1,0,SIZE_MAX,0,&offset,&charge));
  AsyncBudget budget={0};KuError error={0};size_t base=runtime_bytes;int before_sends=user_sends;
  input_expire_on_allocation=clock_ms+100u;
  KuPgAsyncOp* operation=input_create(pool,&budget,text("SELECT 3"),(KuArray_str){0},0,NULL,(KuArray_str){0},&error);
  CHECK(!operation && !input_expire_on_allocation && input_allocation_calls==1u && !budget.bytes);
  CHECK(ku_string_equal(error.code,text("query_timeout")) && user_sends==before_sends);ku_error_drop(&error);
  CHECK(runtime_bytes==base && !ku_database_budget_snapshot().pending_operations);
  /* A detached result outlives the whole input block. */
  operation=input_create(pool,&budget,text("SELECT 3"),(KuArray_str){0},0,NULL,(KuArray_str){0},&error);
  CHECK(operation && async_finish(operation,0) && operation->output.ok);
  KuResult_pg_result result=ku_pg_async_take(operation);size_t result_bytes=0;
  CHECK(result.ok && ku_pg_result_owned_bytes(result.value,&result_bytes));
  ku_pg_async_dispose(operation,1);CHECK(budget.bytes==result_bytes && result.value->rows==1u);
  CHECK(ku_pg_cell_in_bounds(result.value,0,0));ku_drop_pg_result(&result.value);async_refund(&budget,result_bytes);
  CHECK(!budget.bytes && runtime_bytes==base);
  CHECK(ku_pg_client_capture_retain(pool));ku_pg_client_close_owned(pool);CHECK(pool->closing);
  operation=input_create(pool,&budget,text("SELECT 3"),(KuArray_str){0},0,NULL,(KuArray_str){0},&error);
  CHECK(!operation && !budget.bytes && input_allocation_calls==1u && ku_string_equal(error.code,text("client_closed")));
  ku_error_drop(&error);ku_pg_client_capture_release(pool);
  CHECK(!live_connections && !live_results && !runtime_bytes && !live_runtime_allocations
      && !ku_database_budget_snapshot().pending_operations && !ku_database_budget_snapshot().pending_bytes && !bad_calls);
  track_runtime_allocations=0;puts("pg async input owner closed loop");return 0;
}
"#;

const ASYNC_ACQUIRE_HANDOFF_SUITE: &str = r#"
#define KU_TEST_HANDOFF_CANCEL (KU_TEST_HANDOFF_KIND==1)
#define KU_TEST_HANDOFF_SUCCESS (KU_TEST_HANDOFF_KIND==2)
typedef struct HandoffWake { KuPgAsyncOp* op; AsyncBudget* budget; } HandoffWake;
static void handoff_wake(void* context) {
  HandoffWake* wake=(HandoffWake*)context;
  /* The notification owns its callback pin and publishes outside the PG lock. */
  if(ku_test_pg_pool_lock_depth || !wake->op->waiter.in_flight)abort();
  async_wake(wake->budget);
}
static size_t handoff_idle_leases(KuPgClient* p) {
  size_t idle=0;
  for(size_t i=0;i<p->size;i++)if(p->conns[i] && !p->in_use[i])idle++;
  return idle;
}
int main(void) {
  clock_ms=1000;clock_step=0;mode_on_send=T_READ_TIMEOUT;reset_mode_on_send=T_RESTORE;
  track_runtime_allocations=1;
  int before_connects=starts,before_user_sends=user_sends,before_resets=reset_sends;
  KuResult_pg_client opened=ku_pg_client_open(ku_string_static((const uint8_t*)"hostaddr=127.0.0.1",18),KU_TEST_HANDOFF_SUCCESS?2:1,8,1000,250,2000);
  CHECK(opened.ok);KuPgClient* p=opened.value;
  CHECK(live_runtime_allocations>0 && runtime_bytes>0 && runtime_peak_bytes>0);
  AsyncBudget held_budget={0},held_second_budget={0},a_budget={0},b_budget={0};
  KuPgAsyncOp* held=async_create(p,&held_budget);
  CHECK(!ku_pg_async_step(held,0,clock_ms,0));
  CHECK(held->sent && held->phase==KU_PG_ASYNC_READ && held->interest==1u && p->active==1);
  KuPgAsyncOp* held_second=NULL;
  if(KU_TEST_HANDOFF_SUCCESS) {
    held_second=async_create(p,&held_second_budget);
    CHECK(!ku_pg_async_step(held_second,0,clock_ms,0));
    CHECK(held_second->phase==KU_PG_ASYNC_CONNECT && held_second->connecting && !held_second->connect_ready);
    ku_pg_async_connect_run(held_second,0);
    CHECK(!ku_pg_async_step(held_second,0,clock_ms,0));
    CHECK(held_second->sent && held_second->phase==KU_PG_ASYNC_READ && held_second->interest==1u);
    CHECK(p->active==2 && !p->connect_in_flight && starts==before_connects+2);
  }
  KuPgAsyncOp* a=async_create(p,&a_budget);HandoffWake a_wake={a,&a_budget};
  a->waiter.wake=handoff_wake;a->waiter.context=&a_wake;
  CHECK(!ku_pg_async_step(a,0,clock_ms,0));
  CHECK(a->waiting && !a->waiter.notified && a->acquire_deadline==1250u && p->waiters==1);
  /* Supply held responses at 1249ms. Follow normal result/terminal-NULL/
   * DISCARD ALL paths; never force leases free while libpq still has results. */
  clock_ms=a->acquire_deadline-1u;held->conn->mode=T_OK;mode_on_send=T_OK;
  CHECK(async_finish(held,0) && held->output.ok);
  CHECK(a_budget.wakes==1 && a->waiter.notified && !a->waiter.in_flight);
  async_drop_output(held,&held_budget,1);
  if(KU_TEST_HANDOFF_SUCCESS) {
    CHECK(p->active==1 && handoff_idle_leases(p)==1);
    held_second->conn->mode=T_OK;
    CHECK(async_finish(held_second,0) && held_second->output.ok);
    async_drop_output(held_second,&held_second_budget,1);
  }
  CHECK(p->active==0 && p->waiters==1 && handoff_idle_leases(p)==(KU_TEST_HANDOFF_SUCCESS?2u:1u));
  CHECK(reset_sends==before_resets+(KU_TEST_HANDOFF_SUCCESS?2:1) && a_budget.wakes==1);
  /* B cannot barge past A, even though A has not taken the available lease. */
  KuPgAsyncOp* b=async_create(p,&b_budget);HandoffWake b_wake={b,&b_budget};
  b->waiter.wake=handoff_wake;b->waiter.context=&b_wake;
  unsigned long long b_deadline=b->acquire_deadline;
  CHECK(b_deadline==1499u && !ku_pg_async_step(b,0,clock_ms,0));
  CHECK(b->waiting && !b->waiter.notified && !b_budget.wakes && p->waiters==2);
  int fd=-2;unsigned int events=99u;unsigned long long deadline=0;
  CHECK(!ku_pg_async_interest(b,&fd,&events,&deadline));
  CHECK(fd==-1 && !events && deadline==b_deadline && !b->sent);
  if(KU_TEST_HANDOFF_SUCCESS) {
    /* A claims only one lease and remains blocked in its own query. B must be
     * notified for the other idle lease before A can release its lease. */
    mode_on_send=T_READ_TIMEOUT;
    CHECK(!ku_pg_async_step(a,0,clock_ms,0));
    CHECK(a->sent && a->phase==KU_PG_ASYNC_READ && !a->waiting && !a->waiter.linked);
    CHECK(p->active==1 && p->waiters==1 && handoff_idle_leases(p)==1);
    CHECK(user_sends==before_user_sends+3 && reset_sends==before_resets+2);
  } else {
    if(!KU_TEST_HANDOFF_CANCEL)clock_ms=a->acquire_deadline;
    CHECK(ku_pg_async_step(a,0,clock_ms,KU_TEST_HANDOFF_CANCEL));
    CHECK(!a->output.ok && !a->sent && !a->waiting && !a->waiter.linked);
    CHECK(ku_string_equal(a->output.error.code,ku_string_static((const uint8_t*)
        (KU_TEST_HANDOFF_CANCEL?"cancelled":"acquire_timeout"),KU_TEST_HANDOFF_CANCEL?9u:15u)));
    CHECK(p->active==0 && p->waiters==1 && handoff_idle_leases(p)==1);
    CHECK(user_sends==before_user_sends+1);
  }
  CHECK(clock_ms<b_deadline && b->acquire_deadline==b_deadline);
  /* Readiness is the regression witness. Do not manually step an unnotified B:
   * the real reactor would have no descriptor/event before B's own deadline. */
  CHECK(ku_pg_async_interest(b,&fd,&events,&deadline));
  CHECK(fd==-1 && !events && deadline==b_deadline && b->waiter.notified && b_budget.wakes==1);
  CHECK(!b->waiter.in_flight);
  if(!KU_TEST_HANDOFF_SUCCESS) {
    CHECK(ku_pg_async_can_dispose(a) && ku_pg_async_can_dispose(a));
    async_drop_output(a,&a_budget,0);
  }
  CHECK(a_budget.wakes==1 && b_budget.wakes==1);
  CHECK(ku_pg_async_interest(b,&fd,&events,&deadline));
  mode_on_send=T_OK;
  CHECK(async_finish(b,0) && b->output.ok && b->sent && clock_ms<b_deadline);
  CHECK(!b->waiter.linked && !b->waiter.in_flight && b_budget.wakes==1);
  async_drop_output(b,&b_budget,1);
  if(KU_TEST_HANDOFF_SUCCESS) {
    CHECK(p->active==1 && a->sent && a->phase==KU_PG_ASYNC_READ);
    a->conn->mode=T_OK;CHECK(async_finish(a,0) && a->output.ok);
    async_drop_output(a,&a_budget,1);
  }
  int expected_queries=KU_TEST_HANDOFF_SUCCESS?4:2;
  CHECK(user_sends==before_user_sends+expected_queries && reset_sends==before_resets+expected_queries);
  CHECK(starts==before_connects+(KU_TEST_HANDOFF_SUCCESS?2:1));
  CHECK(p->active==0 && p->waiters==0 && p->async_operations==0);
  CHECK(!held_budget.bytes && !held_second_budget.bytes && !a_budget.bytes && !b_budget.bytes);
  ku_pg_client_close_owned(p);
  CHECK(!live_connections && !live_results && !live_runtime_allocations && !runtime_bytes && !bad_calls);
  CHECK(starts==finishes && runtime_peak_bytes>0 && !ku_test_pg_pool_lock_depth && !ku_test_pg_finishes_while_pool_locked);
  track_runtime_allocations=0;
  puts("pg async waiter handoff closed loop");return 0;
}
"#;

const ASYNC_NOTIFICATION_PIN_SUITE: &str = r#"
static KuTestEvent pin_entered,pin_release;
static void pinned_wake(void* context) {
  (void)context;
  if(!ku_test_event_set(&pin_entered) || !ku_test_event_wait(&pin_release,5000))abort();
}
static int close_for_pin(void* context) {ku_pg_client_close_owned((KuPgClient*)context);return 0;}
int main(void) {
  clock_ms=1000;clock_step=0;mode_on_send=T_READ_TIMEOUT;reset_mode_on_send=T_RESTORE;
  CHECK(ku_test_event_init(&pin_entered) && ku_test_event_init(&pin_release));
  KuResult_pg_client opened=ku_pg_client_open(ku_string_static((const uint8_t*)"hostaddr=127.0.0.1",18),1,128,1000,1000,2000);
  CHECK(opened.ok);KuPgClient* p=opened.value;AsyncBudget held_budget={0},wait_budget={0};
  KuPgAsyncOp* held=async_create(p,&held_budget);CHECK(!ku_pg_async_step(held,0,clock_ms,0));
  KuPgAsyncOp* waiting=async_create(p,&wait_budget);waiting->waiter.wake=pinned_wake;
  CHECK(!ku_pg_async_step(waiting,0,clock_ms,0));CHECK(waiting->waiting);
  KuTestThread thread;CHECK(ku_test_thread_start(&thread,close_for_pin,p));
  CHECK(ku_test_event_wait(&pin_entered,5000));
  CHECK(ku_pg_async_step(waiting,0,clock_ms,1));
  CHECK(!ku_pg_async_can_dispose(waiting));
  ku_pg_mutex_lock(&p->lock);CHECK(!waiting->waiter.linked && waiting->waiter.in_flight==1 && p->async_operations==2);ku_pg_mutex_unlock(&p->lock);
  CHECK(ku_test_event_set(&pin_release));CHECK(ku_test_thread_join(&thread,5000));
  CHECK(ku_pg_async_can_dispose(waiting));async_drop_output(waiting,&wait_budget,0);
  CHECK(async_finish(held,1));async_drop_output(held,&held_budget,0);
  CHECK(ku_test_event_destroy(&pin_entered) && ku_test_event_destroy(&pin_release));
  CHECK(live_connections==0 && live_results==0);
  puts("pg async notification pin closed loop");return 0;
}
"#;

const ASYNC_COMPACT_RESULT_SUITE: &str = r#"
static int compact_values(KuPgResult* r) {
  KuResult_str value=ku_pg_value(r,0,0);
  int good=value.ok && equals(value.value,"keep");
  if(value.ok)ku_string_drop(&value.value);else ku_error_drop(&value.error);
  KuResult_bool null=ku_pg_is_null(r,0,2);
  good=good && null.ok && null.value;
  if(!null.ok)ku_error_drop(&null.error);
  return good;
}
int main(void) {
  clock_ms=1000;clock_step=0;track_runtime_allocations=1;
  mode_on_send=T_NULL_EMPTY_UTF8;reset_mode_on_send=T_RESTORE;
  KuResult_pg_client opened=ku_pg_client_open(text("hostaddr=127.0.0.1"),1,8,1000,1000,2000);
  CHECK(opened.ok);AsyncBudget budget={0};KuPgAsyncOp* op=async_create(opened.value,&budget);
  int before_resets=reset_sends;
  CHECK(async_finish(op,0) && op->output.ok && reset_sends==before_resets+1 && !live_results);
  KuResult_pg_result detached=ku_pg_async_take(op);CHECK(detached.ok && detached.value);
  size_t charge=0;CHECK(ku_pg_result_owned_bytes(detached.value,&charge));
  CHECK(detached.value->inline_storage==4u*sizeof(KuPgCell)+16u);
  CHECK(!ku_pg_result_is_inline(detached.value,detached.value->cells)
      && ku_pg_result_is_inline(detached.value,detached.value->bytes));
  CHECK(budget.bytes==op->input_charge+charge);
  ku_pg_async_dispose(op,1);CHECK(budget.bytes==charge);
  ku_pg_client_close_owned(opened.value);
  CHECK(!live_connections && !live_results && runtime_bytes==charge && live_runtime_allocations==2);
  /* Values must outlive PQclear, strict reset, operation disposal and pool close. */
  KuResult_bool null=ku_pg_is_null(detached.value,0,0);CHECK(null.ok && null.value);
  KuResult_str empty=ku_pg_value(detached.value,0,1);CHECK(empty.ok && !empty.value.len);ku_string_drop(&empty.value);
  KuResult_str unicode=ku_pg_value(detached.value,0,2);
  CHECK(unicode.ok && unicode.value.len==6u && !memcmp(unicode.value.ptr,"\xe4\xb8\xad\xe6\x96\x87",6u));
  ku_string_drop(&unicode.value);ku_drop_pg_result(&detached.value);async_refund(&budget,charge);
  if(budget.bytes || live_runtime_allocations || runtime_bytes) {
    fprintf(stderr,"compact detached accounting: budget=%zu live=%d bytes=%zu\n",budget.bytes,live_runtime_allocations,runtime_bytes);
    for(size_t i=0;i<KU_PG_TEST_ALLOCATION_RECORDS;i++)if(runtime_allocations[i].pointer)
      fprintf(stderr,"compact retained record: index=%zu bytes=%zu\n",i,runtime_allocations[i].size);
  }
  CHECK(!budget.bytes && !live_runtime_allocations && !runtime_bytes);

  KuPgResult* r=ku_pg_result_new(3u,16u,256u);CHECK(r);
  CHECK(live_runtime_allocations==1 && ku_pg_result_owned_bytes(r,&charge) && charge==runtime_bytes);
  r->rows=1;r->bytes_len=4;memcpy(r->bytes,"keep",4);
  r->cells[0]=(KuPgCell){0,4};r->cells[1]=(KuPgCell){4,0};r->cells[2]=(KuPgCell){4,KU_PG_NULL_CELL};
  KuPgCell* original_cells=r->cells;uint8_t* original_bytes=r->bytes;
  size_t original_charge=charge;
  int failures=injected_allocation_failures;
  fail_runtime_allocation_after=1;CHECK(!ku_pg_result_grow_cells(r,32u));
  CHECK(!fail_runtime_allocation_after && injected_allocation_failures==failures+1
      && r->cells==original_cells && compact_values(r));
  fail_runtime_allocation_after=1;CHECK(!ku_pg_result_grow_bytes(r,512u));
  CHECK(!fail_runtime_allocation_after && injected_allocation_failures==failures+2
      && r->bytes==original_bytes && compact_values(r));
  CHECK(ku_pg_result_owned_bytes(r,&charge) && charge==original_charge && charge==runtime_bytes);
  size_t planned=0;CHECK(ku_pg_result_plan_bytes(r,32u,512u,&planned));
  CHECK(ku_pg_result_grow_cells(r,32u) && r->cells!=original_cells && compact_values(r));
  CHECK(ku_pg_result_owned_bytes(r,&charge) && charge==original_charge+32u*sizeof(KuPgCell)
      && charge==runtime_bytes);
  CHECK(ku_pg_result_grow_bytes(r,512u) && r->bytes!=original_bytes && compact_values(r));
  CHECK(ku_pg_result_owned_bytes(r,&charge) && charge==planned && charge==runtime_bytes);
  /* The original compact tail remains physically owned after both regions grow. */
  CHECK(charge==sizeof(*r)+r->inline_storage+32u*sizeof(KuPgCell)+512u);
  ku_drop_pg_result(&r);CHECK(!r && !live_runtime_allocations && !runtime_bytes);

  /* A large first row takes the old external-buffer fallback. Each actual
   * allocation failure must leave a valid partial owner that drops exactly once. */
  r=ku_pg_result_new(3u,32u,512u);CHECK(r && !r->inline_storage && !r->cells && !r->bytes);
  fail_runtime_allocation_after=1;CHECK(!ku_pg_result_grow_cells(r,32u));
  CHECK(!r->cells && !fail_runtime_allocation_after);
  CHECK(ku_pg_result_grow_cells(r,32u));
  fail_runtime_allocation_after=1;CHECK(!ku_pg_result_grow_bytes(r,512u));
  CHECK(!r->bytes && !fail_runtime_allocation_after);
  CHECK(ku_pg_result_owned_bytes(r,&charge) && charge==runtime_bytes);
  ku_drop_pg_result(&r);CHECK(!live_runtime_allocations && !runtime_bytes);
  CHECK(!live_connections && !live_results && !bad_calls);
  puts("pg compact result ownership and physical budget closed loop");return 0;
}
"#;

const ASYNC_LARGE_ROW_SUITE: &str = r#"
int main(void) {
  clock_ms=1000;clock_step=0;mode_on_send=T_OK;reset_mode_on_send=T_RESTORE;
  memset(async_large_data,'x',131079);async_large_data[131079]=0;
  memcpy(async_large_data+65535,"\xf0\x9f\x99\x82",4);
  memcpy(async_large_data+131073,"\xe4\xb8\xad\xe6\x96\x87",6);
  async_large_fixture=1;
  KuResult_pg_client opened=ku_pg_client_open(ku_string_static((const uint8_t*)"hostaddr=127.0.0.1",18),1,8,1000,1000,2000);
  CHECK(opened.ok);AsyncBudget b={0};KuPgAsyncOp* op=async_create(opened.value,&b);
  size_t steps=0,copy_turns=0,validation_turns=0;
  while(op->phase!=KU_PG_ASYNC_DONE && steps++<1024) {
    size_t before=op->query.value?op->query.value->bytes_len:0;
    int row_phase=op->row_phase;
    uint64_t progress_before=ku_pg_diag_load(&op->waiter.diagnostic.progress_events);
    clock_ms++;
    (void)ku_pg_async_step(op,3u,clock_ms,0);
    size_t after=op->query.value?op->query.value->bytes_len:before;
    if(after>=before && after>before) {CHECK(after-before<=65536u);copy_turns++;}
    if(row_phase==1) {validation_turns++;CHECK(ku_pg_diag_load(&op->waiter.diagnostic.progress_events)>progress_before && ku_pg_diag_load(&op->waiter.diagnostic.last_progress_ms)==clock_ms);}
  }
  CHECK(op->phase==KU_PG_ASYNC_DONE && op->output.ok && steps<1024 && copy_turns>=2 && validation_turns>=2);
  CHECK(op->output.value->rows==1 && op->output.value->bytes_len==131079);
  KuResult_str value=ku_pg_value(op->output.value,0,0);CHECK(value.ok && value.value.len==131079);
  CHECK(!memcmp(value.value.ptr,async_large_data,131079));ku_string_drop(&value.value);
  async_drop_output(op,&b,1);ku_pg_client_close_owned(opened.value);
  CHECK(live_connections==0 && live_results==0 && b.bytes==0);
  puts("pg async large row closed loop");return 0;
}
"#;

const PG_TASK_FRESH_CANCEL_SUITE: &str = r#"
static KuTaskBlockingPoolV1* fresh_observed_pool;
static uint64_t fresh_real_task_phase(void* control) {
#if defined(_WIN32)
  CHECK(TryAcquireSRWLockExclusive(&fresh_observed_pool->mutex));
  ReleaseSRWLockExclusive(&fresh_observed_pool->mutex);
#else
  CHECK(!pthread_mutex_trylock(&fresh_observed_pool->mutex));
  CHECK(!pthread_mutex_unlock(&fresh_observed_pool->mutex));
#endif
  return ku_task_pg_diagnostic_phase(control);
}
int main(void) {
  KuTaskBlockingPoolV1 pool={0};KuTaskBlockingJobV1 job={0};
  KuTaskControlV1 control={0};KuPgAsyncOp operation={0};
#if defined(_WIN32)
  InitializeSRWLock(&pool.mutex);
#else
  CHECK(!pthread_mutex_init(&pool.mutex,NULL));
#endif
  fresh_observed_pool=&pool;pool.capacity=1u;
  ku_task_control_atomic_init(&pool.pg_closing_view,0u);
  ku_task_control_atomic_init(&job.pg_cancelled_view,0u);
  ku_task_control_atomic_init(&control.phase,KU_TASK_CONTROL_LIVE);
  operation.task_phase_context=&control;operation.task_phase=fresh_real_task_phase;
  job.pg_pool=&pool;job.pg_operation=&operation;job.kind=9u;job.state=KU_TASK_BLOCKING_BUILDING;
  job.generation=UINT64_C(4294967297);
  CHECK(!ku_task_blocking_lock(&pool));
  CHECK(ku_task_blocking_state_locked(&pool,&job,KU_TASK_BLOCKING_NET_STEP)==KU_TASK_DRIVER_OK);
  ku_task_blocking_unlock(&pool);
  CHECK(ku_task_control_atomic_load(&job.pg_cancelled_view)==KU_TASK_BLOCKING_PG_STEPPING);
  CHECK(!ku_task_pg_fresh_cancel(&job,job.generation) && !pool.fault);
  ku_task_control_atomic_store(&control.phase,KU_TASK_CONTROL_REQUESTED_CANCEL);
  CHECK(!job.cancelled && ku_task_pg_fresh_cancel(&job,job.generation) && !pool.fault);
  ku_task_control_atomic_store(&control.phase,KU_TASK_CONTROL_LIVE);
  CHECK(!ku_task_blocking_lock(&pool));
  CHECK(ku_task_blocking_cancel_job_locked(&pool,&job)==KU_TASK_DRIVER_OK);
  ku_task_blocking_unlock(&pool);
  CHECK(ku_task_control_atomic_load(&job.pg_cancelled_view)==(KU_TASK_BLOCKING_PG_CANCELLED|KU_TASK_BLOCKING_PG_STEPPING)
      && ku_task_pg_fresh_cancel(&job,job.generation) && !pool.fault);
  /* Reset this exclusively owned synthetic job before the next independent case. */
  CHECK(!ku_task_blocking_lock(&pool));job.cancelled=0;
  ku_task_control_atomic_store(&job.pg_cancelled_view,KU_TASK_BLOCKING_PG_STEPPING);
  ku_task_blocking_set_closing_locked(&pool,1u);ku_task_blocking_unlock(&pool);
  CHECK(ku_task_pg_fresh_cancel(&job,job.generation) && !pool.fault);
  CHECK(!ku_task_blocking_lock(&pool));ku_task_blocking_set_closing_locked(&pool,0u);ku_task_blocking_unlock(&pool);
  CHECK(ku_task_pg_fresh_cancel(&job,1u) && pool.fault==KU_TASK_DRIVER_INTERNAL);pool.fault=0;
  CHECK(!ku_task_pg_fresh_cancel(&job,job.generation));
  CHECK(!ku_task_blocking_lock(&pool));
  CHECK(ku_task_blocking_state_locked(&pool,&job,KU_TASK_BLOCKING_NET_WAIT)==KU_TASK_DRIVER_OK);
  ku_task_blocking_unlock(&pool);
  CHECK(!ku_task_control_atomic_load(&job.pg_cancelled_view));
  CHECK(ku_task_pg_fresh_cancel(&job,job.generation) && pool.fault==KU_TASK_DRIVER_INTERNAL);pool.fault=0;
  CHECK(!ku_task_blocking_lock(&pool));
  CHECK(ku_task_blocking_state_locked(&pool,&job,KU_TASK_BLOCKING_NET_STEP)==KU_TASK_DRIVER_OK);
  ku_task_blocking_unlock(&pool);job.kind=8u;
  CHECK(ku_task_pg_fresh_cancel(&job,job.generation) && pool.fault==KU_TASK_DRIVER_INTERNAL);pool.fault=0;
  job.kind=9u;CHECK(!ku_task_pg_fresh_cancel(&job,job.generation));
  job.pg_operation=NULL;CHECK(ku_task_pg_fresh_cancel(&job,job.generation) && pool.fault==KU_TASK_DRIVER_INTERNAL);
  pool.fault=0;job.pg_operation=&operation;
  operation.task_phase=NULL;CHECK(ku_task_pg_fresh_cancel(&job,job.generation) && pool.fault==KU_TASK_DRIVER_INTERNAL);
  pool.fault=0;operation.task_phase=fresh_real_task_phase;
  operation.task_phase_context=NULL;CHECK(ku_task_pg_fresh_cancel(&job,job.generation) && pool.fault==KU_TASK_DRIVER_INTERNAL);
  pool.fault=0;operation.task_phase_context=&control;
  CHECK(!ku_task_pg_fresh_cancel(&job,job.generation));
  job.pg_pool=NULL;CHECK(ku_task_pg_fresh_cancel(&job,job.generation));
#if !defined(_WIN32)
  CHECK(!pthread_mutex_destroy(&pool.mutex));
#endif
  CHECK(!live_connections && !live_results && !live_runtime_allocations && !runtime_bytes);
  puts("pg Task fresh cancel observer closed loop");return 0;
}
"#;

const ASYNC_TURN_SUITE: &str = r#"
/* A deterministic pinned-executor substitute. It observes the exact old growth
 * yield, after allocation but before any result byte/cell has been published. */
typedef struct AsyncFreshObserver { KuPgAsyncOp* operation;uint64_t generation;size_t calls;int cancel,expire; } AsyncFreshObserver;
static int async_fresh_observe(void* raw,uint64_t generation) {
  AsyncFreshObserver* observer=(AsyncFreshObserver*)raw;KuPgAsyncOp* op=observer->operation;
  if(generation!=observer->generation || !op->query.value || op->row_phase!=3
      || op->query.value->rows || op->query.value->bytes_len)abort();
  observer->calls++;if(observer->expire)clock_ms=op->deadline;return observer->cancel;
}
static void async_use_fresh_observer(KuPgAsyncOp* op,AsyncFreshObserver* observer) {
  observer->operation=op;observer->generation=17u;op->waiter.diagnostic.job_generation=observer->generation;
  op->fresh_cancel_context=observer;op->fresh_cancel=async_fresh_observe;
}
static size_t async_turn_steps,async_cell_yields,async_byte_yields;
static int async_measured_step(KuPgAsyncOp* op,int cancel) {
  async_validated_bytes=async_copied_bytes=async_reported_bytes=async_reported_cells=async_protocol_results=0;
  int done=ku_pg_async_step(op,3u,clock_ms,cancel);
  if(async_validated_bytes+async_copied_bytes>65536u
      || async_reported_bytes!=async_validated_bytes+async_copied_bytes
      || async_reported_cells>64u || async_protocol_results>32u) {
    fprintf(stderr,"PG turn budget: validated=%zu copied=%zu reported=%zu cells=%zu protocol=%zu\n",
        async_validated_bytes,async_copied_bytes,async_reported_bytes,async_reported_cells,async_protocol_results);
    abort();
  }
  if(async_reported_cells==64u)async_cell_yields++;
  if(async_reported_bytes==65536u)async_byte_yields++;
  async_turn_steps++;
  if(op->phase==KU_PG_ASYNC_CONNECT && !op->connect_ready)ku_pg_async_connect_run(op,cancel);
  return done;
}
static int async_measured_finish(KuPgAsyncOp* op) {
  for(size_t i=0;i<1024;i++)if(async_measured_step(op,0))return 1;
  return 0;
}
int main(void) {
  clock_ms=1000;clock_step=0;mode_on_send=T_ASYNC_ROWS;reset_mode_on_send=T_RESTORE;
  KuResult_pg_client opened=ku_pg_client_open(text("hostaddr=127.0.0.1"),1,8,1000,1000,2000);
  CHECK(opened.ok);KuPgClient* pool=opened.value;AsyncBudget budget={0};
  async_fixture_rows=257;strcpy(async_fixture_data,"last");
  KuPgAsyncOp* op=async_create(pool,&budget);int sends_before=user_sends,resets_before=reset_sends;
  CHECK(async_measured_finish(op) && op->output.ok);
  CHECK(op->output.value->rows==257 && op->output.value->cols==3
      && op->output.value->bytes_len==257u*3u*4u);
  CHECK(async_turn_steps<80 && async_cell_yields && !budget.zero_reserves && !budget.zero_refunds
      && budget.reserves<32 && user_sends==sends_before+1 && reset_sends==resets_before+1);
  size_t owned=0;CHECK(ku_pg_result_owned_bytes(op->output.value,&owned));
  CHECK(budget.bytes==op->input_charge+owned && pool->active==0 && pool->conns[0]);
  KuResult_str last=ku_pg_value(op->output.value,256,2);CHECK(last.ok && equals(last.value,"last"));
  ku_string_drop(&last.value);async_drop_output(op,&budget,1);
  CHECK(!budget.bytes && !ku_database_budget_snapshot().pending_operations
      && !ku_database_budget_snapshot().pending_bytes);

  /* The Task observer may eliminate only the compact first-allocation yield.
   * All byte/cell/protocol quantum assertions above remain active per step. */
  size_t standalone_steps=async_turn_steps;async_turn_steps=0;
  AsyncFreshObserver observer={0};op=async_create(pool,&budget);async_use_fresh_observer(op,&observer);
  CHECK(async_measured_finish(op) && op->output.ok && observer.calls==1u
      && async_turn_steps<=standalone_steps && op->output.value->rows==257
      && op->output.value->bytes_len==257u*3u*4u);
  size_t observed=observer.calls;async_drop_output(op,&budget,1);CHECK(observer.calls==observed && !budget.bytes);
  for(int failure=0;failure<2;failure++) {
    clock_ms=1000;observer=(AsyncFreshObserver){0};observer.cancel=!failure;observer.expire=failure;
    op=async_create(pool,&budget);async_use_fresh_observer(op,&observer);
    sends_before=user_sends;resets_before=reset_sends;
    CHECK(async_measured_finish(op) && !op->output.ok && observer.calls==1u
        && error_is(op->output,"execution_unknown") && user_sends==sends_before+1
        && reset_sends==resets_before && !pool->conns[0] && !pool->active);
    observed=observer.calls;async_drop_output(op,&budget,0);
    CHECK(observer.calls==observed && !budget.bytes && !live_results);
  }
  clock_ms=1000;

  /* A byte budget spans validation, copying and multiple rows. UTF8 at the
   * end of each cell also crosses a partially consumed shared turn budget. */
  async_fixture_rows=65;memset(async_fixture_data,'x',4000);async_fixture_data[4000]=0;
  memcpy(async_fixture_data+3996,"\xf0\x9f\x99\x82",4);
  async_turn_steps=async_byte_yields=0;op=async_create(pool,&budget);
  CHECK(async_measured_finish(op) && op->output.ok && async_byte_yields);
  CHECK(op->output.value->rows==65 && op->output.value->bytes_len==65u*3u*4000u);
  last=ku_pg_value(op->output.value,64,2);CHECK(last.ok && last.value.len==4000
      && !memcmp(last.value.ptr,async_fixture_data,4000));ku_string_drop(&last.value);
  async_drop_output(op,&budget,1);CHECK(!budget.bytes && !budget.zero_reserves);

  async_fixture_rows=257;async_fixture_null_empty=1;strcpy(async_fixture_data,"last");
  op=async_create(pool,&budget);CHECK(async_measured_finish(op) && op->output.ok);
  KuResult_bool absent=ku_pg_is_null(op->output.value,256,0);CHECK(absent.ok && absent.value);
  last=ku_pg_value(op->output.value,256,1);CHECK(last.ok && !last.value.len);ku_string_drop(&last.value);
  CHECK(op->output.value->bytes_len==257u*4u);async_drop_output(op,&budget,1);
  async_fixture_null_empty=0;

  /* Cancellation of a partially copied result still evicts the original lease;
   * a late invalid row drains through the original result/reset path. */
  op=async_create(pool,&budget);CHECK(!async_measured_step(op,0));
  CHECK(!async_measured_step(op,0) && op->query.value && op->query.value->rows>0
      && op->query.value->rows<257);
  CHECK(async_measured_step(op,1) && error_is(op->output,"execution_unknown"));
  CHECK(!pool->conns[0] && !pool->active);async_drop_output(op,&budget,0);
  op=async_create(pool,&budget);
  for(size_t i=0;!op->sent && i<32;i++)CHECK(!async_measured_step(op,0));
  CHECK(op->sent && op->phase!=KU_PG_ASYNC_DONE);
  clock_ms=op->deadline;CHECK(async_measured_step(op,0) && error_is(op->output,"execution_unknown"));
  CHECK(!pool->conns[0] && !pool->active);async_drop_output(op,&budget,0);
  async_fixture_invalid_row=129;op=async_create(pool,&budget);
  CHECK(async_measured_finish(op) && !op->output.ok);async_drop_output(op,&budget,0);
  async_fixture_invalid_row=0;async_fixture_rows=3;reset_mode_on_send=T_READ_TIMEOUT;
  op=async_create(pool,&budget);
  for(size_t i=0;op->phase!=KU_PG_ASYNC_RESET_READ && i<1024;i++)CHECK(!async_measured_step(op,0));
  CHECK(op->phase==KU_PG_ASYNC_RESET_READ && op->output.ok && pool->active==1);
  clock_ms=op->deadline;CHECK(async_measured_step(op,0) && op->output.ok && !pool->conns[0]);
  last=ku_pg_value(op->output.value,2,2);CHECK(last.ok && equals(last.value,"last"));
  ku_string_drop(&last.value);async_drop_output(op,&budget,1);
  CHECK(!budget.bytes && !budget.zero_reserves && !budget.zero_refunds
      && !ku_database_budget_snapshot().pending_operations && !ku_database_budget_snapshot().pending_bytes);
  ku_pg_client_close_owned(pool);CHECK(!live_connections && !live_results && !bad_calls);
  puts("pg async shared turns closed loop");return 0;
}
"#;

const ASYNC_FIRST_CLASS_SUITE: &str = r#"
int main(void) {
  clock_ms=1000;clock_step=0;track_runtime_allocations=1;reset_mode_on_send=T_RESTORE;
  KuResult_pg_client opened=ku_pg_client_open(text("hostaddr=127.0.0.1"),1,8,1000,1000,2000);
  CHECK(opened.ok);KuPgClient* pool=opened.value;AsyncBudget budget={0};
  const int columns[]={0,1,2,15,16,17};const size_t capacities[]={0,1,2,16,16,32};
  for(size_t i=0;i<sizeof(columns)/sizeof(columns[0]);i++) {
    CHECK(ku_pg_result_first_capacity((size_t)columns[i],1u,16u,4096u)==capacities[i]);
    for(size_t j=0;j<4u;j++) {
      const int rows[]={0,1,2,7};first_class_columns=columns[i];first_class_rows=rows[j];
      mode_on_send=T_FIRST_CLASS;
      size_t baseline_bytes=runtime_bytes;int baseline_live=live_runtime_allocations;
      KuPgAsyncOp* op=async_create(pool,&budget);CHECK(async_finish(op,0) && op->output.ok);
      KuPgResult* result=op->output.value;
      CHECK(result->rows==(size_t)first_class_rows && result->cols==(size_t)first_class_columns
          && result->bytes_len==(size_t)first_class_rows*((size_t)first_class_columns/3u)*10u);
      if(first_class_rows==1)CHECK(result->cell_capacity==capacities[i]);
      for(size_t row=0;row<result->rows;row++)for(size_t col=0;col<result->cols;col++) {
        KuPgCell cell=result->cells[row*result->cols+col];
        if(col%3u==0)CHECK(cell.len==KU_PG_NULL_CELL);
        else if(col%3u==1)CHECK(!cell.len);
        else CHECK(cell.len==10u && !memcmp(result->bytes+cell.offset,"\xe4\xb8\xad\xe6\x96\x87\xf0\x9f\x99\x82",10u));
      }
      size_t physical=0;CHECK(ku_pg_result_owned_bytes(result,&physical)
          && budget.bytes==op->input_charge+physical);
      async_drop_output(op,&budget,1);
      CHECK(!budget.bytes && runtime_bytes==baseline_bytes && live_runtime_allocations==baseline_live && !live_results);
    }
  }
  CHECK(!ku_pg_result_first_capacity(4097u,1u,16u,4096u));
  CHECK(ku_pg_result_first_capacity(1u,16u,256u,1048576u)==16u
      && ku_pg_result_first_capacity(17u,16u,256u,1048576u)==32u
      && ku_pg_result_first_capacity(257u,16u,256u,1048576u)==512u);
  first_class_columns=1;first_class_rows=2;first_class_large=1;memset(first_class_bytes,'x',1024u);
  mode_on_send=T_FIRST_CLASS;KuPgAsyncOp* op=async_create(pool,&budget);
  CHECK(async_finish(op,0) && op->output.ok && op->output.value->rows==2u
      && op->output.value->bytes_len==2048u && !op->output.value->inline_storage);
  async_drop_output(op,&budget,1);first_class_large=0;
  first_class_columns=15;first_class_rows=2;first_class_changed=1;
  op=async_create(pool,&budget);CHECK(async_finish(op,0) && !op->output.ok);
  async_drop_output(op,&budget,0);first_class_changed=0;
  ku_pg_client_close_owned(pool);
  CHECK(!budget.bytes && !live_connections && !live_results && !live_runtime_allocations && !runtime_bytes && !bad_calls);
  puts("pg first-row size classes and physical ownership closed loop");return 0;
}
"#;

const TYPED_STUB_GLOBALS: &str = r#"
static int typed_rows=1,typed_errors,typed_large,typed_parameter_checks;
static char typed_large_bytes[131080];
static char typed_source_text[]="\xe4\xb8\xad\xe6\x96\x87\xf0\x9f\x99\x82 ' \\";
static const unsigned int typed_column_oids[10]={23u,20u,16u,25u,1043u,25u,23u,20u,16u,20u};
static const unsigned char typed_numbers[10][8]={{128,0,0,0},{128,0,0,0,0,0,0,0},{1},{0},{0},{0},{127,255,255,255},{127,255,255,255,255,255,255,255},{0},{0}};
static int typed_accept_parameters(int,const void*,const char* const*,const int*,const int*);
"#;

const TYPED_QUERY_SUITE: &str = r#"
static int typed_accept_parameters(int count,const void* raw,const char* const* values,const int* lengths,const int* formats) {
  static const uint32_t expected[6]={23u,20u,16u,25u,25u,1043u};
  const uint32_t* types=(const uint32_t*)raw;int valid=count==6 && types && values && lengths && formats;
  for(int i=0;valid && i<6;i++)valid=types[i]==expected[i] && formats[i]==1;
  valid=valid && lengths[0]==4 && !memcmp(values[0],"\x80\0\0\0",4u)
      && lengths[1]==8 && !memcmp(values[1],"\x7f\xff\xff\xff\xff\xff\xff\xff",8u)
      && lengths[2]==1 && values[2][0]==1 && lengths[3]==14
      && !memcmp(values[3],"\xe4\xb8\xad\xe6\x96\x87\xf0\x9f\x99\x82 ' \\",14u)
      && !values[4] && !lengths[4] && values[5] && !lengths[5];
  if(!valid)bad_calls++;else typed_parameter_checks++;return valid;
}
static KuPgAsyncOp* typed_create(KuPgClient* pool,AsyncBudget* budget) {
  KuValue values[6]={ku_v_int(INT32_MIN),ku_v_int(INT64_MAX),ku_v_bool(true),ku_v_str(text(typed_source_text)),ku_v_null(),ku_v_str(text(""))};
  KuString parameters[]={text("int4"),text("int8"),text("bool"),text("text"),text("text?"),text("varchar")};
  KuString columns[]={text("int4"),text("int8"),text("bool"),text("text"),text("varchar"),text("text?"),text("int4"),text("int8"),text("bool"),text("int8?")};
  KuError error={0};KuPgAsyncOp* op=ku_pg_async_create_one_typed(pool,text("SELECT $1::int4,$2::int8,$3::bool,$4::text,$6::varchar,$5::text,2147483647::int4,9223372036854775807::int8,false,NULL::int8"),
      values,6u,(KuArray_str){6u,parameters},(KuArray_str){10u,columns},clock_ms+2000u,
      (KuPgAsyncBudget){budget,async_reserve,async_refund},async_wake,budget,&error);
  if(!op || values[0].tag!=KU_INT || values[0].as.i!=INT32_MIN || values[3].as.s.ptr!=(const uint8_t*)typed_source_text
      || op->values[3]==typed_source_text || memcmp(op->values[3],typed_source_text,14u))abort();
  return op;
}
static int typed_step(KuPgAsyncOp* op,int cancelled) {
  int done=ku_pg_async_step(op,3u,clock_ms,cancelled);
  if(!done && op->phase==KU_PG_ASYNC_CONNECT && !op->connect_ready)ku_pg_async_connect_run(op,cancelled);
  return done;
}
static int typed_values(KuPgResult* result) {
  KuResult_int a=ku_pg_value_int(result,0,0),b=ku_pg_value_int(result,0,1),c=ku_pg_value_int(result,0,6),d=ku_pg_value_int(result,0,7);
  CHECK(a.ok && a.value==INT32_MIN && b.ok && b.value==INT64_MIN && c.ok && c.value==INT32_MAX && d.ok && d.value==INT64_MAX);
  KuResult_bool yes=ku_pg_value_bool(result,0,2),no=ku_pg_value_bool(result,0,8),nil=ku_pg_is_null(result,0,5);
  CHECK(yes.ok && yes.value && no.ok && !no.value && nil.ok && nil.value);
  KuResult_str value=ku_pg_value(result,0,3),empty=ku_pg_value(result,0,4);
  CHECK(value.ok && value.value.len==14u && !memcmp(value.value.ptr,"\xe4\xb8\xad\xe6\x96\x87\xf0\x9f\x99\x82 ' \\",14u)
      && empty.ok && !empty.value.len);ku_string_drop(&value.value);ku_string_drop(&empty.value);
  value=ku_pg_value(result,0,0);CHECK(!value.ok && equals(value.error.code,"typed_value_error"));ku_error_drop(&value.error);
  a=ku_pg_value_int(result,0,9);CHECK(!a.ok && equals(a.error.code,"null_value"));ku_error_drop(&a.error);
  return 0;
}
int main(void) {
  clock_ms=1000;clock_step=0;track_runtime_allocations=1;mode_on_send=T_TYPED;reset_mode_on_send=T_RESTORE;
  KuResult_pg_client opened=ku_pg_client_open(text("hostaddr=127.0.0.1"),1,8,1000,1000,2000);CHECK(opened.ok);
  KuPgClient* pool=opened.value;AsyncBudget budget={0};KuPgAsyncOp* op=typed_create(pool,&budget);
  typed_source_text[0]='z';CHECK((uint8_t)op->values[3][0]==0xe4u);typed_source_text[0]=(char)0xe4;
  CHECK(async_finish(op,0) && op->output.ok && op->output.value->rows==1u && op->output.value->cols==10u
      && op->output.value->cell_capacity==10u && ku_pg_result_is_typed(op->output.value));
  KuResult_pg_result detached=ku_pg_async_take(op);size_t physical=0;
  CHECK(ku_pg_result_owned_bytes(detached.value,&physical) && physical==sizeof(KuPgResult)+120u+40u);
  ku_pg_async_dispose(op,1);ku_pg_client_close_owned(pool);
  CHECK(!live_connections && !live_results && budget.bytes==physical && runtime_bytes==physical && live_runtime_allocations==1);
  CHECK(!typed_values(detached.value));ku_drop_pg_result(&detached.value);async_refund(&budget,physical);
  CHECK(!budget.bytes && !runtime_bytes && !live_runtime_allocations);

  opened=ku_pg_client_open(text("hostaddr=127.0.0.1"),1,8,1000,1000,2000);CHECK(opened.ok);pool=opened.value;
  for(int i=0;i<6;i++) {
    typed_errors=1<<i;op=typed_create(pool,&budget);CHECK(async_finish(op,0) && !op->output.ok);
    CHECK(error_is(op->output,i==3?"null_value":"typed_value_error"));async_drop_output(op,&budget,0);CHECK(!budget.bytes && !live_results);
  }
  typed_errors=0;
  for(int rows=0;rows<=2;rows+=2) {
    typed_rows=rows;op=typed_create(pool,&budget);CHECK(async_finish(op,0) && error_is(op->output,"row_count_error"));
    async_drop_output(op,&budget,0);CHECK(!budget.bytes && !live_results);
  }
  typed_rows=1;mode_on_send=T_ERROR;op=typed_create(pool,&budget);
  CHECK(async_finish(op,0) && error_is(op->output,"query_error"));async_drop_output(op,&budget,0);mode_on_send=T_TYPED;

  /* Schema/range rejection precedes allocation or SQL; actual allocation sites
   * and a post-send row allocation failure retain the original all-zero ledger. */
  KuValue invalid=ku_v_int(INT64_MAX);KuString int4=text("int4");KuError error={0};int before_sends=user_sends;
  fail_runtime_allocation_after=1;
  CHECK(!ku_pg_async_create_one_typed(pool,text("SELECT $1::int4"),&invalid,1u,(KuArray_str){1u,&int4},(KuArray_str){1u,&int4},clock_ms+2000u,
      (KuPgAsyncBudget){&budget,async_reserve,async_refund},async_wake,&budget,&error)
      && fail_runtime_allocation_after==1 && user_sends==before_sends && !budget.bytes);
  fail_runtime_allocation_after=0;ku_error_drop(&error);
  /* Count the constructor's real allocation attempts, rather than freeze its
   * former four-block representation into an error-handling contract. */
  KuValue probe[1]={ku_v_int(1)};size_t calls_before=runtime_allocation_calls;
  op=ku_pg_async_create_one_typed(pool,text("SELECT $1::int4"),probe,1u,(KuArray_str){1u,&int4},(KuArray_str){1u,&int4},clock_ms+2000u,
      (KuPgAsyncBudget){&budget,async_reserve,async_refund},async_wake,&budget,&error);
  size_t input_calls=runtime_allocation_calls-calls_before;
  CHECK(op && input_calls && input_calls<=16u && user_sends==before_sends);
  CHECK(ku_pg_async_step(op,0u,clock_ms,1));async_drop_output(op,&budget,0);
  for(size_t failure=1;failure<=input_calls;failure++) {
    KuValue values[1]={ku_v_int(1)};error=(KuError){0};fail_runtime_allocation_after=failure;
    CHECK(!ku_pg_async_create_one_typed(pool,text("SELECT $1::int4"),values,1u,(KuArray_str){1u,&int4},(KuArray_str){1u,&int4},clock_ms+2000u,
        (KuPgAsyncBudget){&budget,async_reserve,async_refund},async_wake,&budget,&error)
        && !fail_runtime_allocation_after && !budget.bytes && user_sends==before_sends);
    ku_error_drop(&error);
  }
  op=typed_create(pool,&budget);fail_runtime_allocation_after=1;
  CHECK(async_finish(op,0) && error_is(op->output,"execution_unknown") && !fail_runtime_allocation_after);
  async_drop_output(op,&budget,0);

  typed_large=1;memset(typed_large_bytes,'x',131079u);typed_large_bytes[131079]=0;
  memcpy(typed_large_bytes+65535,"\xf0\x9f\x99\x82",4u);
  op=typed_create(pool,&budget);size_t turns=0;
  while((!op->query.value || !op->query.value->bytes_len) && turns++<1024u)CHECK(!typed_step(op,0));
  CHECK(op->query.value && op->query.value->bytes_len<op->query.value->bytes_capacity
      && ku_pg_async_step(op,3u,clock_ms,1) && error_is(op->output,"execution_unknown"));
  async_drop_output(op,&budget,0);typed_large=0;
  reset_mode_on_send=T_READ_TIMEOUT;op=typed_create(pool,&budget);turns=0;
  while(op->phase!=KU_PG_ASYNC_RESET_READ && turns++<1024u)CHECK(!typed_step(op,0));
  CHECK(op->output.ok && op->phase==KU_PG_ASYNC_RESET_READ);
  clock_ms=op->deadline;CHECK(ku_pg_async_step(op,0,clock_ms,0) && op->output.ok && !pool->conns[0]);
  CHECK(!typed_values(op->output.value));async_drop_output(op,&budget,1);
  ku_pg_client_close_owned(pool);
  CHECK(!budget.bytes && !live_connections && !live_results && !live_runtime_allocations && !runtime_bytes && !bad_calls && typed_parameter_checks>0);
  puts("pg typed single-row binary values and strict lifetime closed loop");return 0;
}
"#;
